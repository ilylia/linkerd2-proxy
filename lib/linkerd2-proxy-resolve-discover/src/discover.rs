use super::Backoff;
use futures::{stream::FuturesUnordered, try_ready, Async, Future, Poll, Stream};
use indexmap::IndexMap;
use linkerd2_never::Never;
use linkerd2_proxy_core::resolve::{Resolution, Resolve, Update};
use linkerd2_proxy_core::Error;
use std::net::SocketAddr;
use tokio::sync::oneshot;
use tokio::timer;
use tower::discover::Change;
use tracing::{debug, trace};

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
pub struct Discover<T, R: Resolve<T>, M: tower::Service<R::Endpoint>, B = ()> {
    state: State<R::Future, R::Resolution, R::Endpoint>,
    target: T,
    resolve: R,
    make_endpoint: M,
    make_futures: MakeFutures<M::Future>,
    pending_removals: Vec<SocketAddr>,
    active_endpoints: IndexMap<SocketAddr, R::Endpoint>,
    backoff: B,
    _marker: std::marker::PhantomData<fn(T)>,
}

enum State<F, R, E> {
    Disconnected,
    Connecting(F),
    Connected(Option<R>),
    Reconcile(Option<Update<E>>, Option<R>),
    Resolving(R),
    Failed(Option<Error>),
    Backoff(timer::Delay),
}

struct MakeFutures<F> {
    futures: FuturesUnordered<MakeFuture<F>>,
    cancelations: IndexMap<SocketAddr, oneshot::Sender<()>>,
}

struct MakeFuture<F> {
    inner: F,
    canceled: oneshot::Receiver<()>,
    addr: SocketAddr,
}

enum MakeError<E> {
    Inner(E),
    Canceled,
}

// === impl Discover ===

impl<T, R, M> Discover<T, R, M>
where
    R: Resolve<T>,
    R::Endpoint: Clone + PartialEq + std::fmt::Debug,
    M: tower::Service<R::Endpoint>,
{
    pub fn new(target: T, resolve: R, make_endpoint: M) -> Self {
        Self {
            state: State::Disconnected,
            target,
            resolve,
            make_endpoint,
            make_futures: MakeFutures::new(),
            pending_removals: Vec::new(),
            active_endpoints: IndexMap::default(),
            backoff: (),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn with_backoff<B: Backoff>(self, backoff: B) -> Discover<T, R, M, B> {
        Discover {
            backoff,
            state: self.state,
            target: self.target,
            resolve: self.resolve,
            make_endpoint: self.make_endpoint,
            make_futures: self.make_futures,
            pending_removals: self.pending_removals,
            active_endpoints: self.active_endpoints,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, R, M, B> Discover<T, R, M, B>
where
    R: Resolve<T>,
    R::Endpoint: Clone + PartialEq + std::fmt::Debug,
    M: tower::Service<R::Endpoint>,
{
    pub fn with_resolution(self, resolution: R::Resolution) -> Self {
        Self {
            state: State::Connected(Some(resolution)),
            ..self
        }
    }
}

impl<T, R, M, B> tower::discover::Discover for Discover<T, R, M, B>
where
    T: Clone + std::fmt::Debug,
    R: Resolve<T>,
    R::Endpoint: Clone + PartialEq + std::fmt::Debug,
    R::Error: Into<Error>,
    M: tower::Service<R::Endpoint>,
    M::Error: Into<Error>,
    B: Backoff,
{
    type Key = SocketAddr;
    type Service = M::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::Error> {
        if let Async::Ready(addr) = self.poll_removals()? {
            return Ok(Async::Ready(Change::Remove(addr)));
        }

        if let Async::Ready(Some((addr, svc))) = self.make_futures.poll().map_err(Into::into)? {
            return Ok(Async::Ready(Change::Insert(addr, svc)));
        }

        Ok(Async::NotReady)
    }
}

impl<T, R, M, B> Discover<T, R, M, B>
where
    T: Clone + std::fmt::Debug,
    R: Resolve<T>,
    R::Endpoint: Clone + PartialEq + std::fmt::Debug,
    R::Error: Into<Error>,
    M: tower::Service<R::Endpoint>,
    M::Error: Into<Error>,
    B: Backoff,
{
    fn poll_resolution(&mut self) -> Poll<Update<R::Endpoint>, Never> {
        loop {
            self.state = match self.state {
                State::Disconnected => {
                    let fut = self.resolve.resolve(self.target.clone());
                    State::Connecting(fut)
                }
                State::Connecting(ref mut fut) => match fut.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(e) => State::Failed(Some(e.into())),
                    Ok(Async::Ready(resolution)) => State::Connected(Some(resolution)),
                },
                State::Connected(ref mut resolution) => {
                    // If there is a prior state -- endpoints that have already been created, for instance,
                    // Get the first update after reconnecting, reconcile
                    // against the prior state, and then continue resolving...
                    if self.active_endpoints.is_empty() {
                        self.backoff.reset_delay();
                        State::Resolving(resolution.take().expect("illegal state"))
                    } else {
                        match resolution.as_mut().expect("illegal state").poll() {
                            Ok(Async::NotReady) => return Ok(Async::NotReady),
                            Err(e) => State::Failed(Some(e.into())),
                            Ok(Async::Ready(update)) => {
                                State::Reconcile(Some(update), resolution.take())
                            }
                        }
                    }
                }
                State::Reconcile(ref mut update, ref mut resolution) => match update.take() {
                    Some(Update::Add(endpoints)) => {
                        if self.active_endpoints.is_empty() {
                            return Ok(Update::Add(endpoints).into());
                        }

                        for (addr, _) in endpoints.iter() {
                            self.active_endpoints.remove(addr);
                        }

                        // The next time through, there will be no
                        // active_endpoints so the added endpoints will be
                        // returned immediately.
                        let rm_addrs = self.active_endpoints.drain(..).map(|(addr, _)| addr);
                        *update = Some(Update::Add(endpoints));

                        return Ok(Update::Remove(rm_addrs.collect()).into());
                    }
                    Some(Update::Remove(mut addrs)) => {
                        addrs.extend(self.active_endpoints.drain(..).map(|(addr, _)| addr));
                        return Ok(Update::Remove(addrs).into());
                    }
                    Some(dne) => {
                        if self.active_endpoints.is_empty() {
                            return Ok(dne.into());
                        }

                        let addrs = self.active_endpoints.drain(..).map(|(addr, _)| addr);
                        *update = Some(dne);
                        return Ok(Update::Remove(addrs.collect()).into());
                    }
                    None => {
                        self.backoff.reset_delay();
                        State::Resolving(resolution.take().expect("illegal state"))
                    }
                },

                State::Resolving(ref mut resolution) => match resolution.poll() {
                    Ok(ready) => return Ok(ready),
                    Err(e) => State::Failed(Some(e.into())),
                },
                State::Failed(ref mut e) => {
                    let err = e.take().expect("illegal staet");
                    debug!(message = %err, target = ?self.target);
                    State::Backoff(self.backoff.next_delay())
                }
                State::Backoff(ref mut fut) => match fut.poll().expect("timer must not fail") {
                    Async::NotReady => return Ok(Async::NotReady),
                    Async::Ready(()) => State::Disconnected,
                },
            };
        }
    }

    fn poll_removals(&mut self) -> Poll<SocketAddr, Error> {
        loop {
            if let Some(addr) = self.pending_removals.pop() {
                self.make_futures.remove(&addr);
                return Ok(addr.into());
            }

            // Before polling the resolution, where we could potentially receive
            // an `Add`, poll_ready to ensure that `make` is ready to build new
            // services. Don't process any updates until we can do so.
            try_ready!(self.make_endpoint.poll_ready().map_err(Into::into));

            let update = try_ready!(self.poll_resolution());
            trace!("watch: {:?}", update);
            match update {
                Update::Add(additions) => {
                    for (addr, target) in additions.into_iter() {
                        // Start building the service and continue. If a pending
                        // service exists for this addr, it will be canceled.
                        let fut = self.make_endpoint.call(target);
                        self.make_futures.push(addr, fut);
                    }
                }
                Update::Remove(removals) => {
                    self.pending_removals.extend(removals);
                }

                Update::Empty | Update::DoesNotExist => unimplemented!(),
            }
        }
    }
}

// === impl MakeFutures ===

impl<F: Future> MakeFutures<F> {
    fn new() -> Self {
        Self {
            futures: FuturesUnordered::new(),
            cancelations: IndexMap::new(),
        }
    }

    fn push(&mut self, addr: SocketAddr, inner: F) {
        let (cancel, canceled) = oneshot::channel();
        if let Some(prior) = self.cancelations.insert(addr, cancel) {
            let _ = prior.send(());
        }
        self.futures.push(MakeFuture {
            addr,
            inner,
            canceled,
        });
    }

    fn remove(&mut self, addr: &SocketAddr) {
        if let Some(cancel) = self.cancelations.remove(addr) {
            let _ = cancel.send(());
        }
    }
}

impl<F: Future> Stream for MakeFutures<F> {
    type Item = (SocketAddr, F::Item);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            return match self.futures.poll() {
                Err(MakeError::Canceled) => continue,
                Err(MakeError::Inner(err)) => Err(err),
                Ok(Async::Ready(Some((addr, svc)))) => {
                    let _rm = self.cancelations.remove(&addr);
                    debug_assert!(_rm.is_some(), "cancelation missing for {}", addr);
                    Ok(Async::Ready(Some((addr, svc))))
                }
                Ok(r) => Ok(r),
            };
        }
    }
}

// === impl MakeFuture ===

impl<F: Future> Future for MakeFuture<F> {
    type Item = (SocketAddr, F::Item);
    type Error = MakeError<F::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Ok(Async::Ready(())) = self.canceled.poll() {
            trace!("canceled making service for {:?}", self.addr);
            return Err(MakeError::Canceled);
        }
        let svc = try_ready!(self.inner.poll());
        Ok((self.addr, svc).into())
    }
}

// === impl MakeError ===

impl<E> From<E> for MakeError<E> {
    fn from(inner: E) -> Self {
        MakeError::Inner(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use linkerd2_never::Never;
    use tokio::sync::mpsc;
    use tower::discover::{Change, Discover as _Discover};
    use tower::Service;
    use tower_util::service_fn;

    #[derive(Debug)]
    struct Svc<F>(Vec<F>);
    impl<F: Future> Service<()> for Svc<F> {
        type Response = F::Item;
        type Error = F::Error;
        type Future = F;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(().into())
        }

        fn call(&mut self, _: ()) -> Self::Future {
            self.0.pop().expect("exhausted")
        }
    }

    #[test]
    fn inserts_delivered_out_of_order() {
        with_task(move || {
            let (mut reso_tx, reso_rx) = mpsc::channel(2);
            let (make0_tx, make0_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();
            let (make1_tx, make1_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();

            let mut discover = Discover::new(
                (),
                Svc(vec![future::ok::<_, Never>(reso_rx)]),
                Svc(vec![make1_rx, make0_rx]),
            );
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without updates"
            );

            let addr0 = SocketAddr::from(([127, 0, 0, 1], 80));
            reso_tx.try_send(Update::Add(vec![(addr0, ())])).unwrap();
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.futures.len(),
                1,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            let addr1 = SocketAddr::from(([127, 0, 0, 2], 80));
            reso_tx
                .try_send(Update::Add(vec![(addr1, ())]))
                .expect("update must be sent");
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.futures.len(),
                2,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                2,
                "no pending cancelation"
            );

            let (rsp1_tx, rsp1_rx) = oneshot::channel();
            make1_tx
                .send(Svc(vec![rsp1_rx]))
                .expect("make must receive service");
            match discover.poll().expect("discover can't fail") {
                Async::NotReady => panic!("not processed"),
                Async::Ready(Change::Remove(..)) => panic!("unexpected remove"),
                Async::Ready(Change::Insert(a, mut svc)) => {
                    assert_eq!(a, addr1);

                    assert!(svc.poll_ready().unwrap().is_ready());
                    let mut fut = svc.call(());
                    assert!(fut.poll().unwrap().is_not_ready());
                    rsp1_tx.send(1).unwrap();
                    assert_eq!(fut.poll().unwrap(), Async::Ready(1));
                }
            }
            assert_eq!(
                discover.make_futures.futures.len(),
                1,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            let (rsp0_tx, rsp0_rx) = oneshot::channel();
            make0_tx
                .send(Svc(vec![rsp0_rx]))
                .expect("make must receive service");
            match discover.poll().expect("discover can't fail") {
                Async::NotReady => panic!("not processed"),
                Async::Ready(Change::Remove(..)) => panic!("unexpected remove"),
                Async::Ready(Change::Insert(a, mut svc)) => {
                    assert_eq!(a, addr0);

                    assert!(svc.poll_ready().unwrap().is_ready());
                    let mut fut = svc.call(());
                    assert!(fut.poll().unwrap().is_not_ready());
                    rsp0_tx.send(0).unwrap();
                    assert_eq!(fut.poll().unwrap(), Async::Ready(0));
                }
            }
            assert!(discover.make_futures.futures.is_empty(), "futures remain");
            assert!(
                discover.make_futures.cancelations.is_empty(),
                "cancelation remains"
            );
        });
    }

    #[test]
    fn overwriting_insert_cancels_original() {
        with_task(move || {
            let (mut reso_tx, reso_rx) = mpsc::channel(2);
            let (make0_tx, make0_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();
            let (make1_tx, make1_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();

            let mut discover = Discover::new(
                (),
                Svc(vec![future::ok::<_, Never>(reso_rx)]),
                Svc(vec![make1_rx, make0_rx]),
            );
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without updates"
            );

            let addr = SocketAddr::from(([127, 0, 0, 1], 80));
            reso_tx.try_send(Update::Add(vec![(addr, ())])).unwrap();
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.futures.len(),
                1,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            reso_tx
                .try_send(Update::Add(vec![(addr, ())]))
                .expect("update must be sent");
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.futures.len(),
                1,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            make0_tx
                .send(Svc(vec![]))
                .expect_err("receiver must have been dropped");

            let (rsp1_tx, rsp1_rx) = oneshot::channel();
            make1_tx
                .send(Svc(vec![rsp1_rx]))
                .expect("make must receive service");
            match discover.poll().expect("discover can't fail") {
                Async::NotReady => panic!("not processed"),
                Async::Ready(Change::Remove(..)) => panic!("unexpected remove"),
                Async::Ready(Change::Insert(a, mut svc)) => {
                    assert_eq!(a, addr);

                    assert!(svc.poll_ready().unwrap().is_ready());
                    let mut fut = svc.call(());
                    assert!(fut.poll().unwrap().is_not_ready());
                    rsp1_tx.send(1).unwrap();
                    assert_eq!(fut.poll().unwrap(), Async::Ready(1));
                }
            }
            assert!(
                discover.make_futures.cancelations.is_empty(),
                "cancelation remains"
            );
        });
    }

    #[test]
    fn cancelation_of_pending_service() {
        with_task(move || {
            let (mut tx, reso_rx) = mpsc::channel(1);

            let mut discover = Discover::new(
                (),
                Svc(vec![future::ok::<_, Never>(reso_rx)]),
                service_fn(|()| future::empty::<Svc<()>, Error>()),
            );
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without updates"
            );

            let addr = SocketAddr::from(([127, 0, 0, 1], 80));
            tx.try_send(Update::Add(vec![(addr, ())])).unwrap();
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            tx.try_send(Update::Remove(vec![addr])).unwrap();
            match discover.poll().expect("discover can't fail") {
                Async::NotReady => panic!("remove not processed"),
                Async::Ready(Change::Insert(..)) => panic!("unexpected insert"),
                Async::Ready(Change::Remove(a)) => assert_eq!(a, addr),
            }
            assert!(
                discover.make_futures.cancelations.is_empty(),
                "cancelation remains"
            );
        });
    }

    fn with_task<F: FnOnce() -> U, U>(f: F) -> U {
        future::lazy(|| Ok::<_, ()>(f())).wait().unwrap()
    }
}

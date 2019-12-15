use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;
use linkerd2_stack::Proxy;
use linkerd2_timeout::{error, Timeout};
use std::time::Duration;
use tokio::timer;
use tracing::{debug, error};

/// Implement on targets to determine if a service has a timeout.
pub trait HasTimeout {
    fn timeout(&self) -> Option<Duration>;
}

/// An HTTP-specific optional timeout layer.
///
/// The stack target must implement `HasTimeout`, and if a duration is
/// specified for the target, a timeout is applied waiting for HTTP responses.
///
/// Timeout errors are translated into `http::Response`s with appropiate
/// status codes.
pub fn layer() -> Layer {
    Layer
}

#[derive(Clone, Debug)]
pub struct Layer;

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
}

pub struct MakeFuture<F> {
    inner: F,
    timeout: Option<Duration>,
}

#[derive(Clone, Debug)]
pub struct Service<S>(Timeout<S>);

/// A marker set in `http::Response::extensions` that *this* process triggered
/// the request timeout.
#[derive(Debug)]
pub struct ProxyTimedOut(());

impl<M> tower::layer::Layer<M> for Layer {
    type Service = Stack<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack { inner }
    }
}

impl<T, M> tower::Service<T> for Stack<M>
where
    M: tower::Service<T>,
    T: HasTimeout,
{
    type Response = tower::util::Either<Service<M::Response>, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let timeout = target.timeout();
        let inner = self.inner.call(target);

        MakeFuture { inner, timeout }
    }
}

impl<F: Future> Future for MakeFuture<F> {
    type Item = tower::util::Either<Service<F::Item>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());

        let svc = if let Some(timeout) = self.timeout {
            tower::util::Either::A(Service(Timeout::new(inner, timeout)))
        } else {
            tower::util::Either::B(inner)
        };
        Ok(svc.into())
    }
}

impl<P, S, A, B> Proxy<http::Request<A>, S> for Service<P>
where
    P: Proxy<http::Request<A>, S, Response = http::Response<B>>,
    S: tower::Service<P::Request>,
    B: Default,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = Error;
    type Future = ResponseFuture<P::Future, B>;

    fn proxy(&self, svc: &mut S, req: http::Request<A>) -> Self::Future {
        ResponseFuture(self.0.proxy(svc, req), std::marker::PhantomData)
    }
}

impl<S, A, B> tower::Service<http::Request<A>> for Service<S>
where
    S: tower::Service<http::Request<A>, Response = http::Response<B>>,
    S::Error: Into<Error>,
    B: Default,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        ResponseFuture(self.0.call(req), std::marker::PhantomData)
    }
}

pub struct ResponseFuture<F, B>(
    Timeout<timer::Timeout<F>>,
    std::marker::PhantomData<fn() -> B>,
);

impl<F, B> Future for ResponseFuture<F, B>
where
    B: Default,
    F: Future<Item = http::Response<B>>,
    F::Error: Into<Error>,
{
    type Item = http::Response<B>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll().or_else(|err| {
            if let Some(err) = err.downcast_ref::<error::Timedout>() {
                debug!("request timed out after {:?}", err.duration());
                let mut res = http::Response::default();
                *res.status_mut() = http::StatusCode::GATEWAY_TIMEOUT;
                res.extensions_mut().insert(ProxyTimedOut(()));
                return Ok(res.into());
            } else if let Some(err) = err.downcast_ref::<error::Timer>() {
                // These are unexpected, and mean the runtime is in a bad place.
                error!("unexpected runtime timer error: {}", err);
                let mut res = http::Response::default();
                *res.status_mut() = http::StatusCode::BAD_GATEWAY;
                return Ok(res.into());
            }

            // else
            Err(err)
        })
    }
}
use futures::{try_ready, Future, Poll};
use http;
use linkerd2_error::Error;
use linkerd2_stack as svc;

/// Determines how a request's response should be classified.
pub trait Classify {
    type Class;
    type ClassifyEos: ClassifyEos<Class = Self::Class>;

    /// Classifies responses.
    ///
    /// Instances are intended to be used as an `http::Extension` that may be
    /// cloned to inner stack layers. Cloned instances are **not** intended to
    /// share state. Each clone should maintain its own internal state.
    type ClassifyResponse: ClassifyResponse<Class = Self::Class, ClassifyEos = Self::ClassifyEos>
        + Clone
        + Send
        + Sync
        + 'static;

    fn classify<B>(&self, req: &http::Request<B>) -> Self::ClassifyResponse;
}

/// Classifies a single response.
pub trait ClassifyResponse {
    /// A response classification.
    type Class;
    type ClassifyEos: ClassifyEos<Class = Self::Class>;

    /// Produce a stream classifier for this response.
    fn start<B>(self, headers: &http::Response<B>) -> Self::ClassifyEos;

    /// Classifies the given error.
    fn error(self, error: &Error) -> Self::Class;
}

pub trait ClassifyEos {
    type Class;

    /// Update the classifier with an EOS.
    ///
    /// Because trailers indicate an EOS, a classification must be returned.
    fn eos(self, trailers: Option<&http::HeaderMap>) -> Self::Class;

    /// Update the classifier with an underlying error.
    ///
    /// Because errors indicate an end-of-stream, a classification must be
    /// returned.
    fn error(self, error: &Error) -> Self::Class;
}

// Used for stack targets that can produce a `Classify` implementation.
pub trait CanClassify {
    type Classify: Classify;

    fn classify(&self) -> Self::Classify;
}

#[derive(Debug, Clone)]
pub struct Layer();

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
}

pub struct MakeFuture<C, F> {
    classify: Option<C>,
    inner: F,
}

#[derive(Clone, Debug)]
pub struct Proxy<C, P> {
    classify: C,
    inner: P,
}

pub fn layer() -> Layer {
    Layer()
}

impl<M> tower::layer::Layer<M> for Layer {
    type Service = Stack<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack { inner }
    }
}

impl<T, M> tower::Service<T> for Stack<M>
where
    T: CanClassify,
    M: tower::Service<T>,
{
    type Response = Proxy<T::Classify, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<T::Classify, M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let classify = Some(target.classify());
        let inner = self.inner.call(target);
        MakeFuture { classify, inner }
    }
}

impl<T, M> svc::Make<T> for Stack<M>
where
    T: CanClassify,
    M: svc::Make<T>,
{
    type Service = Proxy<T::Classify, M::Service>;

    fn make(&self, target: T) -> Self::Service {
        let classify = target.classify();
        let inner = self.inner.make(target);
        Proxy { classify, inner }
    }
}

impl<C, F: Future> Future for MakeFuture<C, F> {
    type Item = Proxy<C, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let classify = self.classify.take().expect("polled more than once");
        Ok(Proxy { classify, inner }.into())
    }
}

impl<C, P, S, B> svc::Proxy<http::Request<B>, S> for Proxy<C, P>
where
    C: Classify,
    P: svc::Proxy<http::Request<B>, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, svc: &mut S, mut req: http::Request<B>) -> Self::Future {
        let classify_rsp = self.classify.classify(&req);
        let _ = req.extensions_mut().insert(classify_rsp);

        self.inner.proxy(svc, req)
    }
}
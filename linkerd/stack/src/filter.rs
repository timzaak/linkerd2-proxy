//! A `Service` middleware that applies arbitrary-user provided logic to each
//! target before it is issued to an inner service.

pub use tower::filter::{Filter, FilterLayer, Predicate};

impl<T, P, S> super::NewService<T> for Filter<S, P>
where
    P: Predicate<T>,
    S: super::NewService<P::Request>,
{
    type Service = super::ResultService<S::Service>;

    fn new_service(&mut self, request: T) -> Self::Service {
        self.check(request)
            .map(move |req| super::ResultService::ok(self.get_mut().new_service(req)))
            .unwrap_or_else(super::ResultService::err)
    }
}

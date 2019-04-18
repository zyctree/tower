#![deny(rust_2018_idioms)]

pub mod future;

use crate::future::ResponseFuture;
use futures::{try_ready, Future, Poll};
use std::sync::{Arc, Weak};
use tokio_sync::semaphore::{Permit, Semaphore};
use tower_layer::Layer;
use tower_service::Service;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// `ServiceLimit` tracks the current number
#[derive(Debug, Clone)]
struct ServiceLimit {
    semaphore: Arc<Semaphore>,
}

pub enum Classification {
    Success,
    Failure,
}

/// A policy to classify whether a response is successful.
pub trait Policy<T, E>: Sized {
    /// Check whether a given response was successful or not.
    ///
    /// While the presence of an `&E` would likely indicate that
    /// the request was _not_ successful, the presence of a `&Res`
    /// might _also_ indicate that a request was not successful.
    /// A common example would be an HTTP response with a
    /// `429 Too Many Requests` or a `500 Internal Server Error` status code.
    fn classify(&self, result: &Result<T, E>) -> Classification;
}

pub struct PolicyFn<F> {
    f: F,
}

pub fn policy_fn<F, T, E>(f: F) -> PolicyFn<F>
where
    F: Fn(&Result<T, E>) -> Classification,
{
    PolicyFn { f }
}

impl<F, T, E> Policy<T, E> for PolicyFn<F>
where
    F: Fn(&Result<T, E>) -> Classification,
{
    fn classify(&self, result: &Result<T, E>) -> Classification {
        (self.f)(result)
    }
}

pub struct QualityOfServiceLayer<P, S> {
    policy: P,
    service: S,
}

impl<P, S> QualityOfServiceLayer<P, S> {
    pub fn new(policy: P, service: S) -> Self {
        QualityOfServiceLayer { policy, service }
    }
}

impl<P, S, Request> Layer<S, Request> for QualityOfServiceLayer<P, S>
where
    S: Service<Request>,
    S::Future: Send + 'static,
    S::Error: Into<Error> + 'static,
    P: Policy<S::Response, S::Error> + Send + Clone + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type LayerError = std::convert::Infallible;
    type Service = QualityOfService<P, S>;

    fn layer(&self, service: S) -> Result<Self::Service, Self::LayerError> {
        Ok(QualityOfService::new(self.policy.clone(), service))
    }
}

#[derive(Debug)]
struct Limit {
    semaphore: Arc<Semaphore>,
    permit: Permit,
}

#[derive(Debug)]
/// Lease is a view of the limit, containing a Weak pointer
/// to the owning semaphore and
struct Lease {}

impl Limit {
    fn new(permit: Permit, semaphore: Arc<Semaphore>) -> Self {
        Self { permit, semaphore }
    }
}

/// At a high level, `QualityOfService` is a [control loop](https://en.wikipedia.org/wiki/Control_loop)
/// that dynamically adjusts the number of concurrent, in-flight requests depending on the health of
/// a downstream service. This is accomplished using [Policy](trait.Policy.html)
/// to inspect and classify responses.
#[derive(Debug)]
pub struct QualityOfService<P, S> {
    policy: P,
    service: S,
    limit: Limit,
}

impl<P, S> QualityOfService<P, S> {
    pub fn new(policy: P, service: S) -> Self {
        let limit = Limit::new(Permit::new(), Arc::new(Semaphore::new(1024)));
        QualityOfService {
            policy,
            service,
            limit,
        }
    }
}

impl<P, S, Request> Service<Request> for QualityOfService<P, S>
where
    S: Service<Request>,
    S::Future: Send + 'static,
    S::Error: Into<Error> + 'static,
    P: Policy<S::Response, S::Error> + Send + Clone + 'static,
{
    type Response = S::Response;
    type Error = Error;
    //type Future = ResponseFuture<S::Future, P>;
    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error> + Send + 'static>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        try_ready!(self
            .limit
            .permit
            .poll_acquire(&self.limit.semaphore)
            .map_err(Error::from));

        self.service.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        if self
            .limit
            .permit
            .try_acquire(&self.limit.semaphore)
            .is_err()
        {
            panic!("max requests in-flight; poll_ready must be called first");
        }
        let fut = self.service.call(request);

        // Forget the permit, the permit will be returned when
        // `future::ResponseFuture` is dropped.
        self.limit.permit.forget();

        let fut = ResponseFuture::new(
            fut,
            self.policy.clone(),
            Arc::downgrade(&self.limit.semaphore),
        )
        .map_err(Into::into);

        Box::new(fut)
    }
}

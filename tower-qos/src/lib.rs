#![deny(rust_2018_idioms)]

pub mod future;

use crate::future::ResponseFuture;
use futures::Poll;
use std::sync::{Arc, atomic::AtomicUsize};
use tokio_sync::semaphore::{self, Semaphore};
use tower_layer::Layer;
use tower_service::Service;
use arc_swap::ArcSwap;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// `ServiceLimit` tracks the current number
#[derive(Debug, Clone)]
struct ServiceLimit {
    semaphore: Arc<Semaphore>,
    permit: semaphore::Permit,
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
    S: Service<Request> + Clone,
    <S as Service<Request>>::Error: Into<Error>,
    P: Policy<S::Response, S::Error> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type LayerError = std::convert::Infallible;
    type Service = QualityOfService<P, S>;

    fn layer(&self, service: S) -> Result<Self::Service, Self::LayerError> {
        Ok(QualityOfService::new(self.policy.clone(), service))
    }
}

/// At a high level, `QualityOfService` is a [control loop](https://en.wikipedia.org/wiki/Control_loop)
/// that dynamically adjusts the number of concurrent, in-flight requests depending on the health of
/// a downstream service. This is accomplished using [Policy](trait.Policy.html)
/// to inspect and classify responses.
#[derive(Clone, Debug)]
pub struct QualityOfService<P, S> {
    policy: P,
    service: S,
    semaphore: Arc<Semaphore>,
}

impl<P, S> QualityOfService<P, S> {
    pub fn new(policy: P, service: S) -> Self {
        let semaphore = Arc::new(Semaphore::new(1024));
        QualityOfService {
            policy,
            service,
            semaphore,
        }
    }
}

impl<P, S, Request> Service<Request> for QualityOfService<P, S>
where
    P: Policy<S::Response, S::Error> + Clone,
    S: Service<Request> + Clone,
    <S as Service<Request>>::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future, P>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let fut = self.service.call(request);
        ResponseFuture::new(fut, self.policy.clone(), self.semaphore.clone())
    }
}

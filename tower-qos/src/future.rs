use futures::{Async, Future, Poll};
use std::sync::Arc;
use tokio_sync::semaphore::Semaphore;

use crate::{Classification, Error, Policy};

pub struct ResponseFuture<F, P> {
    inner: F,
    policy: P,
    semaphore: Arc<Semaphore>,
}

impl<F, P> ResponseFuture<F, P> {
    pub(crate) fn new(inner: F, policy: P, semaphore: Arc<Semaphore>) -> Self {
        ResponseFuture {
            inner,
            policy,
            semaphore,
        }
    }
}

impl<F, P> Future for ResponseFuture<F, P>
where
    F: Future,
    F::Error: Into<Error>,
    P: Policy<F::Item, F::Error>,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let result = match self.inner.poll() {
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(res)) => Ok(res),
                Err(err) => Err(err),
            };

            match self.policy.classify(&result) {
                Classification::Success => self.semaphore.add_permits(1),
                Classification::Failure => unimplemented!(),
            }

            return result.map(Async::Ready);
        }
    }
}

use futures::{Async, Future, Poll};
use std::sync::{Arc, Weak};
use tokio_sync::semaphore::Semaphore;

use crate::{Classification, Error, Policy};

pub struct ResponseFuture<F, P> {
    inner: F,
    policy: P,
    semaphore: Weak<Semaphore>,
}

impl<F, P> ResponseFuture<F, P> {
    pub(crate) fn new(inner: F, policy: P, semaphore: Weak<Semaphore>) -> Self {
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
    type Error = Error;
    // this used to be F::Error
    // but since F::Error -> Into<Error>
    // and the Service wants Error
    // you want this error type to be
    // Error and in side this poll fn .map_err(Into::into)
    //

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let result = match self.inner.poll().map_err(Into::into)? {
            Async::NotReady => return Ok(Async::NotReady),
            Async::Ready(res) => Ok(res),
        };

        match self.policy.classify(&result) {
            Classification::Success => {
                // the original semaphore was _not_ changed while this
                // future was resolving
                if let Some(semaphore) = self.semaphore.upgrade() {
                    semaphore.add_permits(1)
                }
            }
            Classification::Failure => {
                if let Some(mut semaphore) = self.semaphore.upgrade() {
                    let new_limit = semaphore.available_permits() as f64 * 0.8;
                    let new_semaphore = Semaphore::new(new_limit as usize);
                    std::mem::replace(&mut semaphore, Arc::new(new_semaphore));
                }
            }
        }

        return result.map(Async::Ready).map_err(Into::into);
    }
}

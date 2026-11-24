//! A one-way cooperative cancellation flag shared between the scheduler and a task.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// A cheaply-clonable cancellation flag. Once cancelled it stays cancelled.
#[derive(Clone, Debug, Default)]
pub(crate) struct CancelToken {
    inner: Arc<Inner>,
}

impl CancelToken {
    /// Create a token that is not cancelled.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Cancel the token and wake every task waiting in [`cancelled`](Self::cancelled).
    pub(crate) fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Whether the token has been cancelled.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Resolve once the token is cancelled (immediately if it already is).
    pub(crate) async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            // Register as a waiter before re-checking the flag, so a `cancel` that lands
            // between the check and the await still wakes us.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_cancelled_resolves_after_cancel() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        let waiter = {
            let token = token.clone();
            tokio::spawn(async move { token.cancelled().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter was not woken")
            .unwrap();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancelled_resolves_immediately_when_already_cancelled() {
        let token = CancelToken::new();
        token.cancel();
        tokio::time::timeout(Duration::from_millis(100), token.cancelled())
            .await
            .expect("an already-cancelled token must resolve at once");
    }
}

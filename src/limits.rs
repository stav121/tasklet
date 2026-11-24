//! Limits on how many task runs may execute at the same time, scheduler-wide and per
//! named pool.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The concurrency limits of one scheduler, shared with every task it runs.
///
/// Tasks look the semaphores up at the start of each run, so limits configured after a
/// task was added still apply to its later runs.
#[derive(Debug, Default)]
pub(crate) struct Limiter {
    global: RwLock<Option<Arc<Semaphore>>>,
    pools: RwLock<HashMap<String, Arc<Semaphore>>>,
}

/// The permits held for the duration of one run. Dropping it releases them.
#[derive(Debug)]
pub(crate) struct RunPermits {
    _pool: Option<OwnedSemaphorePermit>,
    _global: Option<OwnedSemaphorePermit>,
}

impl Limiter {
    /// Limit the number of runs executing at once across the scheduler. Values below 1
    /// are treated as 1.
    pub(crate) fn set_global(&self, max: usize) {
        *self.global.write().unwrap() = Some(Arc::new(Semaphore::new(max.max(1))));
    }

    /// Create or replace the named pool. Values below 1 are treated as 1.
    pub(crate) fn set_pool(&self, name: &str, max: usize) {
        self.pools
            .write()
            .unwrap()
            .insert(name.to_string(), Arc::new(Semaphore::new(max.max(1))));
    }

    /// Whether a pool with the given name exists.
    pub(crate) fn has_pool(&self, name: &str) -> bool {
        self.pools.read().unwrap().contains_key(name)
    }

    /// Take the permits a run in `pool` needs if they are free right now. Returns
    /// `None`, holding nothing, if either would have to be waited for.
    pub(crate) fn try_acquire(&self, pool: Option<&str>) -> Option<RunPermits> {
        let pool_sem = pool.and_then(|p| self.pools.read().unwrap().get(p).cloned());
        let pool_permit = match pool_sem {
            Some(sem) => Some(sem.try_acquire_owned().ok()?),
            None => None,
        };
        let global_sem = self.global.read().unwrap().clone();
        let global_permit = match global_sem {
            Some(sem) => Some(sem.try_acquire_owned().ok()?),
            None => None,
        };
        Some(RunPermits {
            _pool: pool_permit,
            _global: global_permit,
        })
    }

    /// Wait for the permits a run in `pool` needs.
    ///
    /// The pool permit is always taken before the global one. Because every run uses
    /// the same order, two runs can never each hold a permit the other is waiting for.
    pub(crate) async fn acquire(&self, pool: Option<&str>) -> RunPermits {
        let pool_sem = pool.and_then(|p| self.pools.read().unwrap().get(p).cloned());
        let pool_permit = match pool_sem {
            // The semaphores are never closed, so `acquire_owned` cannot fail.
            Some(sem) => sem.acquire_owned().await.ok(),
            None => None,
        };
        let global_sem = self.global.read().unwrap().clone();
        let global_permit = match global_sem {
            Some(sem) => sem.acquire_owned().await.ok(),
            None => None,
        };
        RunPermits {
            _pool: pool_permit,
            _global: global_permit,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_unlimited_acquire_is_immediate() {
        let limiter = Limiter::default();
        tokio::time::timeout(Duration::from_millis(50), limiter.acquire(None))
            .await
            .expect("no limit configured, acquire must not wait");
    }

    #[tokio::test]
    async fn test_global_limit_blocks_until_released() {
        let limiter = Arc::new(Limiter::default());
        limiter.set_global(1);
        let first = limiter.acquire(None).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), limiter.acquire(None))
                .await
                .is_err(),
            "second acquire must wait while the first permit is held"
        );
        drop(first);
        tokio::time::timeout(Duration::from_millis(50), limiter.acquire(None))
            .await
            .expect("acquire must succeed once the permit is released");
    }

    #[tokio::test]
    async fn test_pool_limit_is_independent_of_other_pools() {
        let limiter = Limiter::default();
        limiter.set_pool("db", 1);
        limiter.set_pool("http", 1);
        assert!(limiter.has_pool("db"));
        assert!(!limiter.has_pool("missing"));
        let _db = limiter.acquire(Some("db")).await;
        tokio::time::timeout(Duration::from_millis(50), limiter.acquire(Some("http")))
            .await
            .expect("a different pool must not be blocked");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), limiter.acquire(Some("db")))
                .await
                .is_err(),
            "the same pool must be limited"
        );
    }

    #[test]
    fn test_zero_limit_is_treated_as_one() {
        let limiter = Limiter::default();
        limiter.set_global(0);
        let sem = limiter.global.read().unwrap().clone().unwrap();
        assert_eq!(sem.available_permits(), 1);
    }
}

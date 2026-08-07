//! Retry policies for task steps.
//!
//! A [`RetryPolicy`] controls how many times a failing step is re-attempted and how
//! long to wait between attempts. Only steps returning
//! [`TaskStepStatusErr::Error`](crate::task::TaskStepStatusErr::Error) (or timing out)
//! are retried; a step returning
//! [`TaskStepStatusErr::ErrorDelete`](crate::task::TaskStepStatusErr::ErrorDelete)
//! bypasses retries and removes the task immediately.

use std::time::Duration;

/// Randomized spread applied on top of the computed backoff delay.
///
/// Jitter avoids a "thundering herd" where many tasks that failed at the same
/// instant all retry at exactly the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Jitter {
    /// No jitter; use the computed delay exactly.
    #[default]
    None,
    /// Pick a delay uniformly in `[0, computed]`.
    Full,
    /// Pick a delay uniformly in `[computed / 2, computed]`.
    Equal,
}

impl Jitter {
    /// Apply the jitter strategy to a computed backoff `delay`.
    fn apply(self, delay: Duration) -> Duration {
        match self {
            Jitter::None => delay,
            Jitter::Full => {
                let nanos = clamp_nanos(delay);
                Duration::from_nanos(fastrand::u64(0..=nanos))
            }
            Jitter::Equal => {
                let half = clamp_nanos(delay) / 2;
                Duration::from_nanos(half + fastrand::u64(0..=half))
            }
        }
    }
}

/// Clamp a duration to a `u64` nanosecond count (durations this large never occur
/// in practice, but this keeps the conversion total).
fn clamp_nanos(delay: Duration) -> u64 {
    delay.as_nanos().min(u64::MAX as u128) as u64
}

/// The delay strategy applied between retry attempts.
#[derive(Debug, Clone, PartialEq)]
pub enum Backoff {
    /// Wait the same fixed duration before every retry.
    Fixed(Duration),
    /// Wait `base * factor^n` before retry `n` (0-indexed), optionally capped at `max`.
    Exponential {
        /// The delay before the first retry.
        base: Duration,
        /// The multiplier applied for each subsequent retry.
        factor: u32,
        /// An optional upper bound on the computed delay.
        max: Option<Duration>,
    },
}

/// A policy describing how a failing step should be retried.
///
/// # Examples
///
/// ```
/// # use std::time::Duration;
/// # use tasklet::RetryPolicy;
/// // Retry up to 3 times, waiting 100ms between attempts.
/// let _ = RetryPolicy::fixed(3, Duration::from_millis(100));
///
/// // Retry up to 5 times with exponential backoff (200ms, 400ms, 800ms, ...),
/// // capped at 2 seconds.
/// let _ = RetryPolicy::exponential(5, Duration::from_millis(200), 2)
///     .with_max_delay(Duration::from_secs(2));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// The maximum number of *additional* attempts after the initial one.
    pub max_retries: usize,
    /// The backoff strategy applied between attempts.
    pub backoff: Backoff,
    /// The randomized spread applied on top of the computed delay.
    pub jitter: Jitter,
}

impl RetryPolicy {
    /// Create a policy that retries with a constant delay between attempts.
    ///
    /// # Arguments
    ///
    /// * max_retries - the maximum number of retries after the first attempt.
    /// * delay       - the fixed delay to wait before each retry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tasklet::RetryPolicy;
    /// let _ = RetryPolicy::fixed(3, Duration::from_millis(250));
    /// ```
    pub fn fixed(max_retries: usize, delay: Duration) -> Self {
        RetryPolicy {
            max_retries,
            backoff: Backoff::Fixed(delay),
            jitter: Jitter::None,
        }
    }

    /// Create a policy that retries with exponentially increasing delays.
    ///
    /// The delay before retry `n` (0-indexed) is `base * factor^n`.
    ///
    /// # Arguments
    ///
    /// * max_retries - the maximum number of retries after the first attempt.
    /// * base        - the delay before the first retry.
    /// * factor      - the multiplier applied for each subsequent retry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tasklet::RetryPolicy;
    /// let _ = RetryPolicy::exponential(4, Duration::from_millis(100), 2);
    /// ```
    pub fn exponential(max_retries: usize, base: Duration, factor: u32) -> Self {
        RetryPolicy {
            max_retries,
            backoff: Backoff::Exponential {
                base,
                factor,
                max: None,
            },
            jitter: Jitter::None,
        }
    }

    /// Cap the computed delay at `max` (only affects [`Backoff::Exponential`]).
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tasklet::RetryPolicy;
    /// let _ = RetryPolicy::exponential(4, Duration::from_millis(100), 2)
    ///     .with_max_delay(Duration::from_secs(1));
    /// ```
    pub fn with_max_delay(mut self, max: Duration) -> Self {
        if let Backoff::Exponential { max: ref mut m, .. } = self.backoff {
            *m = Some(max);
        }
        self
    }

    /// Apply a [`Jitter`] strategy to spread retries out in time.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tasklet::retry::Jitter;
    /// # use tasklet::RetryPolicy;
    /// let _ = RetryPolicy::exponential(4, Duration::from_millis(100), 2)
    ///     .with_jitter(Jitter::Full);
    /// ```
    pub fn with_jitter(mut self, jitter: Jitter) -> Self {
        self.jitter = jitter;
        self
    }

    /// Compute the deterministic backoff delay before the retry at the given
    /// (0-indexed) position, ignoring jitter.
    ///
    /// Overflow-safe: an exponential delay that would overflow saturates rather than
    /// panicking.
    pub(crate) fn delay(&self, retry_index: u32) -> Duration {
        match &self.backoff {
            Backoff::Fixed(d) => *d,
            Backoff::Exponential { base, factor, max } => {
                let mult = (*factor as u128).saturating_pow(retry_index);
                let millis = base.as_millis().saturating_mul(mult);
                let delay = Duration::from_millis(millis.min(u64::MAX as u128) as u64);
                match max {
                    Some(cap) => delay.min(*cap),
                    None => delay,
                }
            }
        }
    }

    /// The actual delay to sleep before the given retry, including any configured
    /// jitter. This is what the scheduler uses at runtime.
    pub(crate) fn jittered_delay(&self, retry_index: u32) -> Duration {
        self.jitter.apply(self.delay(retry_index))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn fixed_delay_is_constant() {
        let policy = RetryPolicy::fixed(3, Duration::from_millis(100));
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.delay(0), Duration::from_millis(100));
        assert_eq!(policy.delay(1), Duration::from_millis(100));
        assert_eq!(policy.delay(5), Duration::from_millis(100));
    }

    #[test]
    fn exponential_delay_grows() {
        let policy = RetryPolicy::exponential(5, Duration::from_millis(100), 2);
        assert_eq!(policy.delay(0), Duration::from_millis(100));
        assert_eq!(policy.delay(1), Duration::from_millis(200));
        assert_eq!(policy.delay(2), Duration::from_millis(400));
        assert_eq!(policy.delay(3), Duration::from_millis(800));
    }

    #[test]
    fn exponential_delay_is_capped() {
        let policy = RetryPolicy::exponential(10, Duration::from_millis(100), 2)
            .with_max_delay(Duration::from_millis(500));
        assert_eq!(policy.delay(0), Duration::from_millis(100));
        assert_eq!(policy.delay(2), Duration::from_millis(400));
        // 800ms would exceed the cap, so it is clamped.
        assert_eq!(policy.delay(3), Duration::from_millis(500));
        assert_eq!(policy.delay(10), Duration::from_millis(500));
    }

    #[test]
    fn exponential_delay_does_not_overflow() {
        let policy = RetryPolicy::exponential(usize::MAX, Duration::from_secs(1), 10)
            .with_max_delay(Duration::from_secs(30));
        // A huge retry index would overflow a naive `base * factor^n`; saturation
        // keeps it clamped at the configured maximum.
        assert_eq!(policy.delay(1000), Duration::from_secs(30));
    }

    #[test]
    fn with_max_delay_is_noop_for_fixed() {
        let policy = RetryPolicy::fixed(2, Duration::from_millis(50))
            .with_max_delay(Duration::from_millis(10));
        assert_eq!(policy.delay(0), Duration::from_millis(50));
    }

    #[test]
    fn jitter_defaults_to_none() {
        let policy = RetryPolicy::fixed(2, Duration::from_millis(100));
        assert_eq!(policy.jitter, Jitter::None);
        // Without jitter, the runtime delay equals the deterministic base.
        assert_eq!(policy.jittered_delay(0), Duration::from_millis(100));
    }

    #[test]
    fn full_jitter_stays_within_bounds() {
        fastrand::seed(42);
        let base = Duration::from_millis(100);
        let policy = RetryPolicy::fixed(3, base).with_jitter(Jitter::Full);
        for _ in 0..1000 {
            let d = policy.jittered_delay(0);
            assert!(d <= base, "full jitter must not exceed the base delay");
        }
    }

    #[test]
    fn equal_jitter_stays_within_bounds() {
        fastrand::seed(7);
        let base = Duration::from_millis(100);
        let policy = RetryPolicy::exponential(3, base, 2).with_jitter(Jitter::Equal);
        for _ in 0..1000 {
            // retry_index 1 => base * 2 = 200ms; equal jitter => [100ms, 200ms].
            let d = policy.jittered_delay(1);
            assert!(
                d >= Duration::from_millis(100) && d <= Duration::from_millis(200),
                "equal jitter out of bounds: {:?}",
                d
            );
        }
    }

    #[test]
    fn jitter_produces_varied_values() {
        fastrand::seed(123);
        let policy = RetryPolicy::fixed(3, Duration::from_millis(100)).with_jitter(Jitter::Full);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            seen.insert(policy.jittered_delay(0).as_nanos());
        }
        // With full jitter over 50 draws we expect more than a single value.
        assert!(seen.len() > 1, "jitter should vary the delay");
    }
}

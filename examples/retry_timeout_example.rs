use log::info;
use simple_logger::SimpleLogger;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tasklet::task::TaskStepStatusErr::Error;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{Jitter, RetryPolicy, TaskBuilder, TaskScheduler};

/// Showcase of the resilience features:
///
/// * `.timeout(..)`  — bounds each individual step attempt.
/// * `.retry(..)`    — re-attempts failing steps with a backoff policy and jitter.
/// * `.on_success` / `.on_failure` / `.on_finish` — async lifecycle callbacks.
///
/// The task runs a single time (`repeat(1)`). Its step fails on the first two
/// attempts and succeeds on the third, so the retry policy is exercised, then the
/// task finishes its lifecycle. The scheduler is driven with `run_until` so the
/// program exits on its own after a few seconds.
#[tokio::main]
async fn main() {
    SimpleLogger::new().init().unwrap();

    // Counts how many times the step's body has been invoked (across retries).
    let attempts = Arc::new(AtomicUsize::new(0));
    let step_attempts = attempts.clone();

    let mut scheduler = TaskScheduler::new(250, chrono::Local);

    let _ = scheduler.add_task(
        TaskBuilder::new(chrono::Local)
            .every("* * * * * * *")
            .description("A flaky task guarded by a timeout and retries")
            .repeat(1)
            // Cancel any single attempt that runs longer than 2 seconds.
            .timeout(Duration::from_secs(2))
            // Retry up to 3 times with exponential backoff (100ms, 200ms, 400ms),
            // capped at 1 second, with full jitter so many failing tasks don't all
            // retry at the same instant.
            .retry(
                RetryPolicy::exponential(3, Duration::from_millis(100), 2)
                    .with_max_delay(Duration::from_secs(1))
                    .with_jitter(Jitter::Full),
            )
            // Async lifecycle callbacks.
            .on_success(|| async { info!("run succeeded") })
            .on_failure(|| async { info!("run failed after exhausting retries") })
            .on_finish(|| async { info!("task finished its lifecycle") })
            .add_step("Flaky step", move || {
                let attempts = step_attempts.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt < 2 {
                        info!("attempt #{} failing, will be retried", attempt + 1);
                        Err(Error)
                    } else {
                        info!("attempt #{} succeeding", attempt + 1);
                        Ok(Success)
                    }
                }
            })
            .build(),
    );

    // Run for a few seconds, then stop cleanly — long enough for the task to run,
    // retry and finish.
    scheduler
        .run_until(tokio::time::sleep(Duration::from_secs(5)))
        .await;

    info!("total step attempts: {}", attempts.load(Ordering::SeqCst));
}

use log::info;
use simple_logger::SimpleLogger;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{OverlapPolicy, TaskBuilder, TaskScheduler};

/// Demonstrates two features:
///
/// * Overlap policy (`OverlapPolicy::Skip`): a slow task that takes longer than its
///   interval does not start a second overlapping run, and crucially does not delay
///   the fast task's schedule (runs are non-blocking).
/// * Runtime status queries: a `SchedulerHandle` is polled from another task to print
///   how many tasks are live and what their states are.
#[tokio::main]
async fn main() {
    SimpleLogger::new().init().unwrap();

    let slow_runs = Arc::new(AtomicUsize::new(0));
    let fast_runs = Arc::new(AtomicUsize::new(0));

    let mut scheduler = TaskScheduler::new(200, chrono::Local);

    // A slow task: each run takes ~2 seconds, longer than its one-second cadence.
    let s = slow_runs.clone();
    let slow_task = TaskBuilder::new(chrono::Local)
        .every("* * * * * * *")
        .description("Slow task")
        .overlap(OverlapPolicy::Skip) // default; shown here for clarity
        .add_step("Slow step", move || {
            let s = s.clone();
            async move {
                let n = s.fetch_add(1, Ordering::SeqCst) + 1;
                info!("slow run #{} started, working for 2s...", n);
                tokio::time::sleep(Duration::from_secs(2)).await;
                info!("slow run #{} done", n);
                Ok(Success)
            }
        })
        .build();
    scheduler.add_task(slow_task).unwrap();

    // A fast task on the same cadence; it keeps ticking regardless of the slow task.
    let f = fast_runs.clone();
    let fast_task = TaskBuilder::new(chrono::Local)
        .every("* * * * * * *")
        .description("Fast task")
        .add_step("Fast step", move || {
            let f = f.clone();
            async move {
                info!("fast run #{}", f.fetch_add(1, Ordering::SeqCst) + 1);
                Ok(Success)
            }
        })
        .build();
    scheduler.add_task(fast_task).unwrap();

    // Poll the scheduler's state from another task using the handle.
    let handle = scheduler.handle();
    let observer = tokio::spawn(async move {
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let running = handle.statuses().iter().filter(|t| t.running).count();
            info!(
                "[status] {} task(s) live, {} currently running",
                handle.task_count(),
                running
            );
        }
    });

    // Run for a few seconds, then stop cleanly.
    scheduler
        .run_until(tokio::time::sleep(Duration::from_secs(5)))
        .await;
    observer.await.unwrap();

    info!(
        "totals: slow ran {} time(s), fast ran {} time(s)",
        slow_runs.load(Ordering::SeqCst),
        fast_runs.load(Ordering::SeqCst)
    );
}

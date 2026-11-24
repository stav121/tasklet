use log::info;
use simple_logger::SimpleLogger;
use std::time::Duration;
use tasklet::task::TaskStepStatusErr;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{MissedRunPolicy, RunReport, SchedulerEvent, TaskBuilder, TaskScheduler};

/// Runs the scheduler in a background Tokio task and drives it from `main` through its
/// handle: trigger, pause, resume and remove.
///
/// * `heartbeat` runs every second with `MissedRunPolicy::Skip`, so the slot it missed
///   while paused is dropped on resume instead of running late.
/// * `import` only runs when triggered. Its step fails, and the reason reaches both
///   `on_failure` and the run history.
#[tokio::main]
async fn main() {
    SimpleLogger::new().init().unwrap();

    let mut scheduler = TaskScheduler::new(200, chrono::Utc);
    scheduler
        .add_task(
            TaskBuilder::new(chrono::Utc)
                .every_seconds(1)
                .name("heartbeat")
                .missed_runs(MissedRunPolicy::Skip {
                    grace: Duration::from_millis(500),
                })
                .add_step("beat", |ctx| async move {
                    info!("heartbeat run {}", ctx.run_id());
                    Ok(Success)
                })
                .build(),
        )
        .unwrap()
        .add_task(
            TaskBuilder::new(chrono::Utc)
                // Far enough away that it only runs when triggered.
                .daily_at(3, 0)
                .name("import")
                .add_step("fetch upstream", |_ctx| async {
                    Err(TaskStepStatusErr::error("upstream answered 503"))
                })
                .on_failure(|report: RunReport| async move {
                    info!(
                        "on_failure: step {:?} failed: {}",
                        report.record.failed_step,
                        report.record.error.as_deref().unwrap_or("-")
                    );
                })
                .build(),
        )
        .unwrap();

    // Take what is needed from the scheduler before it moves into the background task.
    let handle = scheduler.handle();
    let mut events = handle.subscribe();
    let runner = tokio::spawn(async move { scheduler.run().await });

    let printer = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                SchedulerEvent::RunSkipped {
                    task_name,
                    scheduled_for,
                    ..
                } => {
                    info!(
                        "[event] {:?} skipped the run due at {}",
                        task_name, scheduled_for
                    )
                }
                SchedulerEvent::TaskRemoved {
                    task_name, reason, ..
                } => {
                    info!("[event] {:?} removed: {:?}", task_name, reason);
                }
                // The scheduler's last event: every task has stopped by now.
                SchedulerEvent::SchedulerStopped => break,
                _ => {}
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(1500)).await;
    info!("[main] triggering import");
    handle.trigger_name("import");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let import_id = handle.id_of_name("import").unwrap();
    for run in handle.history(import_id) {
        info!(
            "[main] import run {}: {:?}, error {:?}",
            run.run_id, run.outcome, run.error
        );
    }

    info!("[main] pausing heartbeat for 2.5 seconds");
    handle.pause_name("heartbeat");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    info!("[main] resuming heartbeat");
    handle.resume_name("heartbeat");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    info!("[main] removing import");
    handle.remove_name("import");
    tokio::time::sleep(Duration::from_millis(300)).await;
    info!("[main] {} task(s) left", handle.task_count());

    handle.shutdown();
    runner.await.unwrap();
    printer.await.unwrap();
}

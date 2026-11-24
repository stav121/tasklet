use log::info;
use simple_logger::SimpleLogger;
use std::time::{Duration, Instant};
use tasklet::task::TaskStepStatusErr;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{FinishReport, SchedulerEvent, TaskBuilder, TaskScheduler};
use tokio::sync::broadcast::error::RecvError;

/// Follows a scheduler through its event stream and shows how a long step cooperates
/// with a shutdown:
///
/// * `SchedulerHandle::subscribe` delivers task and run changes as they happen, so
///   nothing has to poll the handle.
/// * The step waits in `tokio::select!` on `ctx.cancelled()`, so a shutdown interrupts
///   it straight away instead of letting the grace period run out.
/// * The interrupted run is recorded as `RunOutcome::Cancelled`, and `on_finish`
///   receives `FinishReason::Shutdown`.
#[tokio::main]
async fn main() {
    SimpleLogger::new().init().unwrap();

    let mut scheduler = TaskScheduler::new(200, chrono::Utc);
    // A step that ignored cancellation would be aborted after this long.
    scheduler.grace_period(Duration::from_secs(3));

    // Subscribe before adding the task, otherwise its TaskAdded event is already gone.
    let mut events = scheduler.handle().subscribe();
    let printer = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(SchedulerEvent::RunFinished { report }) => {
                    info!(
                        "[event] run {} finished: {:?}",
                        report.record.run_id, report.record.outcome
                    );
                    for step in &report.steps {
                        info!(
                            "[event]   step {} {:?}: {}",
                            step.index,
                            step.status,
                            step.error.as_deref().unwrap_or("no error")
                        );
                    }
                }
                // The scheduler's last event: every task has stopped by now.
                Ok(SchedulerEvent::SchedulerStopped) => {
                    info!("[event] scheduler stopped");
                    break;
                }
                Ok(event) => info!("[event] {:?}", event),
                Err(RecvError::Lagged(missed)) => info!("[event] missed {} event(s)", missed),
                Err(RecvError::Closed) => break,
            }
        }
    });

    scheduler
        .add_task(
            TaskBuilder::new(chrono::Utc)
                .every_seconds(1)
                .name("long-job")
                .add_step("wait for the job", |ctx| async move {
                    info!("run {} started a job that needs a minute", ctx.run_id());
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(60)) => Ok(Success),
                        // Returning an error here marks the run cancelled, not failed.
                        _ = ctx.cancelled() => Err(TaskStepStatusErr::error("interrupted by shutdown")),
                    }
                })
                .on_finish(|report: FinishReport| async move {
                    info!(
                        "on_finish: reason {:?}, last run outcome {:?}",
                        report.reason,
                        report.last_run.and_then(|run| run.outcome)
                    );
                })
                .build(),
        )
        .unwrap();

    // Stop while the first run is still waiting for its minute-long job.
    let started = Instant::now();
    scheduler
        .run_until(tokio::time::sleep(Duration::from_secs(2)))
        .await;
    info!(
        "scheduler stopped after {:.1?}, well inside the 3 second grace period",
        started.elapsed()
    );

    printer.await.unwrap();
}

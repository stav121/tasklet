use log::info;
use simple_logger::SimpleLogger;
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

/// Demonstrates the v0.4.1 additions:
///
/// * **Schedule helpers**: `every_seconds` builds the cron expression for you.
/// * **`add_task_get_id`**: capture the id of an unnamed task to address it later.
/// * **Live step streaming**: the handle observes step transitions *during* a run,
///   so a completed early step is visible while a later step is still in flight.
#[tokio::main]
async fn main() {
    SimpleLogger::new().init().unwrap();

    let mut scheduler = TaskScheduler::new(200, chrono::Local);

    // A two-step task on a 2-second cadence built without writing raw cron. The
    // second step is slow, so its predecessor shows as completed while it runs.
    let task = TaskBuilder::new(chrono::Local)
        .every_seconds(2)
        .add_step("prepare", || async {
            info!("step 1: prepare");
            Ok(Success)
        })
        .add_step("work (slow)", || async {
            info!("step 2: working...");
            tokio::time::sleep(Duration::from_millis(1200)).await;
            info!("step 2: done");
            Ok(Success)
        })
        .build();

    // Capture the assigned id so we can address the (unnamed) task through the handle.
    let id = scheduler.add_task_get_id(task).unwrap();
    info!("task registered with id {}", id);

    // Poll the per-step snapshot while the task runs to watch it advance live.
    let handle = scheduler.handle();
    let observer = tokio::spawn(async move {
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let steps: Vec<String> = handle
                .step_states(id)
                .into_iter()
                .map(|s| format!("{}={:?}", s.description.as_deref().unwrap_or("-"), s.status))
                .collect();
            if !steps.is_empty() {
                info!("[live steps] {}", steps.join(", "));
            }
        }
    });

    scheduler
        .run_until(tokio::time::sleep(Duration::from_secs(6)))
        .await;
    observer.await.unwrap();
}

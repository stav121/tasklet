use log::info;
use simple_logger::SimpleLogger;
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{Blackboard, TaskBuilder, TaskScheduler};

/// Demonstrates the two 0.5.0 kernel additions:
///
/// * **`TaskContext`**: each step is handed a context carrying the run's identity
///   (task id/name, run id, step index, attempt), a task-level [`Blackboard`] shared
///   across runs, and a per-run store for handing values between steps within one run
///   (XCom-like), without capturing any `Arc<Mutex<...>>` by hand.
/// * **`TaskSpawner`**: add a fully-built task to the scheduler while it is already
///   running.
#[tokio::main]
async fn main() {
    SimpleLogger::new().init().unwrap();

    // A task-level blackboard, attached through the builder so every step gets it from
    // its context (no manual clone-capture needed).
    let board = Blackboard::new();

    let mut scheduler = TaskScheduler::new(200, chrono::Local);

    // A two-step task. The first step produces a value into the per-run store; the
    // second step consumes it. The task-level blackboard counts total runs.
    let pipeline = TaskBuilder::new(chrono::Local)
        .every("* * * * * * *")
        .name("pipeline")
        .blackboard(board.clone())
        .add_step("produce", |ctx| async move {
            // Per-run scratch space: fresh each run, visible to later steps this run.
            let value = (ctx.run_id() as u32 + 1) * 10;
            ctx.run_store().set("value", value);
            info!(
                "[{} run {}] produced value = {}",
                ctx.task_name().unwrap_or("-"),
                ctx.run_id(),
                value
            );
            Ok(Success)
        })
        .add_step("consume", |ctx| async move {
            let value = ctx.run_store().get::<u32>("value").unwrap_or(0);
            // Task-level state: survives across runs.
            let total = ctx.blackboard().get_or_insert("runs", 0u32) + 1;
            ctx.blackboard().set("runs", total);
            info!(
                "[{} run {}] consumed value = {} (total runs so far: {})",
                ctx.task_name().unwrap_or("-"),
                ctx.run_id(),
                value,
                total
            );
            Ok(Success)
        })
        .build();
    scheduler.add_task(pipeline).unwrap();

    // Add a second task at runtime, from outside the scheduler, using a spawner.
    let spawner = scheduler.spawner();
    let injector = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(2500)).await;
        info!("[spawn] injecting a 'late' task into the running scheduler");
        let late = TaskBuilder::new(chrono::Local)
            .every("* * * * * * *")
            .name("late")
            .add_step("tick", |ctx| async move {
                info!("late task tick (attempt {})", ctx.attempt());
                Ok(Success)
            })
            .build();
        // Await the id the scheduler assigns it.
        match spawner.spawn_get_id(late).await {
            Ok(id) => info!("[spawn] 'late' task registered with id {}", id),
            Err(e) => info!("[spawn] failed to register: {}", e),
        }
    });

    scheduler
        .run_until(tokio::time::sleep(Duration::from_secs(5)))
        .await;
    injector.await.unwrap();

    info!(
        "final total pipeline runs recorded on the blackboard: {:?}",
        board.get::<u32>("runs")
    );
}

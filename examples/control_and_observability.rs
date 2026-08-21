use log::info;
use simple_logger::SimpleLogger;
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{Blackboard, TaskBuilder, TaskScheduler};

/// Demonstrates the Layer 0 runtime-control and observability surface:
///
/// * **Named tasks**: tasks are addressed by a stable name through the handle.
/// * **Blackboard**: a producer task writes a value that a consumer task reads,
///   without any hand-rolled `Arc<Mutex<...>>`.
/// * **Runtime control**: the handle pauses, triggers and inspects tasks while the
///   scheduler runs.
/// * **Run history / step states**: the handle reads what each task did.
#[tokio::main]
async fn main() {
    SimpleLogger::new().init().unwrap();

    // A blackboard shared between two tasks (each captures its own clone).
    let board = Blackboard::new();

    let mut scheduler = TaskScheduler::new(200, chrono::Local);

    // Producer: bumps a counter on the blackboard every second.
    let producer_board = board.clone();
    let producer = TaskBuilder::new(chrono::Local)
        .every("* * * * * * *")
        .name("producer")
        .add_step("increment", move || {
            let board = producer_board.clone();
            async move {
                let n = board.get_or_insert("count", 0u32) + 1;
                board.set("count", n);
                info!("producer wrote count = {}", n);
                Ok(Success)
            }
        })
        .build();
    scheduler.add_task(producer).unwrap();

    // Consumer: reads whatever the producer last wrote.
    let consumer_board = board.clone();
    let consumer = TaskBuilder::new(chrono::Local)
        .every("* * * * * * *")
        .name("consumer")
        .add_step("read", move || {
            let board = consumer_board.clone();
            async move {
                match board.get::<u32>("count") {
                    Some(n) => info!("consumer read count = {}", n),
                    None => info!("consumer found no count yet"),
                }
                Ok(Success)
            }
        })
        .build();
    scheduler.add_task(consumer).unwrap();

    // Drive control and observation from another task via the handle.
    let handle = scheduler.handle();
    let controller = tokio::spawn(async move {
        // Let a few runs happen.
        tokio::time::sleep(Duration::from_millis(2500)).await;

        // Pause the consumer; the producer keeps going.
        info!("[control] pausing consumer");
        handle.pause_name("consumer");

        tokio::time::sleep(Duration::from_millis(2000)).await;

        // Trigger an immediate producer run, off-schedule.
        info!("[control] triggering producer now");
        handle.trigger_name("producer");

        // Resume the consumer.
        info!("[control] resuming consumer");
        handle.resume_name("consumer");

        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Inspect what happened.
        for state in handle.statuses() {
            info!(
                "[status] task {} ({}) status={:?} runs={} last={:?} paused={}",
                state.id,
                state.name.as_deref().unwrap_or("-"),
                state.status,
                state.run_count,
                state.last_outcome,
                state.paused,
            );
        }
        if let Some(id) = handle.id_of_name("producer") {
            let history = handle.history(id);
            info!("[history] producer has {} recorded run(s)", history.len());
            for step in handle.step_states(id) {
                info!(
                    "[steps] producer step {} ({}) -> {:?}",
                    step.index,
                    step.description.as_deref().unwrap_or("-"),
                    step.status
                );
            }
        }
    });

    scheduler
        .run_until(tokio::time::sleep(Duration::from_secs(8)))
        .await;
    controller.await.unwrap();
}

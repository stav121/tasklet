use log::info;
use simple_logger::SimpleLogger;
use tasklet::{TaskBuilder, TaskScheduler};

/// An example of a `TaskScheduler` instance with 2 `Task` instances.
#[tokio::main]
async fn main() {
    // Initialize the logger.
    SimpleLogger::new().init().unwrap();

    // Just a passed variable.
    let mut count: usize = 5;

    // Create a scheduler instance.
    let mut scheduler = TaskScheduler::new(500, chrono::Utc);

    // Add two tasks, the first one will die after 5 executions,
    // the second will run forever.
    // The first task will execute at seconds 1, 10, 20 of a minute,
    // and the second at the second 30 of each minute.
    scheduler
        .add_task(
            TaskBuilder::new(chrono::Utc)
                .every("1, 10, 20 * * * * * *")
                .description("Just some task")
                .repeat(5)
                .add_step(None, move || {
                    count = count - 1;
                    info!("I have {} more executions left!", count);
                    Ok(())
                })
                .build(),
        )
        .add_task(
            TaskBuilder::new(chrono::Utc)
                .every("30 * * * * * *")
                .description("Just another task")
                .add_step(None, || {
                    info!("I will run forever!");
                    Ok(())
                })
                .build(),
        );

    // Execute the tasks in the queue.
    scheduler.run().await;
}

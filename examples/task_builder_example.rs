use chrono::Utc;
use simple_logger::SimpleLogger;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

/// An example of a `TaskScheduler` instance with one`Task` instance
/// that is executed exactly 5 times and then removed from the schedule.
#[tokio::main]
async fn main() {
    // Initialize the logger.
    SimpleLogger::new().init().unwrap();

    // Create a scheduler instance.
    let mut scheduler = TaskScheduler::new(500, Utc);

    // Append a new task with two steps.
    let _ = scheduler.add_task(
        TaskBuilder::new(Utc)
            .every("* * * * * *")
            .description("Some description")
            .repeat(5)
            .add_step("First step", || Ok(Success))
            .add_step("Second step", || Ok(Success))
            .build(),
    );

    // Execute the scheduler.
    scheduler.run().await;
}

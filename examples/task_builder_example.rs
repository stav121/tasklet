use chrono::Utc;
use simple_logger::SimpleLogger;
use tasklet::{TaskBuilder, TaskScheduler};

/// An example of a `TaskScheduler` instance with one`Task` instance.
fn main() {
    // Initialize the logger.
    SimpleLogger::new().init().unwrap();

    // Create a scheduler instance.
    let mut scheduler = TaskScheduler::new(500, Utc);

    // Append a new task with two steps.
    scheduler.add_task(
        TaskBuilder::new(Utc)
            .every("* * * * * *")
            .description("Some description")
            .repeat(5)
            .add_step(Some("First step"), || Ok(()))
            .add_step(Some("Second step"), || Ok(()))
            .build(),
    );

    // Execute the scheduler.
    scheduler.run();
}

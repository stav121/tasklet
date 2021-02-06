use chrono::Utc;
use log::info;
use simple_logger::SimpleLogger;
use tasklet::{TaskBuilder, TaskGenerator, TaskScheduler};

/// This examples shows how to use a (not so usefull) `TaskGenerator`
/// to generate new tasks for the a `TaskScheduler`.
fn main() {
    // Initialize the logger.
    SimpleLogger::new().init().unwrap();

    // Create a `TaskScheduler` instance.
    let mut scheduler = TaskScheduler::new(500, Utc);
    // Add a new `TaskGenerator` instance that generates a task
    // that leaves for 2 seconds, at the start of each minute.
    scheduler.set_task_gen(TaskGenerator::new("1 * * * * * *", Utc, || {
        // Run at second "1" of every minute.

        // Create the task that will execute 2 total times.
        // Return the task for the execution queue.
        Some(
            TaskBuilder::new(chrono::Utc)
                .every("0,10,20,30,40,50 * * * * * *")
                .description("Generated task")
                .repeat(2)
                .add_step(None, || {
                    info!("[Step 1] This is a generated task!");
                    Ok(())
                })
                .add_step(None, || {
                    info!("[Step 2] This is generated task!");
                    Ok(())
                })
                .build(),
        )
    }));

    // Execute the scheduler.
    scheduler.run();
}

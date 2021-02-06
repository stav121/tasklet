use log::{error, info};
use simple_logger::SimpleLogger;
use tasklet::{TaskBuilder, TaskScheduler};

/// A simple example of a task with two step,
/// that might work or fail some times.
fn main() {
    // Init the logger.
    SimpleLogger::new().init().unwrap();

    // A variable to be passed in the task.
    let mut exec_count = 0;

    // Task scheduler with 2000ms loop frequency.
    let mut scheduler = TaskScheduler::new(2000, chrono::Local);

    // Create a task with 2 steps and add it to the scheduler.
    // The second step fails every second execution.
    // Append the task to the scheduler.
    scheduler.add_task(
        TaskBuilder::new(chrono::Local)
            .every("1 * * * * * *")
            .description("A simple task")
            .add_step(None, || {
                info!("Hello from step 1");
                Ok(()) // Let the scheduler know this step was a success.
            })
            .add_step(None, move || {
                if exec_count % 2 == 0 {
                    error!("Oh no this step failed!");
                    exec_count += 1;
                    Err(()) // Indicate that this step was a fail.
                } else {
                    info!("Hello from step 2");
                    exec_count += 1;
                    Ok(()) // Indicate that this step was a success.
                }
            })
            .build(),
    );

    // Execute the scheduler.
    scheduler.run();
}

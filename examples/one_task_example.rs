use log::{error, info};
use simple_logger::SimpleLogger;
use tasklet::{Task, TaskScheduler};

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
    let mut task = Task::new("1 * * * * * *", Some("A simple task"), None, chrono::Local);
    // A normal step 1.
    task.add_step(Some("Step 1"), || {
        info!("Hello from step 1");
        Ok(()) // Let the scheduler know this step was a success.
    });
    // A step 2 that can fail some times.
    task.add_step(Some("Step 2"), move || {
        if exec_count % 2 == 0 {
            error!("Oh no this step failed!");
            exec_count += 1;
            Err(()) // Indicate that this step was a fail.
        } else {
            info!("Hello from step 2");
            exec_count += 1;
            Ok(()) // Indicate that this step was a success.
        }
    });

    // Append the task to the scheduler.
    scheduler.add_task(task);

    // Execute the scheduler.
    scheduler.run();
}

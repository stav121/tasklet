use log::info;
use simple_logger::SimpleLogger;
use tasklet::task::TaskStepStatusErr::Error;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

/// A simple example of a task with two steps,
/// that might work or fail sometimes.
#[tokio::main]
async fn main() {
    // Init the logger.
    SimpleLogger::new().init().unwrap();

    // A variable to be passed in the task.
    let mut exec_count = 0;

    // Task scheduler with 1000ms loop frequency.
    let mut scheduler = TaskScheduler::default(chrono::Local);

    // Create a task with 2 steps and add it to the scheduler.
    // The second step fails every second execution.
    // Append the task to the scheduler.
    scheduler.add_task(
        TaskBuilder::new(chrono::Local)
            .every("1 * * * * * *")
            .description("A simple task")
            .add_step("Step 1", || {
                info!("Hello from step 1");
                Ok(Success) // Let the scheduler know this step was a success.
            })
            .add_step("Step 2", move || {
                if exec_count % 2 == 0 {
                    exec_count += 1;
                    Err(Error) // Indicate that this step was a fail.
                } else {
                    info!("Hello from step 2");
                    exec_count += 1;
                    Ok(Success) // Indicate that this step was a success.
                }
            })
            .build(),
    );

    // Execute the scheduler.
    scheduler.run().await;
}

use log::info;
use simple_logger::SimpleLogger;
use tasklet::task::TaskStepStatusErr::{Error, ErrorDelete};
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

/// A simple example of a task with two steps, that might work or fail sometimes and
/// on the 10th execution it is forcibly removed from the queue.
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
    let _ = scheduler.add_task(
        TaskBuilder::new(chrono::Local)
            .every("0,5,10,15,20,25,30,35,40,45,50,55 * * * * * *")
            .description("A simple task")
            .add_step("Step 1", || {
                info!("Hello from step 1");
                Ok(Success) // Let the scheduler know this step was a success.
            })
            .add_step("Step 2", move || {
                if exec_count == 10 {
                    info!("Force deleting!");
                    return Err(ErrorDelete);
                }
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

use chrono::Utc;
use simple_logger::SimpleLogger;
use tasklet::{Task, TaskGenerator, TaskScheduler};

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
        let mut task = Task::new("* * * * * * *", Some("Generated task"), Some(2), Utc);
        task.add_step(None, || {
            println!("[Step 1] This is a generated task!");
            Ok(())
        })
        .add_step(None, || {
            println!("[Step 2] This is a generate task!");
            Ok(())
        });

        // Return the task for the execution queue.
        Some(task)
    }));

    // Execute the scheduler.
    scheduler.run();
}

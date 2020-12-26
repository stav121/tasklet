use chrono::Utc;
use log::info;
use simple_logger::SimpleLogger;
use tasklet::{Task, TaskScheduler};

/// An example of a `TaskScheduler` instance with 2 `Task` instances.
fn main() {
    // Initialize the logger.
    SimpleLogger::new().init().unwrap();

    // Just a passed variable.
    let mut count: usize = 5;

    // Create a scheduler instance.
    let mut scheduler = TaskScheduler::new(500, Utc);

    // Add two tasks, the first one will die after 5 executions,
    // the second will run forever.
    // The first task will execute at seconds 1, 10, 20 of a minute,
    // and the second at the second 30 of each minute.
    let mut task_1 = Task::new("1, 10, 20 * * * * * *", Some("Just a task"), Some(5), Utc);
    task_1.add_step(None, move || {
        count = count - 1;
        info!("I have {} more executions left!", count);
        Ok(())
    });
    let mut task_2 = Task::new("30 * * * * * *", Some("Just another task"), None, Utc);
    task_2.add_step(None, || {
        info!("I will run forever!");
        Ok(())
    });
    scheduler.add_task(task_1).add_task(task_2);

    // Execute the tasks in the queue.
    scheduler.run();
}

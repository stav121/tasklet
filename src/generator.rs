extern crate chrono;
extern crate cron;
use crate::errors::TaskResult;
use crate::task::Task;
use chrono::prelude::*;
use chrono::DateTime;
use cron::Schedule;
use log::debug;

/// Task generation structure.
///
/// It is run in the given interval, and if a new task is generated,
/// then it is appended in the list of tasks to be executed.
pub struct TaskGenerator<T>
where
    T: TimeZone + Send + 'static,
{
    /// The discovery function, used to retrieve new tasks.
    discovery_function: Box<dyn FnMut() -> Option<TaskResult<Task<T>>>>,
    /// The execution schedule.
    schedule: Schedule,
    /// The task generator's timezone.
    timezone: T,
    /// The next execution time.
    pub(crate) next_exec: DateTime<T>,
}

/// Implementation of `TaskGenerator`.
impl<T> TaskGenerator<T>
where
    T: TimeZone + Clone + Send + 'static,
{
    /// Create a new `TaskGenerator` instance.
    ///
    /// # Arguments
    ///
    /// * expression    - A valid cron expression.
    /// * function      - The discovery function.
    /// * timezone      - The generator's timezone.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::{TaskGenerator, Task};
    /// // Create a `TaskGenerator` instance.
    /// // It will be executed at the first second of each minute.
    /// let _task_gen = TaskGenerator::new("1 * * * * * *", chrono::Local, || Some(Task::new("* * * * * * *", None, None, chrono::Local)));
    /// ```
    pub fn new<F>(expression: &str, timezone: T, function: F) -> TaskGenerator<T>
    where
        F: (FnMut() -> Option<TaskResult<Task<T>>>) + 'static,
    {
        let schedule: Schedule = expression.parse().unwrap();

        TaskGenerator {
            discovery_function: Box::new(function),
            schedule: expression.parse().unwrap(),
            timezone: timezone.clone(),
            next_exec: schedule.upcoming(timezone).next().unwrap(),
        }
    }

    /// Run the discovery function and reschedule the generation function.
    pub(crate) fn run(&mut self) -> Option<TaskResult<Task<T>>> {
        debug!("Executing discovery function");
        self.next_exec = self.schedule.upcoming(self.timezone.clone()).next()?;
        match (self.discovery_function)() {
            Some(t) => {
                // A task was generated and must be returned.
                debug!("A task was found, adding it to the queue...");
                Some(t)
            }
            None => {
                debug!("No task was generated");
                None
            }
        }
    }
}

/// Module tests.
#[cfg(test)]
mod test {
    use super::*;
    use chrono::Local;

    /// Test the normal flow of a task generation instance.
    ///
    /// In this case the function is returning a new `Task`.
    #[test]
    fn test_task_generation_with_result() {
        // Create a task generation instance.
        let mut task_gen = TaskGenerator::new("* * * * * * *", Local, || {
            Some(Task::new("* * * * * * *", None, Some(1), Local))
        });
        assert!(task_gen.run().is_some());
    }

    /// Test the normal flow of a task generation instance.
    ///
    /// In this case no new `Task` instance is generated.
    #[test]
    fn test_task_generation_without_result() {
        // Create a task generation instance.
        let mut task_gen = TaskGenerator::new("* * * * * * *", Local, || None);
        assert!(task_gen.run().is_none());
    }
}

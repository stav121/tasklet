extern crate chrono;
extern crate cron;

use chrono::DateTime;
use chrono::TimeZone;
use cron::Schedule;
use log::{debug, error, warn};

/// A task step.
///
/// Contains the executable body and an optional short description.
pub struct TaskStep<'a> {
    /// The function's body.
    pub(crate) function: Box<dyn (FnMut() -> Result<(), ()>) + 'a>,
    /// An (optional) short description.
    pub(crate) description: String,
}

/// Available task statuses.
///
/// - Init      => The task is not initialized yet.
/// - Scheduled => The task has been scheduled and pending execution.
/// - Failed    => The task has executed but has failed.
/// - Executed  => The task has executed successfully.
/// - Finished  => The task has finished and can be removed from the queue.
#[derive(Debug, PartialEq)]
pub enum Status {
    Init,
    Scheduled,
    Failed,
    Executed,
    Finished,
}

/// A structure that contains the basic information of the job.
pub struct Task<'a, T>
where
    T: TimeZone,
{
    /// Task's executable tasks.
    pub(crate) steps: Vec<TaskStep<'a>>,
    /// The execution schedule.
    pub(crate) schedule: Schedule,
    /// Total number of executions, if `None` then it will run forever.
    pub(crate) repeats: Option<usize>,
    /// (Optional) Task's description.
    pub(crate) description: String,
    /// The timezone of the task.
    pub(crate) timezone: T,
    /// (Internal) task id.
    pub(crate) task_id: usize,
    /// (Internal) next execution time.
    pub(crate) next_exec: Option<DateTime<T>>,
    /// (Internal) task status.
    pub(crate) status: Status,
}

/// `Task` implementation.
impl<'a, T> Task<'a, T>
where
    T: TimeZone,
{
    /// Create a new instance of type `Task`.
    ///
    /// # Arguments
    ///
    /// * expression    - A valid cron expression.
    /// * description   - (Optional) description.
    /// * repeats       - maximum number of repeats, if `None` this task will run forever.
    /// * timezone      - The tasks' timezone.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::Task;
    /// // Create a new task instance. This task will execute every second for 5 times.
    /// let _task = Task::new("* * * * * * * ", Some("Runs every second!"), Some(5), chrono::Utc);
    /// ```
    /// ```
    /// # use tasklet::Task;
    /// // Create a new task instance. This task will run on second 30 of each minute forever.
    /// let _task_1 = Task::new("30 * * * * * *", Some("Runs every second 30 of a minute!"), None, chrono::Local);
    /// ```
    pub fn new(
        expression: &str,
        description: Option<&str>,
        repeats: Option<usize>,
        timezone: T,
    ) -> Task<'a, T> {
        Task {
            steps: Vec::new(),
            schedule: expression.parse().unwrap(),
            description: match description {
                Some(s) => s.to_string(),
                None => "-".to_string(),
            },
            repeats,
            timezone,
            task_id: 0,
            status: Status::Init,
            next_exec: None,
        }
    }

    /// Add a new `TaskStep` in the `Task`.
    ///
    /// # Arguments
    ///
    /// * description   - A short task step description (Optional).
    /// * function      - The executable function.
    pub(crate) fn add_step<F>(&mut self, description: Option<&str>, function: F) -> &mut Task<'a, T>
    where
        F: (FnMut() -> Result<(), ()>) + 'a,
    {
        self.steps.push(TaskStep {
            function: Box::new(function),
            description: match description {
                Some(s) => s.to_string(),
                None => "-".to_string(),
            },
        });
        self
    }

    /// Set the value of the steps vector.
    ///
    /// # Arguments
    ///
    /// * steps   - A vector that contains the executable steps.
    pub(crate) fn set_steps(&mut self, steps: Vec<TaskStep<'a>>) -> &mut Task<'a, T> {
        self.steps = steps;
        self
    }

    /// Set the value of `schedule` property.
    ///
    /// # Arguments
    ///
    /// * schedule  - The schedule.
    pub(crate) fn set_schedule(&mut self, schedule: Schedule) -> &mut Task<'a, T> {
        self.schedule = schedule;
        self
    }

    /// Initialize the `Task` instance and schedule the first execution.
    ///
    /// # Arguments
    ///
    /// * id - The task's id.
    pub(crate) fn init(&mut self, id: usize) {
        self.task_id = id;
        self.next_exec = Some(
            self.schedule
                .upcoming(self.timezone.clone())
                .next()
                .unwrap(),
        );
        self.status = Status::Scheduled;
    }

    /// Run the task and handle the output.
    pub(crate) fn run_task(&mut self) {
        match &self.status {
            Status::Init => panic!("Task not initialized yet!"),
            Status::Failed => panic!("Task must be rescheduled!"),
            Status::Executed => panic!("Task already executed and must be rescheduled!"),
            Status::Finished => panic!("Task has finished and must be removed!"),
            Status::Scheduled => {
                debug!(
                    "[Task {}] [{}] is been executed...",
                    self.task_id, self.description
                );
                let mut had_error: bool = false;
                for (index, step) in self.steps.iter_mut().enumerate() {
                    if !had_error {
                        match (step.function)() {
                            Ok(_) => {
                                debug!(
                                    "[Task {}-{}] [{}] Executed successfully.",
                                    self.task_id, index, step.description,
                                );
                                self.status = Status::Executed
                            }
                            Err(_) => {
                                error!(
                                    "[Task {}]  [{}] Execution failed at step {}.",
                                    self.task_id, index, step.description,
                                );
                                // Indicate that there was an error.
                                had_error = true;
                                self.status = Status::Failed
                            }
                        };
                    }
                }
                // Avoid underflow in case of a task without steps.
                if self.steps.len() == 0 {
                    self.status = Status::Executed
                }

                // Reduce the total executions (if set).
                self.repeats = match self.repeats {
                    Some(t) => Some(t - 1),
                    None => None,
                };
            }
        }
    }

    /// Reschedule the current task instance (if needed).
    pub(crate) fn reschedule(&mut self) {
        match &self.status {
            Status::Init => panic!("Task not initialized yet!"),
            Status::Failed | Status::Executed => {
                self.next_exec = Some(
                    self.schedule
                        .upcoming(self.timezone.clone())
                        .next()
                        .unwrap(),
                );
                self.status = match self.repeats {
                    Some(t) => {
                        if t > 0 {
                            debug!("[Task {}] Has been rescheduled.", self.task_id);
                            Status::Scheduled
                        } else {
                            warn!(
                                "[Task  {}] Has finished it's execution cycle and will be removed.",
                                self.task_id
                            );
                            Status::Finished
                        }
                    }
                    None => Status::Scheduled,
                }
            }
            Status::Finished => panic!("[Task {}] has finished and must be removed!", self.task_id),
            Status::Scheduled => { /* Do nothing */ }
        }
    }
}

/// Module tests
#[cfg(test)]
mod test {
    use super::*;
    use chrono::prelude::*;

    /// Test the normal execution of a simple task.
    /// Verify the status changes.
    #[test]
    fn normal_task_flow_test() {
        let mut task = Task::new("* * * * * *", Some("Test task"), Some(2), Local);
        task.add_step(None, || Ok(()));
        assert_eq!(task.status, Status::Init);
        task.init(0);
        assert_eq!(task.status, Status::Scheduled);
        task.run_task();
        assert_eq!(task.status, Status::Executed);
        task.reschedule();
        assert_eq!(task.status, Status::Scheduled);
        task.run_task();
        assert_eq!(task.status, Status::Executed);
        task.reschedule();
        assert_eq!(task.status, Status::Finished);
    }

    /// Test the normal execution of the `set_schedule` function.
    #[test]
    fn test_task_set_schedule() {
        let schedule: Schedule = "* * * * * * *".parse().unwrap();
        let mut task = Task::new("* * * * * * *", None, None, Local);
        task.set_schedule(schedule);
        task.add_step(None, || Ok(()));
        assert_eq!(task.status, Status::Init);
        task.init(0);
        assert_eq!(task.status, Status::Scheduled);
    }

    /// Test the normal execution of a simple task.
    /// Verify the status changes.
    #[test]
    fn normal_task_error_flow_test() {
        let mut task = Task::new("* * * * * *", Some("Test task"), Some(2), Local);
        task.add_step(None, || Err(()));
        assert_eq!(task.status, Status::Init);
        task.init(0);
        assert_eq!(task.status, Status::Scheduled);
        task.run_task();
        assert_eq!(task.status, Status::Failed);
        task.reschedule();
        assert_eq!(task.status, Status::Scheduled);
        task.run_task();
        assert_eq!(task.status, Status::Failed);
        task.reschedule();
        assert_eq!(task.status, Status::Finished);
    }

    /// Test the normal execution of a simple task, without fixed repeats.
    #[test]
    fn normal_task_no_fixed_repeats_test() {
        let mut task = Task::new("* * * * * * *", Some("Test task"), None, Local);
        task.add_step(None, || Ok(()));
        assert_eq!(task.status, Status::Init);
        task.init(0);
        assert_eq!(task.status, Status::Scheduled);
        // Run it for a few times.
        for _i in 1..10 {
            task.run_task();
            assert_eq!(task.status, Status::Executed);
            task.reschedule();
            assert_eq!(task.status, Status::Scheduled);
        }
    }

    /// Test the rescheduling of an unintialized task.
    #[test]
    #[should_panic(expected = "Task not initialized yet!")]
    fn test_reschedule_init_panic() {
        let mut task = Task::new("* * * * * * *", None, None, Local);
        // This task is not initialized, so it should fail.
        task.reschedule();
    }

    /// Test the rescheduling of a task that has been marked as finished.
    #[test]
    #[should_panic(expected = "[Task 0] has finished and must be removed!")]
    fn test_reschedule_finished_panic() {
        let mut task = Task::new("* * * * * * *", None, Some(1), Local);
        // Execute the task.
        task.init(0);
        task.run_task();
        task.reschedule();
        // Try to reschedule after it's finished. It should fail.
        task.reschedule();
    }

    /// Test the execution of an uninitialized task.
    #[test]
    #[should_panic = "Task not initialized yet!"]
    fn test_run_uninitialized_task() {
        let mut task = Task::new("* * * * * * *", None, None, Local);
        task.run_task();
    }

    /// Test the exection of a not rescheduled failed task.
    #[test]
    #[should_panic = "Task must be rescheduled!"]
    fn test_run_failed_task() {
        let mut task = Task::new("* * * * * * *", None, None, Local);
        task.add_step(None, || Err(()));
        task.init(0);
        task.run_task();
        // Attempt to rerun it, it should fail.
        task.run_task();
    }

    /// Test the execution of a task that has been run but not rescheduled.
    #[test]
    #[should_panic = "Task already executed and must be rescheduled!"]
    fn test_run_executed_task() {
        let mut task = Task::new("* * * * * * *", None, None, Local);
        task.add_step(None, || Ok(()));
        task.init(0);
        task.run_task();
        // Attempt to run it again, it should fail.
        task.run_task();
    }

    /// Test the execution of a task that has already finished.
    #[test]
    #[should_panic = "Task has finished and must be removed!"]
    fn test_run_finished_task() {
        let mut task = Task::new("* * * * * * *", None, Some(1), Local);
        task.init(0);
        task.run_task();
        task.reschedule();
        // At this point the task is Finished. It should not be allowed to run again.
        task.run_task();
    }
}

extern crate chrono;
extern crate cron;

use chrono::TimeZone;
use chrono::{DateTime, Utc};
use cron::Schedule;
use log::{debug, error, warn};
use std::fmt;
use tokio::sync::{mpsc, oneshot};

/// Possible success status values for a step's execution.
#[derive(Debug)]
pub enum TaskStepStatusOk {
    /// The step was a success, move to the next one (or exit if last).
    Success,
    /// The step execution had errors but can continue the execution.
    HadErrors,
}

/// Possible error status values for a step's execution.
#[derive(Debug)]
pub enum TaskStepStatusErr {
    /// The task step execution failed.
    Error,
    /// The step failed and the task has to be removed from the execution list.
    ErrorDelete,
}

/// An executable function.
pub type ExecutableFn = dyn FnMut() -> Result<TaskStepStatusOk, TaskStepStatusErr> + 'static + Send;

/// A task step.
///
/// Contains the executable body and an optional short description.
pub struct TaskStep {
    /// The function's body.
    pub(crate) function: Box<ExecutableFn>,
    /// An (optional) short description.
    pub(crate) description: Option<String>,
}

impl fmt::Display for TaskStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.description {
            Some(desc) if !desc.is_empty() => write!(f, "{}", desc),
            _ => write!(f, "-"),
        }
    }
}

impl TaskStep {
    /// Default constructor.
    ///
    /// # Arguments
    ///
    /// * description   - a description for the task step
    /// * function      - the executable body of the function
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::task::{TaskStep, TaskStepStatusOk};
    /// let _ = TaskStep::new("Some task", || Ok(TaskStepStatusOk::Success));
    /// ```
    pub fn new<F>(description: &str, function: F) -> Self
    where
        F: (FnMut() -> Result<TaskStepStatusOk, TaskStepStatusErr>) + 'static + Send,
    {
        Self {
            description: Some(description.to_string()),
            function: Box::new(function),
        }
    }

    /// Default constructor for a task step without a provided description.
    ///
    /// # Arguments
    ///
    /// *function -> the executable function body
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::task::TaskStep;
    /// use tasklet::task::TaskStepStatusOk::Success;
    /// let _ = TaskStep::default(|| {Ok(Success)});
    /// ```
    pub fn default<F>(function: F) -> Self
    where
        F: (FnMut() -> Result<TaskStepStatusOk, TaskStepStatusErr>) + 'static + Send,
    {
        Self {
            function: Box::new(function),
            description: None,
        }
    }
}

/// Available task statuses.
#[derive(Debug, PartialEq, Default, Clone)]
pub enum Status {
    #[default]
    /// The task is not initialized yet.
    Init,
    /// The task has been scheduled and pending execution.
    Scheduled,
    /// The task has executed but has failed.
    Failed,
    /// The task has executed successfully.
    Executed,
    /// The task has finished and can be removed from the queue.
    Finished,
    /// The task is forcibly removed from the execution list due to fatal error.
    ForceRemoved,
}

/// A message response from a task
#[derive(Debug)]
pub(crate) struct TaskResponse {
    /// The id of the task as set by the scheduler
    pub id: usize,
    /// The status after the request has been fulfilled
    pub status: Status,
}

#[derive(Debug)]
/// Available commands to be sent
pub(crate) enum TaskCmd {
    /// Request to initialize the task
    Init {
        sender: oneshot::Sender<TaskResponse>,
    },
    /// Execute the task
    Run {
        sender: oneshot::Sender<TaskResponse>,
    },
    /// Request the rescheduling of the task
    Reschedule {
        sender: oneshot::Sender<TaskResponse>,
    },
}

/// A structure that contains the basic information of the job.
pub struct Task<T>
where
    T: TimeZone + Send + 'static,
{
    /// Task's executable tasks.
    pub(crate) steps: Vec<TaskStep>,
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
    /// Task receiver
    pub(crate) receiver: Option<mpsc::Receiver<TaskCmd>>,
}

unsafe impl<T> Send for Task<T> where T: TimeZone + Send + 'static {}

impl<T> Task<T>
where
    T: TimeZone + Send + 'static,
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
    ) -> Task<T> {
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
            status: Status::default(),
            next_exec: None,
            receiver: None,
        }
    }

    /// Set the receiver for the task.
    pub(crate) fn set_receiver(&mut self, receiver: mpsc::Receiver<TaskCmd>) {
        self.receiver = Some(receiver);
    }

    /// Set the task id of the current task.
    ///
    /// # Arguments
    ///
    /// * id    - the id of the task
    ///
    /// # Examples
    ///
    /// ```
    /// # use chrono::Utc;
    /// # use tasklet::task::Task;
    ///
    /// let mut t = Task::new("* * * * * *", None, None, Utc);
    /// t.set_id(0);
    /// ```
    pub fn set_id(&mut self, id: usize) {
        self.task_id = id;
    }

    /// Add a new `TaskStep` in the `Task`.
    ///
    /// # Arguments
    ///
    /// * description   - A short task step description (Optional).
    /// * function      - The executable function.
    #[cfg(test)]
    pub(crate) fn add_step<F>(&mut self, description: &str, function: F) -> &mut Task<T>
    where
        F: (FnMut() -> Result<TaskStepStatusOk, TaskStepStatusErr>) + 'static + Send,
    {
        self.steps.push(TaskStep::new(description, function));
        self
    }

    /// Add a new `TaskStep` in the `Task` without a provided name/description.
    ///
    /// # Arguments
    ///
    /// * function  - the executable function
    #[cfg(test)]
    pub(crate) fn add_step_default<F>(&mut self, function: F) -> &mut Task<T>
    where
        F: (FnMut() -> Result<TaskStepStatusOk, TaskStepStatusErr>) + 'static + Send,
    {
        self.steps.push(TaskStep::default(function));
        self
    }

    /// Set the value of the steps vector.
    ///
    /// # Arguments
    ///
    /// * steps   - A vector that contains the executable steps.
    pub(crate) fn set_steps(&mut self, steps: Vec<TaskStep>) -> &mut Task<T> {
        self.steps = steps;
        self
    }

    /// Set the value of `schedule` property.
    ///
    /// # Arguments
    ///
    /// * schedule  - The schedule.
    pub(crate) fn set_schedule(&mut self, schedule: Schedule) -> &mut Task<T> {
        self.schedule = schedule;
        self
    }

    /// Initialize the `Task` instance and schedule the first execution.
    ///
    /// # Arguments
    ///
    /// * id - The task's id.
    pub(crate) fn init(&mut self) {
        debug!("Task with id {} is initializing", self.task_id);
        self.next_exec = Some(
            self.schedule
                .upcoming(self.timezone.clone())
                .next()
                .unwrap(),
        );
        self.status = Status::Scheduled;
        debug!("Task with id {} finished initializing", self.task_id);
    }

    /// Create a `TaskResponse` from the current state of the task.
    fn get_task_response(&self) -> TaskResponse {
        TaskResponse {
            id: self.task_id,
            status: self.status.clone(),
        }
    }

    /// Execute a command sent by the scheduler.
    ///
    /// Each of the commands triggers the underlying method of the task,
    /// and responds with the id of the task and the status of the task after the execution
    /// of the command has finished.
    pub(crate) fn execute_command(&mut self, msg: TaskCmd) {
        match msg {
            TaskCmd::Run { sender } => {
                if self.next_exec.as_ref().unwrap()
                    <= &Utc::now().with_timezone(&self.timezone.clone())
                {
                    self.run_task();
                }
                let _ = sender.send(self.get_task_response());
            }
            TaskCmd::Reschedule { sender } => {
                self.reschedule();
                let _ = sender.send(self.get_task_response());
            }
            TaskCmd::Init { sender } => {
                if self.status == Status::Init {
                    self.init();
                }
                let _ = sender.send(self.get_task_response());
            }
        }
    }

    /// Run the task and handle the output.
    pub(crate) fn run_task(&mut self) {
        match &self.status {
            Status::Init => panic!("Task not initialized yet!"),
            Status::Failed => panic!("Task must be rescheduled!"),
            Status::Executed => panic!("Task already executed and must be rescheduled!"),
            Status::Finished => panic!("Task has finished and must be removed!"),
            Status::ForceRemoved => panic!("Task has been forced removed!"),
            Status::Scheduled => {
                debug!(
                    "[Task {}] [{}] is been executed...",
                    self.task_id, self.description
                );
                let mut had_error: bool = false;
                for (index, step) in self.steps.iter_mut().enumerate() {
                    if !had_error {
                        match (step.function)() {
                            Ok(status) => {
                                match status {
                                    TaskStepStatusOk::Success => debug!("[Task step {}-{}] [{}] Executed successfully",self.task_id, index, step),
                                    TaskStepStatusOk::HadErrors => debug!("[Task step {}-{}] [{}] Executed successfully but had some non fatal errors",self.task_id, index, step)
                                }
                                self.status = Status::Executed
                            }
                            Err(status) => {
                                // Indicate that there was an error.
                                had_error = true;
                                match status {
                                    TaskStepStatusErr::Error => {
                                        error!(
                                            "[Task step {}-{}] [{}] Execution failed",
                                            self.task_id, index, step
                                        );
                                        self.status = Status::Failed
                                    }
                                    TaskStepStatusErr::ErrorDelete => {
                                        error!("[Task step {}-{}] [{}] Execution failed and the task is marked for deletion",self.task_id, index, step);
                                        self.status = Status::ForceRemoved
                                    }
                                }
                            }
                        };
                    }
                }
                // Avoid underflow in case of a task without steps.
                if self.steps.is_empty() {
                    self.status = Status::Executed
                }

                // Reduce the total executions (if set).
                self.repeats = self.repeats.map(|r| r - 1);
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
                            debug!("[Task {}] Has been rescheduled", self.task_id);
                            Status::Scheduled
                        } else {
                            warn!(
                                "[Task  {}] Has finished its execution cycle and will be removed",
                                self.task_id
                            );
                            Status::Finished
                        }
                    }
                    None => Status::Scheduled,
                }
            }
            Status::Finished | Status::ForceRemoved => warn!(
                "[Task {}] The task will be removed from the queue",
                self.task_id
            ),
            Status::Scheduled => { /* Do nothing, keep silent */ }
        }
    }
}

/// Wrap a `Task` around a receiver, each time a command is received, forward it to the task.
///
/// # Arguments
///
/// * task  - the task to run in the background
///
/// # Examples
///
/// ```
/// # use chrono::Utc;
/// # use tasklet::task::Task;
/// # use tasklet::task::run_task;
/// # tokio_test::block_on( async {
/// let t = Task::new("* * * * * *", None, None, Utc);
/// let h = tokio::spawn(run_task(t));
/// # h.abort();
/// # })
/// ```
pub async fn run_task<T>(mut task: Task<T>)
where
    T: TimeZone + Send + 'static,
{
    while let Some(msg) = task
        .receiver
        .as_mut()
        .expect("Failed to borrow receiver.")
        .recv()
        .await
    {
        task.execute_command(msg);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::task::TaskStepStatusErr::{Error, ErrorDelete};
    use crate::task::TaskStepStatusOk::Success;
    use chrono::prelude::*;

    #[test]
    fn normal_task_flow_test() {
        let mut task = Task::new("* * * * * *", Some("Test task"), Some(2), Local);
        task.add_step_default(|| Ok(Success));
        assert_eq!(task.status, Status::Init);
        task.set_id(0);
        task.init();
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

    #[test]
    fn test_task_set_schedule() {
        let schedule: Schedule = "* * * * * * *".parse().unwrap();
        let mut task = Task::new("* * * * * * *", None, None, Local);
        task.set_schedule(schedule);
        task.add_step_default(|| Ok(Success));
        assert_eq!(task.status, Status::Init);
        task.set_id(0);
        task.init();
        assert_eq!(task.status, Status::Scheduled);
    }

    #[test]
    fn normal_task_error_flow_test() {
        let mut task = Task::new("* * * * * *", Some("Test task"), Some(2), Local);
        task.add_step_default(|| Err(Error));
        assert_eq!(task.status, Status::Init);
        task.set_id(0);
        task.init();
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
        task.add_step_default(|| Ok(Success));
        assert_eq!(task.status, Status::Init);
        task.set_id(0);
        task.init();
        assert_eq!(task.status, Status::Scheduled);
        // Run it for a few times.
        for _i in 1..10 {
            task.run_task();
            assert_eq!(task.status, Status::Executed);
            task.reschedule();
            assert_eq!(task.status, Status::Scheduled);
        }
    }

    #[test]
    #[should_panic(expected = "Task not initialized yet!")]
    fn test_reschedule_init_panic() {
        let mut task = Task::new("* * * * * * *", None, None, Local);
        // This task is not initialized, so it should fail.
        task.reschedule();
    }

    #[test]
    fn test_reschedule_finished_should_mark_as_finished() {
        let mut task = Task::new("* * * * * * *", None, Some(1), Local);
        // Execute the task.
        task.set_id(0);
        task.init();
        task.run_task();
        task.reschedule();
        assert_eq!(task.status, Status::Finished);
    }

    #[test]
    #[should_panic = "Task not initialized yet!"]
    fn test_run_uninitialized_task() {
        let mut task = Task::new("* * * * * * *", None, None, Local);
        task.run_task();
    }

    #[test]
    #[should_panic = "Task must be rescheduled!"]
    fn test_run_failed_task() {
        let mut task = Task::new("* * * * * * *", None, None, Local);
        task.add_step_default(|| Err(Error));
        task.set_id(0);
        task.init();
        task.run_task();
        // Attempt to rerun it, it should fail.
        task.run_task();
    }

    #[test]
    #[should_panic = "Task already executed and must be rescheduled!"]
    fn test_run_executed_task() {
        let mut task = Task::new("* * * * * * *", None, None, Local);
        task.add_step("Step 1", || Ok(Success));
        task.set_id(0);
        task.init();
        task.run_task();
        // Attempt to run it again, it should fail.
        task.run_task();
    }

    #[test]
    #[should_panic = "Task has finished and must be removed!"]
    fn test_run_finished_task() {
        let mut task = Task::new("* * * * * * *", None, Some(1), Local);
        task.set_id(0);
        task.init();
        task.run_task();
        task.reschedule();
        // At this point the task is Finished. It should not be allowed to run again.
        task.run_task();
    }

    #[test]
    fn test_run_failed_delete() {
        let mut task = Task::new("* * * * * * *", None, None, Local);
        task.add_step_default(|| Err(ErrorDelete));
        task.set_id(0);
        task.init();
        task.run_task();
        assert_eq!(task.status, Status::ForceRemoved);
    }
}

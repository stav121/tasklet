extern crate chrono;
extern crate cron;

use crate::blackboard::Blackboard;
use crate::errors::{TaskError, TaskResult};
use crate::retry::RetryPolicy;
use crate::{step_log, task_log};
use chrono::TimeZone;
use chrono::{DateTime, Utc};
use cron::Schedule;
use std::collections::VecDeque;
use std::fmt::{self, Debug};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// The default number of past [`RunRecord`]s kept per task.
pub(crate) const DEFAULT_HISTORY_LIMIT: usize = 20;

/// Possible success status values for a step's execution.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskStepStatusOk {
    /// The step was a success, move to the next one (or exit if last).
    Success,
    /// The step execution had errors but can continue the execution.
    HadErrors,
}

/// Possible error status values for a step's execution.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskStepStatusErr {
    /// The task step execution failed.
    Error,
    /// The step failed and the task has to be removed from the execution list.
    ErrorDelete,
}

/// The boxed future produced by a task step when it is invoked.
///
/// Steps are asynchronous: invoking a step returns a `Future` that is awaited by the
/// scheduler, allowing steps to perform real async work (I/O, timers, etc.).
pub type StepFuture =
    Pin<Box<dyn Future<Output = Result<TaskStepStatusOk, TaskStepStatusErr>> + Send>>;

/// An executable, asynchronous function.
///
/// Each call is handed a [`TaskContext`] for the current attempt and produces a fresh
/// [`StepFuture`] to be awaited.
pub type ExecutableFn = dyn FnMut(TaskContext) -> StepFuture + 'static + Send;

/// The execution context handed to a step each time it runs.
///
/// A `TaskContext` identifies the run (which task, which run, which step, which attempt)
/// and gives the step access to two typed key/value stores:
///
/// * [`blackboard`](TaskContext::blackboard) - the task-level [`Blackboard`], shared
///   across every run of the task and, if the same blackboard is attached to several
///   tasks via [`TaskBuilder::blackboard`](crate::TaskBuilder::blackboard), across those
///   tasks too. Use it for state that must outlive a single run.
/// * [`run_store`](TaskContext::run_store) - a fresh [`Blackboard`] created for each run,
///   so steps can hand values to later steps within the same run without leaking them
///   into the next run (an XCom-like scratchpad).
///
/// The context is cheap to clone (both stores are `Arc`-backed); it is moved into the
/// step's future, so a step can keep it for the duration of its async work.
///
/// # Examples
///
/// ```
/// use tasklet::task::TaskStepStatusOk::Success;
/// use tasklet::TaskBuilder;
///
/// let _task = TaskBuilder::new(chrono::Local)
///     .every("* * * * * * *")
///     .add_step("produce", |ctx| async move {
///         ctx.run_store().set("value", 21u32);
///         Ok(Success)
///     })
///     .add_step("consume", |ctx| async move {
///         let doubled = ctx.run_store().get::<u32>("value").unwrap_or(0) * 2;
///         assert_eq!(doubled, 42);
///         Ok(Success)
///     })
///     .build();
/// ```
#[derive(Clone)]
pub struct TaskContext {
    task_id: usize,
    task_name: Option<Arc<str>>,
    run_id: usize,
    step_index: usize,
    attempt: u32,
    blackboard: Blackboard,
    run_store: Blackboard,
}

impl TaskContext {
    /// The id the scheduler assigned to the running task.
    pub fn task_id(&self) -> usize {
        self.task_id
    }

    /// The task's unique name, if one was set via
    /// [`TaskBuilder::name`](crate::TaskBuilder::name).
    pub fn task_name(&self) -> Option<&str> {
        self.task_name.as_deref()
    }

    /// The id of the current run (monotonically increasing per task).
    pub fn run_id(&self) -> usize {
        self.run_id
    }

    /// The zero-based index of the step this context was handed to.
    pub fn step_index(&self) -> usize {
        self.step_index
    }

    /// The current attempt number for this step, starting at 1. Values above 1 mean an
    /// earlier attempt failed and is being retried under the task's
    /// [`RetryPolicy`].
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The task-level [`Blackboard`], shared across runs (and across tasks that share
    /// the same blackboard).
    pub fn blackboard(&self) -> &Blackboard {
        &self.blackboard
    }

    /// A per-run [`Blackboard`], created fresh for each run, for handing values between
    /// steps within a single run.
    pub fn run_store(&self) -> &Blackboard {
        &self.run_store
    }
}

impl fmt::Debug for TaskContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskContext")
            .field("task_id", &self.task_id)
            .field("task_name", &self.task_name)
            .field("run_id", &self.run_id)
            .field("step_index", &self.step_index)
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

/// The boxed future produced by a lifecycle callback when it is invoked.
pub type CallbackFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// An asynchronous lifecycle callback (on-success / on-failure / on-finish).
///
/// Each call produces a fresh [`CallbackFuture`] to be awaited by the scheduler.
pub type CallbackFn = dyn FnMut() -> CallbackFuture + 'static + Send;

/// Box an async closure into a [`CallbackFn`].
pub(crate) fn boxed_callback<F, Fut>(mut callback: F) -> Box<CallbackFn>
where
    F: (FnMut() -> Fut) + 'static + Send,
    Fut: Future<Output = ()> + Send + 'static,
{
    Box::new(move || Box::pin(callback()))
}

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
    /// let _ = TaskStep::new("Some task", |_ctx| async { Ok(TaskStepStatusOk::Success) });
    /// ```
    pub fn new<F, Fut>(description: &str, mut function: F) -> Self
    where
        F: (FnMut(TaskContext) -> Fut) + 'static + Send,
        Fut: Future<Output = Result<TaskStepStatusOk, TaskStepStatusErr>> + Send + 'static,
    {
        Self {
            description: Some(description.to_string()),
            function: Box::new(move |ctx| Box::pin(function(ctx))),
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
    /// let _ = TaskStep::default(|_ctx| async { Ok(Success) });
    /// ```
    pub fn default<F, Fut>(mut function: F) -> Self
    where
        F: (FnMut(TaskContext) -> Fut) + 'static + Send,
        Fut: Future<Output = Result<TaskStepStatusOk, TaskStepStatusErr>> + Send + 'static,
    {
        Self {
            function: Box::new(move |ctx| Box::pin(function(ctx))),
            description: None,
        }
    }
}

/// Available task statuses.
#[derive(Debug, PartialEq, Default, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

/// The observable status of a single step within a task run.
///
/// Reflects the most recently completed run: before the first run every step is
/// [`StepStatus::Pending`].
#[derive(Debug, PartialEq, Eq, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum StepStatus {
    /// The step has not run yet in the current cycle.
    #[default]
    Pending,
    /// The step completed successfully.
    Succeeded,
    /// The step completed but reported non-fatal errors.
    HadErrors,
    /// The step failed (after exhausting any retries) or timed out.
    Failed,
    /// The step did not run because an earlier step failed.
    Skipped,
}

/// A read-only view of a single step's identity and last-run outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StepState {
    /// The step's zero-based position within the task.
    pub index: usize,
    /// The step's optional description.
    pub description: Option<String>,
    /// The step's status as of the last completed run.
    pub status: StepStatus,
}

/// The outcome of a single task run, recorded in the task's [`RunRecord`] history.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RunOutcome {
    /// Every step succeeded.
    Success,
    /// The run completed but at least one step reported non-fatal errors.
    HadErrors,
    /// A step failed after exhausting any retries.
    Failed,
    /// A step requested force-removal of the task.
    ForceRemoved,
}

/// A record of one execution of a task, kept in a bounded per-task history.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RunRecord {
    /// A per-task monotonically increasing run identifier.
    pub run_id: usize,
    /// When the run started (UTC).
    pub started_at: DateTime<Utc>,
    /// When the run finished (UTC); `None` while the run is still in progress.
    pub finished_at: Option<DateTime<Utc>>,
    /// The run's outcome; `None` while the run is still in progress.
    pub outcome: Option<RunOutcome>,
    /// The total number of step attempts made during the run (including retries).
    pub attempts: usize,
}

/// What to do when a task's next scheduled time arrives while a previous run of
/// the same task is still in progress.
///
/// Concurrent overlapping runs are intentionally not offered: task steps are
/// `FnMut` (they own mutable state), so a second simultaneous invocation would alias
/// that state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlapPolicy {
    /// Skip the occurrence; the task keeps running and resumes on its next future
    /// slot. This is the safe default.
    #[default]
    Skip,
    /// Run the missed occurrence once the current run finishes (at most one queued).
    Queue,
}

/// A command sent from the scheduler to a running task.
#[derive(Debug)]
pub(crate) enum TaskCmd {
    /// Execute the task once now.
    Run,
}

/// State a running task publishes so the scheduler can observe it and decide when
/// to dispatch the next run without a blocking request/response round-trip.
#[derive(Debug)]
pub(crate) struct TaskShared {
    /// True while a run is in progress.
    pub(crate) running: AtomicBool,
    /// Set when a due occurrence was missed while running (used by [`OverlapPolicy::Queue`]).
    pub(crate) pending: AtomicBool,
    /// Set once the task reaches a terminal state so the scheduler reaps it.
    pub(crate) finished: AtomicBool,
    /// Set while the task is paused; a paused task is not dispatched.
    pub(crate) paused: AtomicBool,
    /// Set when a caller has requested removal; the scheduler reaps the task next round.
    pub(crate) remove_requested: AtomicBool,
    /// Total number of runs started over the task's lifetime.
    pub(crate) runs: AtomicUsize,
    /// The next run id to hand out.
    run_seq: AtomicUsize,
    /// The maximum number of [`RunRecord`]s retained in `history`.
    history_limit: usize,
    /// The observable lifecycle state (status + next execution time, in UTC).
    pub(crate) state: Mutex<SharedState>,
    /// The per-step state as of the last completed run.
    pub(crate) steps: Mutex<Vec<StepState>>,
    /// A bounded history of recent runs, oldest first.
    pub(crate) history: Mutex<VecDeque<RunRecord>>,
}

/// The observable, timezone-erased portion of a task's state.
#[derive(Debug, Clone)]
pub(crate) struct SharedState {
    /// The task's current lifecycle status.
    pub(crate) status: Status,
    /// The task's next execution time, normalized to UTC (`None` if not scheduled).
    pub(crate) next_exec: Option<DateTime<Utc>>,
}

impl TaskShared {
    /// Create a fresh shared-state cell for an uninitialized task.
    ///
    /// `history_limit` bounds how many [`RunRecord`]s are retained.
    pub(crate) fn new(history_limit: usize) -> Self {
        TaskShared {
            running: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            remove_requested: AtomicBool::new(false),
            runs: AtomicUsize::new(0),
            run_seq: AtomicUsize::new(0),
            history_limit,
            state: Mutex::new(SharedState {
                status: Status::Init,
                next_exec: None,
            }),
            steps: Mutex::new(Vec::new()),
            history: Mutex::new(VecDeque::new()),
        }
    }

    /// Record the start of a run and return its run id. Trims the history to the
    /// configured limit.
    fn start_run(&self, started_at: DateTime<Utc>) -> usize {
        let run_id = self.run_seq.fetch_add(1, Ordering::SeqCst);
        self.runs.fetch_add(1, Ordering::SeqCst);
        let mut history = self.history.lock().unwrap();
        while history.len() >= self.history_limit && !history.is_empty() {
            history.pop_front();
        }
        // A `history_limit` of zero disables history entirely.
        if self.history_limit > 0 {
            history.push_back(RunRecord {
                run_id,
                started_at,
                finished_at: None,
                outcome: None,
                attempts: 0,
            });
        }
        run_id
    }

    /// Record the completion of the run with the given id.
    fn finish_run(
        &self,
        run_id: usize,
        finished_at: DateTime<Utc>,
        outcome: RunOutcome,
        attempts: usize,
    ) {
        let mut history = self.history.lock().unwrap();
        if let Some(record) = history.iter_mut().rev().find(|r| r.run_id == run_id) {
            record.finished_at = Some(finished_at);
            record.outcome = Some(outcome);
            record.attempts = attempts;
        }
    }
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
    /// (Optional) caller-supplied unique name used to address the task at runtime.
    pub(crate) name: Option<String>,
    /// The timezone of the task.
    pub(crate) timezone: T,
    /// (Internal) task id.
    pub(crate) task_id: usize,
    /// (Internal) per-step state of the last run, republished after every run.
    pub(crate) step_states: Vec<StepState>,
    /// (Internal) shared cell to publish step-state transitions to as they happen, so
    /// observers see live progress within a run rather than only the final snapshot.
    /// Set by the scheduler when the task is spawned; `None` when the task is driven
    /// directly (e.g. in unit tests), in which case streaming is a no-op.
    pub(crate) step_sink: Option<Arc<TaskShared>>,
    /// (Internal) total step attempts made during the last run.
    pub(crate) last_attempts: usize,
    /// The maximum number of run records retained for this task.
    pub(crate) history_limit: usize,
    /// (Internal) next execution time.
    pub(crate) next_exec: Option<DateTime<T>>,
    /// (Internal) task status.
    pub(crate) status: Status,
    /// Task receiver
    pub(crate) receiver: Option<mpsc::Receiver<TaskCmd>>,
    /// (Optional) per-step execution timeout. A step exceeding it is cancelled and
    /// treated as a (retryable) failure.
    pub(crate) timeout: Option<Duration>,
    /// (Optional) retry policy applied to failing steps.
    pub(crate) retry_policy: Option<RetryPolicy>,
    /// (Optional) callback invoked after a successful execution.
    pub(crate) on_success: Option<Box<CallbackFn>>,
    /// (Optional) callback invoked after a failed execution.
    pub(crate) on_failure: Option<Box<CallbackFn>>,
    /// (Optional) callback invoked once when the task reaches a terminal state.
    pub(crate) on_finish: Option<Box<CallbackFn>>,
    /// Behaviour when a scheduled run overlaps a still-running one.
    pub(crate) overlap: OverlapPolicy,
    /// The task-level shared store handed to every step through its [`TaskContext`].
    /// Defaults to a fresh, empty [`Blackboard`]; attach a shared one via
    /// [`TaskBuilder::blackboard`](crate::TaskBuilder::blackboard) to pass data across
    /// tasks.
    pub(crate) blackboard: Blackboard,
}

impl<T> Debug for Task<T>
where
    T: TimeZone + Send + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("task_id", &self.task_id)
            .field("description", &self.description)
            .field("status", &self.status)
            .field("repeats", &self.repeats)
            .field("next_exec", &self.next_exec)
            .field("timeout", &self.timeout)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

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
    ) -> TaskResult<Task<T>> {
        // Parse the schedule with proper error handling
        let schedule = expression.parse().map_err(|e| {
            TaskError::InvalidCronExpression(format!("Invalid cron expression: {}", e))
        })?;

        Ok(Task {
            steps: Vec::new(),
            schedule,
            description: match description {
                Some(s) => s.to_string(),
                None => "-".to_string(),
            },
            name: None,
            repeats,
            timezone,
            task_id: 0,
            step_states: Vec::new(),
            step_sink: None,
            last_attempts: 0,
            history_limit: DEFAULT_HISTORY_LIMIT,
            status: Status::default(),
            next_exec: None,
            receiver: None,
            timeout: None,
            retry_policy: None,
            on_success: None,
            on_failure: None,
            on_finish: None,
            overlap: OverlapPolicy::default(),
            blackboard: Blackboard::new(),
        })
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
    /// let mut t = Task::new("* * * * * *", None, None, Utc).unwrap();
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
    pub(crate) fn add_step<F, Fut>(&mut self, description: &str, function: F) -> &mut Task<T>
    where
        F: (FnMut(TaskContext) -> Fut) + 'static + Send,
        Fut: Future<Output = Result<TaskStepStatusOk, TaskStepStatusErr>> + Send + 'static,
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
    pub(crate) fn add_step_default<F, Fut>(&mut self, function: F) -> &mut Task<T>
    where
        F: (FnMut(TaskContext) -> Fut) + 'static + Send,
        Fut: Future<Output = Result<TaskStepStatusOk, TaskStepStatusErr>> + Send + 'static,
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

    /// Set the per-step execution timeout.
    pub(crate) fn set_timeout(&mut self, timeout: Duration) -> &mut Task<T> {
        self.timeout = Some(timeout);
        self
    }

    /// Set the retry policy for failing steps.
    pub(crate) fn set_retry_policy(&mut self, policy: RetryPolicy) -> &mut Task<T> {
        self.retry_policy = Some(policy);
        self
    }

    /// Set the overlap policy.
    pub(crate) fn set_overlap(&mut self, overlap: OverlapPolicy) -> &mut Task<T> {
        self.overlap = overlap;
        self
    }

    /// Set the task's unique name.
    pub(crate) fn set_name(&mut self, name: &str) -> &mut Task<T> {
        self.name = Some(name.to_string());
        self
    }

    /// Set the maximum number of run records retained for this task.
    pub(crate) fn set_history_limit(&mut self, limit: usize) -> &mut Task<T> {
        self.history_limit = limit;
        self
    }

    /// Attach a task-level [`Blackboard`], shared with every step through its
    /// [`TaskContext`].
    pub(crate) fn set_blackboard(&mut self, blackboard: Blackboard) -> &mut Task<T> {
        self.blackboard = blackboard;
        self
    }

    /// Rebuild the per-step snapshot from the current steps, marking every step
    /// [`StepStatus::Pending`].
    fn reset_step_states(&mut self) {
        self.step_states = self
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| StepState {
                index,
                description: step.description.clone(),
                status: StepStatus::Pending,
            })
            .collect();
    }

    /// Publish the current per-step snapshot to the shared cell, if a sink is set, so
    /// observers see step transitions live during a run. A no-op when no sink is set.
    fn publish_steps(&self) {
        if let Some(shared) = &self.step_sink {
            *shared.steps.lock().unwrap() = self.step_states.clone();
        }
    }

    /// Set the callbacks invoked over the task's lifecycle.
    pub(crate) fn set_callbacks(
        &mut self,
        on_success: Option<Box<CallbackFn>>,
        on_failure: Option<Box<CallbackFn>>,
        on_finish: Option<Box<CallbackFn>>,
    ) -> &mut Task<T> {
        self.on_success = on_success;
        self.on_failure = on_failure;
        self.on_finish = on_finish;
        self
    }

    /// Invoke a lifecycle callback if it is set.
    async fn fire_callback(callback: &mut Option<Box<CallbackFn>>) {
        if let Some(callback) = callback.as_mut() {
            callback().await;
        }
    }

    /// Initialize the `Task` instance and schedule the first execution.
    ///
    /// # Arguments
    ///
    /// * id - The task's id.
    pub(crate) fn init(&mut self) {
        task_log!(self.task_id, log::Level::Debug, "Initializing");
        // Seed the step snapshot so observers see the step list before the first run.
        self.reset_step_states();
        match self.schedule.upcoming(self.timezone.clone()).next() {
            Some(next) => {
                self.next_exec = Some(next);
                self.status = Status::Scheduled;
                task_log!(self.task_id, log::Level::Debug, "Finished initializing");
            }
            None => {
                // The schedule has no upcoming execution (e.g. a cron with a fixed
                // past year). Mark it finished so the scheduler reaps it instead of
                // panicking on an unwrap.
                self.status = Status::Finished;
                task_log!(
                    self.task_id,
                    log::Level::Warn,
                    "Has no upcoming execution and will be removed"
                );
            }
        }
    }

    /// Run the task's steps and fire the success/failure callbacks based on the
    /// outcome. `on_finish` is fired here only for the terminal `ForceRemoved` state
    /// (a normally-finishing task fires it from [`Task::reschedule_and_notify`]).
    pub(crate) async fn run_and_notify(&mut self, run_id: usize) {
        let _ = self.run_task(run_id).await; // status is updated in place; the result is redundant
        match self.status {
            Status::Executed => Self::fire_callback(&mut self.on_success).await,
            Status::Failed => Self::fire_callback(&mut self.on_failure).await,
            Status::ForceRemoved => {
                Self::fire_callback(&mut self.on_failure).await;
                Self::fire_callback(&mut self.on_finish).await;
            }
            _ => {}
        }
    }

    /// Reschedule the task and fire `on_finish` if it has just reached the
    /// `Finished` state (exactly once; force-removed tasks fire it from
    /// [`Task::run_and_notify`]).
    pub(crate) async fn reschedule_and_notify(&mut self) {
        let _ = self.reschedule(); // status is updated in place; the result is redundant
        if self.status == Status::Finished {
            Self::fire_callback(&mut self.on_finish).await;
        }
    }

    /// Whether the task has reached a terminal state and should be reaped.
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self.status, Status::Finished | Status::ForceRemoved)
    }

    /// Classify the outcome of the run that just completed, based on the status and
    /// the per-step snapshot. Call this after `run_and_notify` and before
    /// `reschedule`, which would otherwise overwrite the status.
    pub(crate) fn run_outcome(&self) -> RunOutcome {
        match self.status {
            Status::ForceRemoved => RunOutcome::ForceRemoved,
            Status::Failed => RunOutcome::Failed,
            _ => {
                if self
                    .step_states
                    .iter()
                    .any(|s| s.status == StepStatus::HadErrors)
                {
                    RunOutcome::HadErrors
                } else {
                    RunOutcome::Success
                }
            }
        }
    }

    /// The next execution time normalized to UTC, for the observable snapshot.
    pub(crate) fn next_exec_utc(&self) -> Option<DateTime<Utc>> {
        self.next_exec.as_ref().map(|d| d.with_timezone(&Utc))
    }

    /// Run the task and handle the output.
    ///
    /// `run_id` identifies the current run and is surfaced to each step through its
    /// [`TaskContext`].
    pub(crate) async fn run_task(&mut self, run_id: usize) -> TaskResult<()> {
        match &self.status {
            Status::Init => Err(TaskError::NotInitialized),
            Status::Failed => Err(TaskError::Failed),
            Status::Executed => Err(TaskError::AlreadyExecuted),
            Status::Finished => Err(TaskError::Finished),
            Status::ForceRemoved => Err(TaskError::ForceRemoved),
            Status::Scheduled => {
                task_log!(
                    self.task_id,
                    log::Level::Debug,
                    "Executing '{}'",
                    self.description
                );
                // Snapshot the timeout / retry configuration before borrowing
                // `self.steps` mutably below.
                let timeout = self.timeout;
                let retry = self.retry_policy.clone();
                let max_attempts = 1 + retry.as_ref().map(|r| r.max_retries).unwrap_or(0);

                // Rebuild the per-step snapshot for this run and reset the attempt count.
                self.reset_step_states();
                self.last_attempts = 0;
                // Stream the fresh (all-pending) snapshot so observers see the run begin.
                self.publish_steps();

                // A fresh per-run store for step-to-step data flow, plus the task
                // identity captured once so building each attempt's context is cheap.
                let run_store = Blackboard::new();
                let task_name: Option<Arc<str>> = self.name.as_deref().map(Arc::from);
                let task_id = self.task_id;
                let blackboard = self.blackboard.clone();

                let mut had_error: bool = false;
                // Iterate by index so `self` is free between step invocations, letting us
                // stream each transition to the shared cell as it happens.
                for index in 0..self.steps.len() {
                    if had_error {
                        break;
                    }
                    // The description is fixed per step; capture it for logging so we do
                    // not hold a borrow of `self.steps` across the awaited step future.
                    let step_desc = self.steps[index].to_string();
                    // Attempt the step, retrying transient (`Error`) failures and
                    // timeouts according to the retry policy. `ErrorDelete` and
                    // success both terminate the attempt loop immediately.
                    let mut attempt: u32 = 0;
                    loop {
                        attempt += 1;
                        self.last_attempts += 1;

                        // The context handed to the step for this attempt. Cheap to
                        // build: the stores are `Arc`-backed clones and the name is a
                        // shared `Arc<str>`.
                        let ctx = TaskContext {
                            task_id,
                            task_name: task_name.clone(),
                            run_id,
                            step_index: index,
                            attempt,
                            blackboard: blackboard.clone(),
                            run_store: run_store.clone(),
                        };

                        // Run the step, optionally bounded by the configured timeout.
                        // A timeout is treated as a (retryable) `Error`. Producing the
                        // step future borrows `self.steps[index]` only until it returns;
                        // the boxed future is `'static`, so no borrow is held across the
                        // await and `self` is free afterwards.
                        let result = match timeout {
                            Some(duration) => {
                                match tokio::time::timeout(
                                    duration,
                                    (self.steps[index].function)(ctx),
                                )
                                .await
                                {
                                    Ok(result) => result,
                                    Err(_) => {
                                        step_log!(
                                            self.task_id,
                                            index,
                                            log::Level::Warn,
                                            "Timed out after {:?} - {}",
                                            duration,
                                            step_desc
                                        );
                                        Err(TaskStepStatusErr::Error)
                                    }
                                }
                            }
                            None => (self.steps[index].function)(ctx).await,
                        };

                        match result {
                            Ok(status) => {
                                match status {
                                    TaskStepStatusOk::Success => {
                                        step_log!(
                                            self.task_id,
                                            index,
                                            log::Level::Debug,
                                            "Executed successfully - {}",
                                            step_desc
                                        );
                                        self.step_states[index].status = StepStatus::Succeeded;
                                    }
                                    TaskStepStatusOk::HadErrors => {
                                        step_log!(
                                            self.task_id,
                                            index,
                                            log::Level::Debug,
                                            "Executed with non-fatal errors - {}",
                                            step_desc
                                        );
                                        self.step_states[index].status = StepStatus::HadErrors;
                                    }
                                }
                                self.status = Status::Executed;
                                self.publish_steps();
                                break;
                            }
                            Err(TaskStepStatusErr::ErrorDelete) => {
                                step_log!(
                                    self.task_id,
                                    index,
                                    log::Level::Error,
                                    "Execution failed and task is marked for deletion - {}",
                                    step_desc
                                );
                                self.step_states[index].status = StepStatus::Failed;
                                self.status = Status::ForceRemoved;
                                self.publish_steps();
                                had_error = true;
                                break;
                            }
                            Err(TaskStepStatusErr::Error) => {
                                if (attempt as usize) < max_attempts {
                                    // `retry` is guaranteed `Some` here: `max_attempts > 1`
                                    // only when a retry policy is set.
                                    let delay = retry.as_ref().unwrap().jittered_delay(attempt - 1);
                                    step_log!(
                                        self.task_id,
                                        index,
                                        log::Level::Warn,
                                        "Execution failed, retrying (attempt {}/{}) after {:?} - {}",
                                        attempt,
                                        max_attempts,
                                        delay,
                                        step_desc
                                    );
                                    if !delay.is_zero() {
                                        tokio::time::sleep(delay).await;
                                    }
                                    continue;
                                }
                                step_log!(
                                    self.task_id,
                                    index,
                                    log::Level::Error,
                                    "Execution failed - {}",
                                    step_desc
                                );
                                self.step_states[index].status = StepStatus::Failed;
                                self.status = Status::Failed;
                                self.publish_steps();
                                had_error = true;
                                break;
                            }
                        }
                    }
                }
                // Any step still pending was never reached because an earlier step failed.
                for step_state in self.step_states.iter_mut() {
                    if step_state.status == StepStatus::Pending {
                        step_state.status = StepStatus::Skipped;
                    }
                }
                // Stream the final per-step outcome (including any skipped steps).
                self.publish_steps();
                // Avoid underflow in case of a task without steps.
                if self.steps.is_empty() {
                    self.status = Status::Executed
                }

                // Reduce the total executions (if set). Saturate at zero so a task
                // constructed with `repeat(0)` cannot underflow.
                self.repeats = self.repeats.map(|r| r.saturating_sub(1));

                Ok(())
            }
        }
    }

    /// Reschedule the current task instance (if needed).
    pub(crate) fn reschedule(&mut self) -> TaskResult<()> {
        match &self.status {
            Status::Init => Err(TaskError::NotInitialized),
            Status::Failed | Status::Executed => {
                // Determine the next status based on the remaining repeats first: if the
                // task is done there is no need to compute a next execution time.
                self.status = match self.repeats {
                    Some(t) => {
                        if t > 0 {
                            task_log!(self.task_id, log::Level::Debug, "Has been rescheduled");
                            Status::Scheduled
                        } else {
                            task_log!(
                                self.task_id,
                                log::Level::Warn,
                                "Has finished its execution cycle and will be removed"
                            );
                            Status::Finished
                        }
                    }
                    None => Status::Scheduled,
                };
                if self.status == Status::Scheduled {
                    match self.schedule.upcoming(self.timezone.clone()).next() {
                        Some(next) => self.next_exec = Some(next),
                        None => {
                            // No further executions available; retire the task.
                            task_log!(
                                self.task_id,
                                log::Level::Warn,
                                "Has no upcoming execution and will be removed"
                            );
                            self.status = Status::Finished;
                        }
                    }
                }
                Ok(())
            }
            Status::Finished | Status::ForceRemoved => {
                task_log!(
                    self.task_id,
                    log::Level::Warn,
                    "Will be removed from the queue"
                );
                Ok(())
            }
            Status::Scheduled => Ok(()), /* Do nothing, keep silent */
        }
    }
}

/// Publish the task's current status and next execution time to the shared cell
/// the scheduler observes.
fn publish<T>(task: &Task<T>, shared: &TaskShared)
where
    T: TimeZone + Send + 'static,
{
    {
        let mut state = shared.state.lock().unwrap();
        state.status = task.status.clone();
        state.next_exec = task.next_exec_utc();
    }
    *shared.steps.lock().unwrap() = task.step_states.clone();
}

/// Execute the task once, fire its lifecycle callbacks, reschedule it and republish
/// the resulting state. Marks the shared `running` flag around the work and the
/// `finished` flag if the task became terminal.
async fn run_once<T>(task: &mut Task<T>, shared: &TaskShared)
where
    T: TimeZone + Send + 'static,
    <T as TimeZone>::Offset: Send,
{
    let run_id = shared.start_run(Utc::now());
    shared.running.store(true, Ordering::SeqCst);
    task.run_and_notify(run_id).await;
    // Capture the outcome before `reschedule` overwrites the status.
    let outcome = task.run_outcome();
    task.reschedule_and_notify().await;
    shared.running.store(false, Ordering::SeqCst);
    shared.finish_run(run_id, Utc::now(), outcome, task.last_attempts);
    if task.is_terminal() {
        shared.finished.store(true, Ordering::SeqCst);
    }
    publish(task, shared);
}

/// Drive a `Task` in its own Tokio task.
///
/// The task initializes itself, publishes its state to `shared`, then runs once for
/// each [`TaskCmd::Run`] the scheduler dispatches. Runs never block the scheduler:
/// the scheduler fire-and-forgets `Run` and observes progress through `shared`. When
/// a run is queued via [`OverlapPolicy::Queue`], it is drained here once the current
/// run finishes.
pub(crate) async fn run_task<T>(mut task: Task<T>, shared: Arc<TaskShared>)
where
    T: TimeZone + Send + 'static,
    <T as TimeZone>::Offset: Send,
{
    // Wire the shared cell as the step-state sink so step transitions stream live
    // during each run, then self-initialize and publish the first scheduled time.
    task.step_sink = Some(shared.clone());
    task.init();
    publish(&task, &shared);
    if task.is_terminal() {
        shared.finished.store(true, Ordering::SeqCst);
        return;
    }

    let mut receiver = task
        .receiver
        .take()
        .expect("run_task requires a receiver to be set");

    while let Some(TaskCmd::Run) = receiver.recv().await {
        run_once(&mut task, &shared).await;
        if shared.finished.load(Ordering::SeqCst) {
            break;
        }
        // `OverlapPolicy::Queue`: run any occurrence that came due while we were
        // busy, one at a time, until none remain or the task retires.
        while shared.pending.swap(false, Ordering::SeqCst) && !task.is_terminal() {
            run_once(&mut task, &shared).await;
            if shared.finished.load(Ordering::SeqCst) {
                break;
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::task::TaskStepStatusErr::{Error, ErrorDelete};
    use crate::task::TaskStepStatusOk::Success;
    use chrono::prelude::*;

    #[tokio::test]
    async fn normal_task_flow_test() {
        let mut task = Task::new("* * * * * *", Some("Test task"), Some(2), Local).unwrap();
        task.add_step_default(|_ctx| async { Ok(Success) });
        assert_eq!(task.status, Status::Init);
        task.set_id(0);
        task.init();
        assert_eq!(task.status, Status::Scheduled);
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Executed);
        assert!(task.reschedule().is_ok());
        assert_eq!(task.status, Status::Scheduled);
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Executed);
        assert!(task.reschedule().is_ok());
        assert_eq!(task.status, Status::Finished);
    }

    #[test]
    fn test_task_set_schedule() {
        let schedule: Schedule = "* * * * * * *".parse().unwrap();
        let mut task = Task::new("* * * * * * *", None, None, Local).unwrap();
        task.set_schedule(schedule);
        task.add_step_default(|_ctx| async { Ok(Success) });
        assert_eq!(task.status, Status::Init);
        task.set_id(0);
        task.init();
        assert_eq!(task.status, Status::Scheduled);
    }

    #[tokio::test]
    async fn normal_task_error_flow_test() {
        let mut task = Task::new("* * * * * *", Some("Test task"), Some(2), Local).unwrap();
        task.add_step_default(|_ctx| async { Err(Error) });
        assert_eq!(task.status, Status::Init);
        task.set_id(0);
        task.init();
        assert_eq!(task.status, Status::Scheduled);
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Failed);
        assert!(task.reschedule().is_ok());
        assert_eq!(task.status, Status::Scheduled);
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Failed);
        assert!(task.reschedule().is_ok());
        assert_eq!(task.status, Status::Finished);
    }

    /// Test the normal execution of a simple task, without fixed repeats.
    #[tokio::test]
    async fn normal_task_no_fixed_repeats_test() {
        let mut task = Task::new("* * * * * * *", Some("Test task"), None, Local).unwrap();
        task.add_step_default(|_ctx| async { Ok(Success) });
        assert_eq!(task.status, Status::Init);
        task.set_id(0);
        task.init();
        assert_eq!(task.status, Status::Scheduled);
        // Run it for a few times.
        for _i in 1..10 {
            assert!(task.run_task(0).await.is_ok());
            assert_eq!(task.status, Status::Executed);
            assert!(task.reschedule().is_ok());
            assert_eq!(task.status, Status::Scheduled);
        }
    }

    #[test]
    fn test_reschedule_not_initialized() {
        let mut task = Task::new("* * * * * * *", None, None, Local).unwrap();
        // This task is not initialized, so it should fail.
        assert!(task.reschedule().is_err());
        assert!(matches!(
            task.reschedule().unwrap_err(),
            TaskError::NotInitialized
        ));
    }

    #[tokio::test]
    async fn test_reschedule_finished_should_mark_as_finished() {
        let mut task = Task::new("* * * * * * *", None, Some(1), Local).unwrap();
        // Execute the task.
        task.set_id(0);
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert!(task.reschedule().is_ok());
        assert_eq!(task.status, Status::Finished);
    }

    #[tokio::test]
    async fn test_run_uninitialized_task() {
        let mut task = Task::new("* * * * * * *", None, None, Local).unwrap();
        assert!(task.run_task(0).await.is_err());
        assert!(matches!(
            task.run_task(0).await.unwrap_err(),
            TaskError::NotInitialized
        ));
    }

    #[tokio::test]
    async fn test_run_failed_task() {
        let mut task = Task::new("* * * * * * *", None, None, Local).unwrap();
        task.add_step_default(|_ctx| async { Err(Error) });
        task.set_id(0);
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Failed);
        // Attempt to rerun it, it should fail.
        assert!(task.run_task(0).await.is_err());
        assert!(matches!(
            task.run_task(0).await.unwrap_err(),
            TaskError::Failed
        ));
    }

    #[tokio::test]
    async fn test_run_executed_task() {
        let mut task = Task::new("* * * * * * *", None, None, Local).unwrap();
        task.add_step("Step 1", |_ctx| async { Ok(Success) });
        task.set_id(0);
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Executed);
        // Attempt to run it again, it should fail.
        assert!(task.run_task(0).await.is_err());
        assert!(matches!(
            task.run_task(0).await.unwrap_err(),
            TaskError::AlreadyExecuted
        ));
    }

    #[tokio::test]
    async fn test_run_finished_task() {
        let mut task = Task::new("* * * * * * *", None, Some(1), Local).unwrap();
        task.set_id(0);
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert!(task.reschedule().is_ok());
        assert_eq!(task.status, Status::Finished);
        // At this point the task is Finished. It should not be allowed to run again.
        assert!(task.run_task(0).await.is_err());
        assert!(matches!(
            task.run_task(0).await.unwrap_err(),
            TaskError::Finished
        ));
    }

    #[tokio::test]
    async fn test_run_failed_delete() {
        let mut task = Task::new("* * * * * * *", None, None, Local).unwrap();
        task.add_step_default(|_ctx| async { Err(ErrorDelete) });
        task.set_id(0);
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::ForceRemoved);
    }

    #[test]
    fn test_invalid_cron_expression() {
        let task = Task::new("invalid expression", None, None, Local);
        assert!(task.is_err());
        assert!(matches!(
            task.unwrap_err(),
            TaskError::InvalidCronExpression(_)
        ));
    }

    #[tokio::test]
    async fn test_task_status_transitions() {
        let mut task = Task::new(
            "* * * * * * *",
            Some("Status transition test"),
            Some(1),
            Local,
        )
        .unwrap();

        // Initial status should be Init
        assert_eq!(task.status, Status::Init);

        // After init(), status should be Scheduled
        task.init();
        assert_eq!(task.status, Status::Scheduled);

        // Add a step that succeeds
        task.add_step_default(|_ctx| async { Ok(TaskStepStatusOk::Success) });

        // After run_task(), status should be Executed
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Executed);

        // After reschedule(), with repeats=1 and already executed once,
        // status should be Finished
        assert!(task.reschedule().is_ok());
        assert_eq!(task.status, Status::Finished);
    }

    #[test]
    fn test_task_step_display() {
        // Test with description
        let step_with_desc =
            TaskStep::new("Test step", |_ctx| async { Ok(TaskStepStatusOk::Success) });
        assert_eq!(format!("{}", step_with_desc), "Test step");

        // Test without description
        let step_no_desc = TaskStep::default(|_ctx| async { Ok(TaskStepStatusOk::Success) });
        assert_eq!(format!("{}", step_no_desc), "-");

        // Test with empty description
        let step_empty_desc = TaskStep::new("", |_ctx| async { Ok(TaskStepStatusOk::Success) });
        assert_eq!(format!("{}", step_empty_desc), "-");
    }

    #[tokio::test]
    async fn test_run_and_reschedule_notify() {
        let mut task = Task::new("* * * * * * *", Some("Notify flow"), Some(1), Utc).unwrap();
        task.set_id(1);
        task.add_step("Test step", |_ctx| async { Ok(TaskStepStatusOk::Success) });

        task.init();
        assert_eq!(task.status, Status::Scheduled);

        // One run drives Scheduled -> Executed.
        task.run_and_notify(0).await;
        assert_eq!(task.status, Status::Executed);

        // With the single repeat spent, rescheduling retires the task.
        task.reschedule_and_notify().await;
        assert_eq!(task.status, Status::Finished);
        assert!(task.is_terminal());
    }

    /// The `run_task` loop initializes, publishes state to the shared cell, runs on a
    /// `Run` command, and marks itself finished when its repeat cycle is exhausted.
    #[tokio::test]
    async fn test_run_task_loop_publishes_and_finishes() {
        let (tx, rx) = mpsc::channel(1);
        let mut task = Task::new("* * * * * * *", Some("Loop"), Some(1), Utc).unwrap();
        task.set_id(3);
        task.add_step_default(|_ctx| async { Ok(Success) });
        task.set_receiver(rx);

        let shared = Arc::new(TaskShared::new(DEFAULT_HISTORY_LIMIT));
        let join = tokio::spawn(run_task(task, shared.clone()));

        // After self-initialization the task publishes Scheduled + a next execution.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        {
            let state = shared.state.lock().unwrap();
            assert_eq!(state.status, Status::Scheduled);
            assert!(state.next_exec.is_some());
        }

        // Dispatch a single run; with repeat(1) the task then retires and the loop ends.
        tx.send(TaskCmd::Run).await.unwrap();
        join.await.unwrap();

        assert!(shared.finished.load(Ordering::SeqCst));
        assert_eq!(shared.state.lock().unwrap().status, Status::Finished);
    }

    #[tokio::test]
    async fn test_task_multiple_steps_execution() {
        use std::sync::{Arc, Mutex};

        // Create counters to track step execution
        let counter1 = Arc::new(Mutex::new(0));
        let counter2 = Arc::new(Mutex::new(0));
        let counter3 = Arc::new(Mutex::new(0));

        // Create a task with multiple steps
        let mut task = Task::new("* * * * * * *", Some("Multiple steps"), Some(1), Local).unwrap();

        // Add steps that increment counters
        let c1 = counter1.clone();
        task.add_step("Step 1", move |_ctx| {
            let c1 = c1.clone();
            async move {
                *c1.lock().unwrap() += 1;
                Ok(TaskStepStatusOk::Success)
            }
        });

        let c2 = counter2.clone();
        task.add_step("Step 2", move |_ctx| {
            let c2 = c2.clone();
            async move {
                *c2.lock().unwrap() += 1;
                Ok(TaskStepStatusOk::Success)
            }
        });

        let c3 = counter3.clone();
        task.add_step("Step 3", move |_ctx| {
            let c3 = c3.clone();
            async move {
                *c3.lock().unwrap() += 1;
                Ok(TaskStepStatusOk::Success)
            }
        });

        // Initialize and run
        task.init();
        assert!(task.run_task(0).await.is_ok());

        // Verify all steps executed
        assert_eq!(*counter1.lock().unwrap(), 1);
        assert_eq!(*counter2.lock().unwrap(), 1);
        assert_eq!(*counter3.lock().unwrap(), 1);

        // Verify final status
        assert_eq!(task.status, Status::Executed);
    }

    #[tokio::test]
    async fn test_task_step_failure_scenarios() {
        use std::sync::{Arc, Mutex};

        // Create a counter to verify which steps executed
        let execution_counter = Arc::new(Mutex::new(Vec::new()));

        // Create a task with three steps, where the second step fails
        let mut task = Task::new("* * * * * * *", Some("Failure test"), None, Local).unwrap();

        // First step succeeds
        let counter = execution_counter.clone();
        task.add_step("Step 1", move |_ctx| {
            let counter = counter.clone();
            async move {
                counter.lock().unwrap().push(1);
                Ok(TaskStepStatusOk::Success)
            }
        });

        // Second step fails
        let counter = execution_counter.clone();
        task.add_step("Step 2", move |_ctx| {
            let counter = counter.clone();
            async move {
                counter.lock().unwrap().push(2);
                Err(TaskStepStatusErr::Error)
            }
        });

        // Third step should not execute due to previous failure
        let counter = execution_counter.clone();
        task.add_step("Step 3", move |_ctx| {
            let counter = counter.clone();
            async move {
                counter.lock().unwrap().push(3);
                Ok(TaskStepStatusOk::Success)
            }
        });

        // Initialize and run
        task.init();
        assert!(task.run_task(0).await.is_ok());

        // Verify only steps 1 and 2 executed
        assert_eq!(*execution_counter.lock().unwrap(), vec![1, 2]);

        // Verify task is in Failed state
        assert_eq!(task.status, Status::Failed);
    }

    #[tokio::test]
    async fn test_force_removal() {
        // Create a task with a step that forces removal
        let mut task = Task::new("* * * * * * *", Some("Force removal"), None, Local).unwrap();
        task.add_step("Failing step", |_ctx| async {
            Err(TaskStepStatusErr::ErrorDelete)
        });

        // Initialize and run
        task.init();
        assert!(task.run_task(0).await.is_ok());

        // Verify task is marked for force removal
        assert_eq!(task.status, Status::ForceRemoved);

        // Verify reschedule respects force removal status
        assert!(task.reschedule().is_ok());
        assert_eq!(task.status, Status::ForceRemoved);
    }

    #[tokio::test]
    async fn test_empty_task_execution() {
        // Create a task with no steps
        let mut task = Task::new("* * * * * * *", Some("Empty task"), None, Local).unwrap();

        // Initialize and run
        task.init();
        assert!(task.run_task(0).await.is_ok());

        // Verify task is marked as Executed even with no steps
        assert_eq!(task.status, Status::Executed);
    }

    /// A step that actually awaits should be driven to completion by `run_task`.
    #[tokio::test]
    async fn test_async_step_is_awaited() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();

        let mut task = Task::new("* * * * * * *", Some("Async step"), Some(1), Utc).unwrap();
        task.add_step("Awaits", move |_ctx| {
            let flag = flag.clone();
            async move {
                // Yield to the runtime to prove the future is actually polled/awaited.
                tokio::task::yield_now().await;
                flag.store(true, Ordering::SeqCst);
                Ok(TaskStepStatusOk::Success)
            }
        });

        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert!(ran.load(Ordering::SeqCst), "async step body must run");
        assert_eq!(task.status, Status::Executed);
    }

    /// A task built with `repeat(0)` must not underflow when executed (B6 regression).
    #[tokio::test]
    async fn test_repeat_zero_does_not_underflow() {
        let mut task = Task::new("* * * * * * *", Some("Zero repeats"), Some(0), Utc).unwrap();
        task.add_step_default(|_ctx| async { Ok(Success) });
        task.init();
        // Would panic with an integer underflow before the `saturating_sub` fix.
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.repeats, Some(0));
        // With no repeats left, the task is retired on reschedule.
        assert!(task.reschedule().is_ok());
        assert_eq!(task.status, Status::Finished);
    }

    /// A step exceeding the configured timeout is cancelled and marks the task failed. (C1)
    #[tokio::test(start_paused = true)]
    async fn test_step_timeout_marks_failed() {
        let mut task = Task::new("* * * * * * *", Some("Timeout"), None, Utc).unwrap();
        task.timeout = Some(Duration::from_millis(50));
        task.add_step_default(|_ctx| async {
            // Sleeps far longer than the timeout; the paused clock auto-advances so
            // the timeout fires without a real wall-clock wait.
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(Success)
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Failed);
    }

    /// A transient failure is retried and the task succeeds once a step returns Ok. (C2)
    #[tokio::test(start_paused = true)]
    async fn test_retry_eventually_succeeds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();

        let mut task = Task::new("* * * * * * *", Some("Retry ok"), None, Utc).unwrap();
        task.retry_policy = Some(RetryPolicy::fixed(3, Duration::from_millis(10)));
        task.add_step_default(move |_ctx| {
            let c = c.clone();
            async move {
                // Fail the first two attempts, succeed on the third.
                if c.fetch_add(1, Ordering::SeqCst) < 2 {
                    Err(Error)
                } else {
                    Ok(Success)
                }
            }
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Executed);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// When the retry budget is exhausted the task is marked failed. (C2)
    #[tokio::test(start_paused = true)]
    async fn test_retry_exhausted_marks_failed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();

        let mut task = Task::new("* * * * * * *", Some("Retry fail"), None, Utc).unwrap();
        task.retry_policy = Some(RetryPolicy::fixed(2, Duration::from_millis(10)));
        task.add_step_default(move |_ctx| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(Error)
            }
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Failed);
        // 1 initial attempt + 2 retries.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// `ErrorDelete` bypasses the retry policy and removes the task immediately. (C2)
    #[tokio::test(start_paused = true)]
    async fn test_error_delete_bypasses_retry() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();

        let mut task = Task::new("* * * * * * *", Some("Delete"), None, Utc).unwrap();
        task.retry_policy = Some(RetryPolicy::fixed(5, Duration::from_millis(10)));
        task.add_step_default(move |_ctx| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(ErrorDelete)
            }
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::ForceRemoved);
        // Called exactly once — no retries.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A timed-out attempt counts as a retryable failure; a later attempt can succeed. (C1 + C2)
    #[tokio::test(start_paused = true)]
    async fn test_timeout_is_retried() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();

        let mut task = Task::new("* * * * * * *", Some("Timeout retry"), None, Utc).unwrap();
        task.timeout = Some(Duration::from_millis(50));
        task.retry_policy = Some(RetryPolicy::fixed(2, Duration::from_millis(0)));
        task.add_step_default(move |_ctx| {
            let c = c.clone();
            async move {
                // First attempt hangs past the timeout; subsequent attempts return quickly.
                if c.fetch_add(1, Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Ok(Success)
            }
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Executed);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Lifecycle callbacks fire for a successful, finishing task. (C6)
    #[tokio::test]
    async fn test_lifecycle_callbacks_success_and_finish() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let success = Arc::new(AtomicUsize::new(0));
        let failure = Arc::new(AtomicUsize::new(0));
        let finish = Arc::new(AtomicUsize::new(0));

        let mut task = Task::new("* * * * * * *", Some("Callbacks"), Some(1), Utc).unwrap();
        task.add_step_default(|_ctx| async { Ok(Success) });

        let s = success.clone();
        task.on_success = Some(boxed_callback(move || {
            let s = s.clone();
            async move {
                s.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let f = failure.clone();
        task.on_failure = Some(boxed_callback(move || {
            let f = f.clone();
            async move {
                f.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let fin = finish.clone();
        task.on_finish = Some(boxed_callback(move || {
            let fin = fin.clone();
            async move {
                fin.fetch_add(1, Ordering::SeqCst);
            }
        }));

        task.set_id(0);
        task.init();

        // A run fires on_success; the follow-up reschedule retires the task (repeat 1)
        // and fires on_finish exactly once.
        task.run_and_notify(0).await;
        task.reschedule_and_notify().await;

        assert_eq!(success.load(Ordering::SeqCst), 1);
        assert_eq!(failure.load(Ordering::SeqCst), 0);
        assert_eq!(finish.load(Ordering::SeqCst), 1);
    }

    /// A force-removed task fires both on_failure and on_finish. (C6)
    #[tokio::test]
    async fn test_lifecycle_callbacks_force_removed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let failure = Arc::new(AtomicUsize::new(0));
        let finish = Arc::new(AtomicUsize::new(0));

        let mut task = Task::new("* * * * * * *", Some("Callbacks delete"), None, Utc).unwrap();
        task.add_step_default(|_ctx| async { Err(ErrorDelete) });

        let f = failure.clone();
        task.on_failure = Some(boxed_callback(move || {
            let f = f.clone();
            async move {
                f.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let fin = finish.clone();
        task.on_finish = Some(boxed_callback(move || {
            let fin = fin.clone();
            async move {
                fin.fetch_add(1, Ordering::SeqCst);
            }
        }));

        task.set_id(0);
        task.init();

        // ForceRemoved is terminal, so run_and_notify fires both on_failure and on_finish.
        task.run_and_notify(0).await;

        assert_eq!(failure.load(Ordering::SeqCst), 1);
        assert_eq!(finish.load(Ordering::SeqCst), 1);
    }

    /// Step states track each step's outcome; a step after a failure is skipped. (Layer 0)
    #[tokio::test]
    async fn test_step_states_track_outcomes() {
        let mut task = Task::new("* * * * * * *", Some("Steps"), Some(1), Utc).unwrap();
        task.add_step("ok", |_ctx| async { Ok(Success) });
        task.add_step("boom", |_ctx| async { Err(Error) });
        task.add_step("never", |_ctx| async { Ok(Success) });
        task.set_id(0);
        task.init();

        // Before the first run every step is pending, with descriptions preserved.
        assert_eq!(task.step_states.len(), 3);
        assert!(task
            .step_states
            .iter()
            .all(|s| s.status == StepStatus::Pending));
        assert_eq!(task.step_states[0].description.as_deref(), Some("ok"));

        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.step_states[0].status, StepStatus::Succeeded);
        assert_eq!(task.step_states[1].status, StepStatus::Failed);
        assert_eq!(task.step_states[2].status, StepStatus::Skipped);
        assert_eq!(task.last_attempts, 2);
    }

    /// A step returning `HadErrors` classifies the run outcome as `HadErrors`. (Layer 0)
    #[tokio::test]
    async fn test_run_outcome_had_errors() {
        let mut task = Task::new("* * * * * * *", None, Some(1), Utc).unwrap();
        task.add_step_default(|_ctx| async { Ok(TaskStepStatusOk::HadErrors) });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.step_states[0].status, StepStatus::HadErrors);
        assert_eq!(task.run_outcome(), RunOutcome::HadErrors);
    }

    /// Run history records each run's outcome and is bounded by the history limit. (Layer 0)
    #[tokio::test]
    async fn test_run_history_records_and_caps() {
        let mut task = Task::new("* * * * * * *", Some("Hist"), None, Utc).unwrap();
        task.set_id(0);
        task.set_history_limit(2);
        task.add_step_default(|_ctx| async { Ok(Success) });
        task.init();

        let shared = TaskShared::new(task.history_limit);
        for _ in 0..3 {
            run_once(&mut task, &shared).await;
        }

        let hist = shared.history.lock().unwrap().clone();
        // Only the two most recent runs are retained, oldest first.
        assert_eq!(hist.len(), 2);
        assert_eq!(hist.front().unwrap().run_id, 1);
        assert_eq!(hist.back().unwrap().run_id, 2);
        assert!(hist
            .iter()
            .all(|r| r.outcome == Some(RunOutcome::Success) && r.finished_at.is_some()));
        // The lifetime counter is not bounded by the retained history.
        assert_eq!(shared.runs.load(Ordering::SeqCst), 3);
        // The per-step snapshot is published to the shared cell.
        assert_eq!(
            shared.steps.lock().unwrap()[0].status,
            StepStatus::Succeeded
        );
    }

    /// A `history_limit` of zero disables run history but still counts runs. (Layer 0)
    #[tokio::test]
    async fn test_history_limit_zero_disables_history() {
        let mut task = Task::new("* * * * * * *", None, None, Utc).unwrap();
        task.set_history_limit(0);
        task.add_step_default(|_ctx| async { Ok(Success) });
        task.init();

        let shared = TaskShared::new(task.history_limit);
        run_once(&mut task, &shared).await;

        assert!(shared.history.lock().unwrap().is_empty());
        assert_eq!(shared.runs.load(Ordering::SeqCst), 1);
    }

    /// Step-state transitions are streamed to the shared cell during a run, so an
    /// observer sees a completed step while a later step is still pending. (§4.3)
    #[tokio::test]
    async fn test_step_states_stream_live() {
        use tokio::sync::Notify;

        let ready = Arc::new(Notify::new());
        let proceed = Arc::new(Notify::new());

        let (tx, rx) = mpsc::channel(1);
        let mut task = Task::new("* * * * * * *", None, Some(1), Utc).unwrap();
        task.set_id(0);
        // Step 0 succeeds immediately.
        task.add_step("first", |_ctx| async { Ok(Success) });
        // Step 1 signals that it has been reached, then waits for permission to finish.
        let r = ready.clone();
        let p = proceed.clone();
        task.add_step("second", move |_ctx| {
            let r = r.clone();
            let p = p.clone();
            async move {
                r.notify_one();
                p.notified().await;
                Ok(Success)
            }
        });
        task.set_receiver(rx);

        let shared = Arc::new(TaskShared::new(DEFAULT_HISTORY_LIMIT));
        let join = tokio::spawn(run_task(task, shared.clone()));

        // Dispatch a run and wait until step 1 is mid-flight.
        tx.send(TaskCmd::Run).await.unwrap();
        ready.notified().await;

        // Live snapshot: step 0 already succeeded while step 1 is still pending.
        {
            let steps = shared.steps.lock().unwrap();
            assert_eq!(steps[0].status, StepStatus::Succeeded);
            assert_eq!(steps[1].status, StepStatus::Pending);
        }

        // Let step 1 finish; the run completes and, with repeat(1), the task retires.
        proceed.notify_one();
        join.await.unwrap();

        let steps = shared.steps.lock().unwrap();
        assert_eq!(steps[0].status, StepStatus::Succeeded);
        assert_eq!(steps[1].status, StepStatus::Succeeded);
    }

    /// The observable state types round-trip through serde. (Layer 0, `serde` feature)
    #[cfg(feature = "serde")]
    #[test]
    fn test_serde_roundtrip_observability_types() {
        let step = StepState {
            index: 0,
            description: Some("s".to_string()),
            status: StepStatus::Succeeded,
        };
        let json = serde_json::to_string(&step).unwrap();
        assert_eq!(serde_json::from_str::<StepState>(&json).unwrap(), step);

        let status = Status::Scheduled;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), status);

        let record = RunRecord {
            run_id: 3,
            started_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            finished_at: Some(Utc.timestamp_opt(1_700_000_001, 0).unwrap()),
            outcome: Some(RunOutcome::HadErrors),
            attempts: 2,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(serde_json::from_str::<RunRecord>(&json).unwrap(), record);
    }

    /// A step's context reports the task id/name, run id, step index and attempt. (0.5.0)
    #[tokio::test]
    async fn test_context_reports_identity() {
        let seen = Arc::new(Mutex::new(
            Vec::<(usize, Option<String>, usize, usize, u32)>::new(),
        ));
        let s = seen.clone();
        let mut task = Task::new("* * * * * *", None, Some(1), Local).unwrap();
        task.set_name("ctx-task");
        task.set_id(7);
        task.add_step("first", move |ctx| {
            let s = s.clone();
            async move {
                s.lock().unwrap().push((
                    ctx.task_id(),
                    ctx.task_name().map(str::to_string),
                    ctx.run_id(),
                    ctx.step_index(),
                    ctx.attempt(),
                ));
                Ok(Success)
            }
        });
        task.init();
        // Run with an explicit run id, as the scheduler would.
        assert!(task.run_task(42).await.is_ok());

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0],
            (7, Some("ctx-task".to_string()), 42, 0, 1),
            "context should carry task id/name, run id, step index and attempt"
        );
    }

    /// The per-run store hands values from one step to a later step within the same run,
    /// and does not leak into the next run. (0.5.0)
    #[tokio::test]
    async fn test_run_store_passes_data_between_steps() {
        let observed = Arc::new(Mutex::new(Vec::<Option<u32>>::new()));
        let mut task = Task::new("* * * * * *", None, Some(2), Local).unwrap();
        task.set_id(0);
        // First step writes a value keyed by the current run id.
        task.add_step("produce", |ctx| async move {
            ctx.run_store().set("value", ctx.run_id() as u32 + 1);
            Ok(Success)
        });
        // Second step reads what the first step wrote.
        let o = observed.clone();
        task.add_step("consume", move |ctx| {
            let o = o.clone();
            async move {
                o.lock().unwrap().push(ctx.run_store().get::<u32>("value"));
                Ok(Success)
            }
        });
        task.init();

        // Two runs with distinct run ids; each run gets a fresh store.
        assert!(task.run_task(0).await.is_ok());
        assert!(task.reschedule().is_ok());
        assert!(task.run_task(1).await.is_ok());

        let observed = observed.lock().unwrap();
        assert_eq!(
            *observed,
            vec![Some(1), Some(2)],
            "each run's store is fresh and visible to later steps in that run"
        );
    }

    /// A blackboard attached to the task is visible to every step through its context and
    /// persists across runs. (0.5.0)
    #[tokio::test]
    async fn test_context_blackboard_persists_across_runs() {
        let board = Blackboard::new();
        let mut task = Task::new("* * * * * *", None, Some(3), Local).unwrap();
        task.set_id(0);
        task.set_blackboard(board.clone());
        task.add_step("count", |ctx| async move {
            let n: u32 = ctx.blackboard().get_or_insert("runs", 0);
            ctx.blackboard().set("runs", n + 1);
            Ok(Success)
        });
        task.init();

        for run_id in 0..3 {
            assert!(task.run_task(run_id).await.is_ok());
            let _ = task.reschedule();
        }

        // The externally-held blackboard reflects every run.
        assert_eq!(board.get::<u32>("runs"), Some(3));
    }

    /// The context's attempt counter increments as a failing step is retried. (0.5.0)
    #[tokio::test]
    async fn test_context_attempt_increments_on_retry() {
        use crate::retry::RetryPolicy;
        let attempts = Arc::new(Mutex::new(Vec::<u32>::new()));
        let a = attempts.clone();
        let mut task = Task::new("* * * * * *", None, Some(1), Local).unwrap();
        task.set_id(0);
        // Fail the first two attempts (recording each attempt number), succeed on the third.
        task.set_retry_policy(RetryPolicy::fixed(2, Duration::from_millis(0)));
        task.add_step("flaky", move |ctx| {
            let a = a.clone();
            async move {
                a.lock().unwrap().push(ctx.attempt());
                if ctx.attempt() < 3 {
                    Err(Error)
                } else {
                    Ok(Success)
                }
            }
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::Executed);
        assert_eq!(*attempts.lock().unwrap(), vec![1, 2, 3]);
    }
}

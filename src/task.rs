extern crate chrono;
extern crate cron;

use crate::blackboard::Blackboard;
use crate::cancel::CancelToken;
use crate::errors::{TaskError, TaskResult};
use crate::events::{SchedulerEvent, EVENT_CAPACITY};
use crate::limits::Limiter;
use crate::retry::RetryPolicy;
use crate::{step_log, task_log};
use chrono::TimeZone;
use chrono::{DateTime, Utc};
use cron::Schedule;
use std::any::Any;
use std::collections::VecDeque;
use std::fmt::{self, Debug};
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Notify};

/// The default number of past [`RunRecord`]s kept per task.
pub(crate) const DEFAULT_HISTORY_LIMIT: usize = 20;

/// The smallest grace period [`MissedRunPolicy::Skip`] applies. Every dispatch starts a
/// little after its slot, and cron has one-second resolution, so a shorter grace
/// would skip on-time runs.
pub(crate) const MIN_MISSED_RUN_GRACE: Duration = Duration::from_secs(1);

/// Possible success status values for a step's execution.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskStepStatusOk {
    /// The step was a success, move to the next one (or exit if last).
    Success,
    /// The step execution had errors but can continue the execution.
    HadErrors,
}

/// Possible error status values for a step's execution.
///
/// Both variants carry a human-readable reason. It is stored in the step's
/// [`StepState::error`] and the run's [`RunRecord::error`], and reaches the
/// [`on_failure`](crate::TaskBuilder::on_failure) callback through its [`RunReport`].
///
/// # Examples
///
/// ```
/// use tasklet::task::TaskStepStatusErr;
///
/// let retryable = TaskStepStatusErr::error("database unavailable");
/// assert_eq!(retryable.reason(), "database unavailable");
/// assert!(!retryable.is_fatal());
///
/// let fatal = TaskStepStatusErr::error_delete("configuration removed");
/// assert!(fatal.is_fatal());
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum TaskStepStatusErr {
    /// The step failed. It is retried under the task's [`RetryPolicy`], if any; after
    /// the last attempt the run fails and the task stays scheduled.
    Error(String),
    /// The step failed and the task has to be removed from the scheduler. Retries are
    /// skipped.
    ErrorDelete(String),
}

impl TaskStepStatusErr {
    /// A retryable failure with the given reason.
    pub fn error(reason: impl Into<String>) -> Self {
        TaskStepStatusErr::Error(reason.into())
    }

    /// A fatal failure with the given reason; the task is removed.
    pub fn error_delete(reason: impl Into<String>) -> Self {
        TaskStepStatusErr::ErrorDelete(reason.into())
    }

    /// The failure's reason.
    pub fn reason(&self) -> &str {
        match self {
            TaskStepStatusErr::Error(reason) | TaskStepStatusErr::ErrorDelete(reason) => reason,
        }
    }

    /// Whether the failure removes the task ([`ErrorDelete`](Self::ErrorDelete)).
    pub fn is_fatal(&self) -> bool {
        matches!(self, TaskStepStatusErr::ErrorDelete(_))
    }
}

impl fmt::Display for TaskStepStatusErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
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
/// A `TaskContext` identifies the run (which task, which run, which step, which attempt),
/// tells the step whether it should stop early, and gives it access to two typed
/// key/value stores:
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
    cancel: CancelToken,
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

    /// Whether the task is being stopped, because it was removed through
    /// [`SchedulerHandle::remove`](crate::SchedulerHandle::remove) or the scheduler is
    /// shutting down.
    ///
    /// A long-running step should check this (or await [`cancelled`](Self::cancelled))
    /// and return early. The scheduler waits for the step up to its
    /// [grace period](crate::TaskScheduler::grace_period) and aborts it after that.
    /// Remaining steps of a cancelled run do not start.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Resolve once the task is being stopped. Use it in `tokio::select!` to interrupt
    /// a long wait.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use tasklet::task::TaskStepStatusErr;
    /// use tasklet::task::TaskStepStatusOk::Success;
    /// use tasklet::TaskBuilder;
    ///
    /// let _task = TaskBuilder::new(chrono::Utc)
    ///     .every_minutes(5)
    ///     .add_step("long poll", |ctx| async move {
    ///         tokio::select! {
    ///             _ = tokio::time::sleep(Duration::from_secs(60)) => Ok(Success),
    ///             _ = ctx.cancelled() => Err(TaskStepStatusErr::error("stopped")),
    ///         }
    ///     })
    ///     .build();
    /// ```
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await
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
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// The boxed future produced by a lifecycle callback when it is invoked.
pub type CallbackFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// An asynchronous lifecycle callback taking an argument of type `A`.
///
/// [`on_success`](crate::TaskBuilder::on_success) and
/// [`on_failure`](crate::TaskBuilder::on_failure) callbacks receive a [`RunReport`];
/// [`on_finish`](crate::TaskBuilder::on_finish) callbacks receive a [`FinishReport`].
pub type CallbackFn<A> = dyn FnMut(A) -> CallbackFuture + 'static + Send;

/// Box an async closure into a [`CallbackFn`].
pub(crate) fn boxed_callback<A, F, Fut>(mut callback: F) -> Box<CallbackFn<A>>
where
    F: (FnMut(A) -> Fut) + 'static + Send,
    Fut: Future<Output = ()> + Send + 'static,
{
    Box::new(move |arg| Box::pin(callback(arg)))
}

/// Polls a future and turns a panic during any poll into an `Err` carrying the panic
/// payload, so one panicking step cannot kill the task's driver.
struct CatchUnwind<F>(F);

impl<F> Future for CatchUnwind<F>
where
    F: Future + Unpin,
{
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.0;
        match catch_unwind(AssertUnwindSafe(|| Pin::new(&mut *inner).poll(cx))) {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

/// The message of a panic payload, for logs and error reasons.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// Invoke a step and await it, converting a panic (while creating or polling the
/// future) into a retryable [`TaskStepStatusErr::Error`].
async fn call_step(
    function: &mut Box<ExecutableFn>,
    ctx: TaskContext,
) -> Result<TaskStepStatusOk, TaskStepStatusErr> {
    let future = match catch_unwind(AssertUnwindSafe(|| function(ctx))) {
        Ok(future) => future,
        Err(payload) => {
            return Err(TaskStepStatusErr::Error(format!(
                "step panicked: {}",
                panic_message(payload.as_ref())
            )))
        }
    };
    match CatchUnwind(future).await {
        Ok(result) => result,
        Err(payload) => Err(TaskStepStatusErr::Error(format!(
            "step panicked: {}",
            panic_message(payload.as_ref())
        ))),
    }
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
/// While a run is in progress the step states are updated live; between runs they hold
/// the final state of the most recent run. Before the first run every step is
/// [`StepStatus::Pending`].
#[derive(Debug, PartialEq, Eq, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum StepStatus {
    /// The step has not started yet in the current run.
    #[default]
    Pending,
    /// An attempt of the step is executing right now.
    Running,
    /// The step completed successfully.
    Succeeded,
    /// The step completed but reported non-fatal errors.
    HadErrors,
    /// The step failed (after exhausting any retries) or timed out.
    Failed,
    /// The step did not run because an earlier step failed.
    Skipped,
    /// The step did not run, or was not retried, because the task was being stopped.
    Cancelled,
}

/// A read-only view of a single step's identity and state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct StepState {
    /// The step's zero-based position within the task.
    pub index: usize,
    /// The step's optional description.
    pub description: Option<String>,
    /// The step's status in the current run, or in the most recent run when idle.
    pub status: StepStatus,
    /// The number of attempts made in that run (0 if the step has not started).
    pub attempts: u32,
    /// The reason of the step's most recent failed attempt in that run, if any.
    pub error: Option<String>,
}

/// The outcome of a single task run, recorded in the task's [`RunRecord`] history.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum RunOutcome {
    /// Every step succeeded.
    Success,
    /// The run completed but at least one step reported non-fatal errors.
    HadErrors,
    /// A step failed after exhausting any retries.
    Failed,
    /// A step requested force-removal of the task.
    ForceRemoved,
    /// The run was interrupted because the task was removed or the scheduler shut down.
    Cancelled,
}

/// A record of one execution of a task, kept in a bounded per-task history.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
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
    /// The reason of the failure that ended the run, if a step failed.
    pub error: Option<String>,
    /// The index of the step that failed, if any.
    pub failed_step: Option<usize>,
}

impl RunRecord {
    /// A record for a run that has just started.
    pub(crate) fn started(run_id: usize, started_at: DateTime<Utc>) -> Self {
        RunRecord {
            run_id,
            started_at,
            finished_at: None,
            outcome: None,
            attempts: 0,
            error: None,
            failed_step: None,
        }
    }
}

/// The result of one run, handed to the [`on_success`](crate::TaskBuilder::on_success)
/// and [`on_failure`](crate::TaskBuilder::on_failure) callbacks and published in
/// [`SchedulerEvent::RunFinished`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct RunReport {
    /// The task's id.
    pub task_id: usize,
    /// The task's name, if set.
    pub task_name: Option<String>,
    /// The completed run.
    pub record: RunRecord,
    /// Every step's final state in the run.
    pub steps: Vec<StepState>,
}

/// Why a task left the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum FinishReason {
    /// The task completed its repeat count, or its schedule has no further execution.
    Completed,
    /// A step returned [`TaskStepStatusErr::ErrorDelete`].
    ForceRemoved,
    /// The task was removed through [`SchedulerHandle::remove`](crate::SchedulerHandle::remove).
    Removed,
    /// The scheduler shut down.
    Shutdown,
    /// The task's driver stopped unexpectedly. Panics in steps and callbacks are caught,
    /// so this indicates a bug in tasklet itself.
    Panicked,
}

/// Handed to the [`on_finish`](crate::TaskBuilder::on_finish) callback when a task
/// leaves the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct FinishReport {
    /// The task's id.
    pub task_id: usize,
    /// The task's name, if set.
    pub task_name: Option<String>,
    /// Why the task left.
    pub reason: FinishReason,
    /// The task's most recent run, if it ran at all.
    pub last_run: Option<RunRecord>,
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

/// What to do with a scheduled run that starts late: because the task was paused when
/// it came due, or because the process was busy or suspended.
///
/// Runs requested with [`SchedulerHandle::trigger`](crate::SchedulerHandle::trigger)
/// and runs queued by [`OverlapPolicy::Queue`] are never skipped.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use tasklet::{MissedRunPolicy, TaskBuilder};
///
/// // Drop any run that would start more than 30 seconds after its slot.
/// let _task = TaskBuilder::new(chrono::Utc)
///     .every_minutes(5)
///     .missed_runs(MissedRunPolicy::Skip { grace: Duration::from_secs(30) })
///     .build();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MissedRunPolicy {
    /// Run once as soon as possible, however late, then continue with the next future
    /// slot. Several missed slots still produce a single run. This is the default.
    #[default]
    RunOnce,
    /// Skip the run if it would start more than `grace` after its scheduled time, and
    /// continue with the next future slot. The check is made when the run is dispatched
    /// and again after it waits for a concurrency permit. A grace below one second is
    /// treated as one second.
    Skip {
        /// How late a run may start and still execute.
        grace: Duration,
    },
}

/// Who asked for a run; decides whether the [`MissedRunPolicy`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunKind {
    /// Dispatched by the scheduler because the task's slot came due.
    Scheduled,
    /// Requested through the handle.
    Triggered,
    /// Queued while a previous run was in progress.
    Queued,
}

/// A command sent from the scheduler to a running task.
#[derive(Debug)]
pub(crate) enum TaskCmd {
    /// Execute the task once now.
    Run(RunKind),
}

/// The scheduler-side resources a task is wired to when it is added.
#[derive(Debug, Clone)]
pub(crate) struct SchedulerLinks {
    /// Where the task publishes [`SchedulerEvent`]s.
    pub(crate) events: broadcast::Sender<SchedulerEvent>,
    /// Notified whenever the scheduler should recompute its next wake-up.
    pub(crate) wake: Arc<Notify>,
    /// The scheduler's concurrency limits.
    pub(crate) limiter: Arc<Limiter>,
}

impl Default for SchedulerLinks {
    fn default() -> Self {
        SchedulerLinks {
            events: broadcast::channel(EVENT_CAPACITY).0,
            wake: Arc::new(Notify::new()),
            limiter: Arc::new(Limiter::default()),
        }
    }
}

/// State a running task publishes so the scheduler can observe it and decide when
/// to dispatch the next run without a blocking request/response round-trip.
#[derive(Debug)]
pub(crate) struct TaskShared {
    /// The task's id.
    pub(crate) id: usize,
    /// The task's name, if set.
    pub(crate) name: Option<String>,
    /// The concurrency pool the task belongs to, if any.
    pub(crate) pool: Option<String>,
    /// True while a run is in progress.
    pub(crate) running: AtomicBool,
    /// True while a run is waiting for a concurrency permit.
    pub(crate) waiting: AtomicBool,
    /// True from the moment a `Run` is sent until the task is idle again. Whoever flips
    /// it from false to true owns the dispatch, so two senders cannot queue duplicate
    /// runs.
    pub(crate) dispatched: AtomicBool,
    /// Set when another run was requested while one was in progress (a trigger, or a
    /// slot that came due under [`OverlapPolicy::Queue`]).
    pub(crate) pending: AtomicBool,
    /// Set once the task's driver has exited, so the scheduler reaps it.
    pub(crate) finished: AtomicBool,
    /// Set while the task is paused; a paused task is not dispatched.
    pub(crate) paused: AtomicBool,
    /// Set when a caller has requested removal.
    pub(crate) remove_requested: AtomicBool,
    /// Total number of runs started over the task's lifetime.
    pub(crate) runs: AtomicUsize,
    /// The next run id to hand out.
    run_seq: AtomicUsize,
    /// The maximum number of [`RunRecord`]s retained in `history`.
    history_limit: usize,
    /// The observable lifecycle state (status + next execution time, in UTC).
    pub(crate) state: Mutex<SharedState>,
    /// The per-step state, updated live during a run.
    pub(crate) steps: Mutex<Vec<StepState>>,
    /// A bounded history of recent runs, oldest first.
    pub(crate) history: Mutex<VecDeque<RunRecord>>,
    /// The outcome of the most recent finished run, kept even when history is disabled.
    pub(crate) last_outcome: Mutex<Option<RunOutcome>>,
    /// Why the task is being stopped, set by removal or shutdown before cancelling.
    pub(crate) stop_reason: Mutex<Option<FinishReason>>,
    /// Guards the single [`SchedulerEvent::TaskRemoved`] per task.
    removal_reported: AtomicBool,
    /// Cancelled when the task is being stopped.
    pub(crate) cancel: CancelToken,
    /// Links back to the owning scheduler.
    pub(crate) links: SchedulerLinks,
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
    /// Create the shared cell for a task that is about to be added to a scheduler.
    pub(crate) fn new<T>(task: &Task<T>, links: SchedulerLinks) -> Self
    where
        T: TimeZone + Send + 'static,
    {
        TaskShared {
            id: task.task_id,
            name: task.name.clone(),
            pool: task.pool.clone(),
            running: AtomicBool::new(false),
            waiting: AtomicBool::new(false),
            dispatched: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            remove_requested: AtomicBool::new(false),
            runs: AtomicUsize::new(0),
            run_seq: AtomicUsize::new(0),
            history_limit: task.history_limit,
            state: Mutex::new(SharedState {
                status: task.status.clone(),
                next_exec: task.next_exec_utc(),
            }),
            steps: Mutex::new(task.step_states.clone()),
            history: Mutex::new(VecDeque::new()),
            last_outcome: Mutex::new(None),
            stop_reason: Mutex::new(None),
            removal_reported: AtomicBool::new(false),
            cancel: task.cancel.clone(),
            links,
        }
    }

    /// Publish an event to the scheduler's subscribers. A send with no subscriber is
    /// not an error.
    pub(crate) fn emit(&self, event: SchedulerEvent) {
        let _ = self.links.events.send(event);
    }

    /// Ask the task to stop for `reason`. The first reason recorded wins.
    pub(crate) fn request_stop(&self, reason: FinishReason) {
        self.stop_reason.lock().unwrap().get_or_insert(reason);
        self.cancel.cancel();
    }

    /// The reason recorded by [`request_stop`](Self::request_stop), or `Shutdown`.
    pub(crate) fn stop_reason(&self) -> FinishReason {
        self.stop_reason
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(FinishReason::Shutdown)
    }

    /// Publish [`SchedulerEvent::TaskRemoved`] unless it was already published.
    pub(crate) fn report_removed(&self, reason: FinishReason) {
        if !self.removal_reported.swap(true, Ordering::SeqCst) {
            self.emit(SchedulerEvent::TaskRemoved {
                task_id: self.id,
                task_name: self.name.clone(),
                reason,
            });
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
            history.push_back(RunRecord::started(run_id, started_at));
        }
        run_id
    }

    /// Open a run record as a real run would, for tests outside this module.
    #[cfg(test)]
    pub(crate) fn start_run_for_test(&self) -> usize {
        self.start_run(Utc::now())
    }

    /// Replace the in-progress record of a run with its final version.
    fn finish_run(&self, record: &RunRecord) {
        let mut history = self.history.lock().unwrap();
        if let Some(entry) = history.iter_mut().rev().find(|r| r.run_id == record.run_id) {
            *entry = record.clone();
        }
        *self.last_outcome.lock().unwrap() = record.outcome.clone();
    }

    /// Mark a run that will never finish (its driver was aborted) as cancelled.
    pub(crate) fn abandon_run(&self, now: DateTime<Utc>) {
        let mut abandoned = false;
        for record in self.history.lock().unwrap().iter_mut() {
            if record.outcome.is_none() {
                record.finished_at = Some(now);
                record.outcome = Some(RunOutcome::Cancelled);
                abandoned = true;
            }
        }
        if abandoned || self.running.load(Ordering::SeqCst) {
            *self.last_outcome.lock().unwrap() = Some(RunOutcome::Cancelled);
        }
        self.running.store(false, Ordering::SeqCst);
        self.waiting.store(false, Ordering::SeqCst);
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
    /// (Internal) per-step state, updated live during a run.
    pub(crate) step_states: Vec<StepState>,
    /// (Internal) shared cell to publish step transitions and events to as they happen.
    /// Set by the scheduler when the task is spawned; `None` when the task is driven
    /// directly (e.g. in unit tests), in which case publishing is a no-op.
    pub(crate) sink: Option<Arc<TaskShared>>,
    /// (Internal) the record of the most recent completed run.
    pub(crate) last_run: Option<RunRecord>,
    /// (Internal) whether the most recent run was interrupted by cancellation.
    run_cancelled: bool,
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
    pub(crate) on_success: Option<Box<CallbackFn<RunReport>>>,
    /// (Optional) callback invoked after a failed execution.
    pub(crate) on_failure: Option<Box<CallbackFn<RunReport>>>,
    /// (Optional) callback invoked once when the task leaves the scheduler.
    pub(crate) on_finish: Option<Box<CallbackFn<FinishReport>>>,
    /// Behaviour when a scheduled run overlaps a still-running one.
    pub(crate) overlap: OverlapPolicy,
    /// Behaviour when a scheduled run starts late.
    pub(crate) missed_runs: MissedRunPolicy,
    /// The concurrency pool the task belongs to, if any.
    pub(crate) pool: Option<String>,
    /// Cancelled when the task is being stopped; handed to every step's context.
    pub(crate) cancel: CancelToken,
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
            .field("name", &self.name)
            .field("description", &self.description)
            .field("status", &self.status)
            .field("repeats", &self.repeats)
            .field("next_exec", &self.next_exec)
            .field("timeout", &self.timeout)
            .field("retry_policy", &self.retry_policy)
            .field("overlap", &self.overlap)
            .field("missed_runs", &self.missed_runs)
            .field("pool", &self.pool)
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
            sink: None,
            last_run: None,
            run_cancelled: false,
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
            missed_runs: MissedRunPolicy::default(),
            pool: None,
            cancel: CancelToken::new(),
            blackboard: Blackboard::new(),
        })
    }

    /// Set the receiver for the task.
    pub(crate) fn set_receiver(&mut self, receiver: mpsc::Receiver<TaskCmd>) {
        self.receiver = Some(receiver);
    }

    /// Set the task id of the current task.
    ///
    /// The scheduler assigns ids itself when a task is added, overwriting any value set
    /// here.
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

    /// Set the missed-run policy.
    pub(crate) fn set_missed_runs(&mut self, policy: MissedRunPolicy) -> &mut Task<T> {
        self.missed_runs = policy;
        self
    }

    /// Put the task in a named concurrency pool.
    pub(crate) fn set_pool(&mut self, pool: &str) -> &mut Task<T> {
        self.pool = Some(pool.to_string());
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
                attempts: 0,
                error: None,
            })
            .collect();
    }

    /// Publish the current per-step snapshot to the shared cell, if a sink is set, so
    /// observers see step transitions live during a run. A no-op when no sink is set.
    fn publish_steps(&self) {
        if let Some(shared) = &self.sink {
            *shared.steps.lock().unwrap() = self.step_states.clone();
        }
    }

    /// Publish an event through the sink, if one is set.
    fn emit(&self, event: SchedulerEvent) {
        if let Some(shared) = &self.sink {
            shared.emit(event);
        }
    }

    /// Set the callbacks invoked over the task's lifecycle.
    pub(crate) fn set_callbacks(
        &mut self,
        on_success: Option<Box<CallbackFn<RunReport>>>,
        on_failure: Option<Box<CallbackFn<RunReport>>>,
        on_finish: Option<Box<CallbackFn<FinishReport>>>,
    ) -> &mut Task<T> {
        self.on_success = on_success;
        self.on_failure = on_failure;
        self.on_finish = on_finish;
        self
    }

    /// Invoke a lifecycle callback if it is set.
    ///
    /// A panicking callback is logged and otherwise ignored, so it cannot stop the
    /// task's driver.
    async fn fire_callback<A>(task_id: usize, callback: &mut Option<Box<CallbackFn<A>>>, arg: A) {
        if let Some(callback) = callback.as_mut() {
            let outcome = match catch_unwind(AssertUnwindSafe(|| callback(arg))) {
                Ok(future) => CatchUnwind(future).await,
                Err(payload) => Err(payload),
            };
            if let Err(payload) = outcome {
                task_log!(
                    task_id,
                    log::Level::Error,
                    "Lifecycle callback panicked: {}",
                    panic_message(payload.as_ref())
                );
            }
        }
    }

    /// Fire `on_finish` with the given reason.
    pub(crate) async fn fire_finish(&mut self, reason: FinishReason) {
        let report = FinishReport {
            task_id: self.task_id,
            task_name: self.name.clone(),
            reason,
            last_run: self.last_run.clone(),
        };
        Self::fire_callback(self.task_id, &mut self.on_finish, report).await;
    }

    /// Initialize the `Task` instance and schedule the first execution.
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

    /// The report of the most recent run.
    pub(crate) fn last_report(&self) -> Option<RunReport> {
        Some(RunReport {
            task_id: self.task_id,
            task_name: self.name.clone(),
            record: self.last_run.clone()?,
            steps: self.step_states.clone(),
        })
    }

    /// Fire `on_success` or `on_failure` for a finished run. A cancelled run fires
    /// neither.
    pub(crate) async fn notify_run(&mut self, report: &RunReport) {
        match report.record.outcome {
            Some(RunOutcome::Success) | Some(RunOutcome::HadErrors) => {
                Self::fire_callback(self.task_id, &mut self.on_success, report.clone()).await
            }
            Some(RunOutcome::Failed) | Some(RunOutcome::ForceRemoved) => {
                Self::fire_callback(self.task_id, &mut self.on_failure, report.clone()).await
            }
            _ => {}
        }
    }

    /// Run the task's steps and fire `on_success` or `on_failure` based on the outcome.
    ///
    /// Returns the run's report, or `None` if the task was not in a runnable state.
    /// `on_finish` is fired from [`Task::reschedule_and_notify`] or when the task is
    /// stopped.
    #[cfg(test)]
    pub(crate) async fn run_and_notify(&mut self, run_id: usize) -> Option<RunReport> {
        if self.run_task(run_id).await.is_err() {
            return None;
        }
        let report = self.last_report()?;
        self.notify_run(&report).await;
        Some(report)
    }

    /// Reschedule the task and fire `on_finish` if it has just reached a terminal state
    /// (`Finished` or `ForceRemoved`).
    ///
    /// After a cancelled run nothing changes: the task is being stopped, and its driver
    /// fires `on_finish` with the stop reason instead of `Completed`.
    pub(crate) async fn reschedule_and_notify(&mut self) {
        if self.run_cancelled {
            return;
        }
        let _ = self.reschedule(); // status is updated in place; the result is redundant
        match self.status {
            Status::Finished => self.fire_finish(FinishReason::Completed).await,
            Status::ForceRemoved => self.fire_finish(FinishReason::ForceRemoved).await,
            _ => {}
        }
    }

    /// Whether the task has reached a terminal state and should be reaped.
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self.status, Status::Finished | Status::ForceRemoved)
    }

    /// The reason matching the task's terminal status.
    pub(crate) fn terminal_reason(&self) -> FinishReason {
        match self.status {
            Status::ForceRemoved => FinishReason::ForceRemoved,
            _ => FinishReason::Completed,
        }
    }

    /// Classify the outcome of the run that just completed, based on the status and
    /// the per-step snapshot. Call this after `run_task` and before `reschedule`, which
    /// would otherwise overwrite the status.
    pub(crate) fn run_outcome(&self) -> RunOutcome {
        if self.run_cancelled {
            return RunOutcome::Cancelled;
        }
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

    /// If the next scheduled slot is later than the [`MissedRunPolicy`] allows at `now`,
    /// return that slot.
    pub(crate) fn missed_slot(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self.missed_runs {
            MissedRunPolicy::RunOnce => None,
            MissedRunPolicy::Skip { grace } => {
                let slot = self.next_exec_utc()?;
                let late = now.signed_duration_since(slot).to_std().ok()?;
                (late > grace.max(MIN_MISSED_RUN_GRACE)).then_some(slot)
            }
        }
    }

    /// Whether a scheduled slot came due after `since` and no later than now. Used after
    /// a run to detect a slot that passed while the run was in progress.
    pub(crate) fn slot_passed_since(&self, since: &DateTime<Utc>) -> bool {
        let since = since.with_timezone(&self.timezone);
        let now = Utc::now().with_timezone(&self.timezone);
        self.schedule
            .after(&since)
            .next()
            .is_some_and(|slot| slot <= now)
    }

    /// Move to the next future slot without running, as the [`MissedRunPolicy`]
    /// requires. Retires the task (and fires `on_finish`) if there is none.
    pub(crate) async fn skip_to_next_slot(&mut self) {
        match self.schedule.upcoming(self.timezone.clone()).next() {
            Some(next) => self.next_exec = Some(next),
            None => {
                self.status = Status::Finished;
                self.fire_finish(FinishReason::Completed).await;
            }
        }
    }

    /// Run the task and handle the output.
    ///
    /// `run_id` identifies the current run and is surfaced to each step through its
    /// [`TaskContext`]. On success the run's record is left in `last_run`.
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
                let started_at = Utc::now();
                // Snapshot the timeout / retry configuration before borrowing
                // `self.steps` mutably below.
                let timeout = self.timeout;
                let retry = self.retry_policy.clone();
                let max_attempts = 1 + retry.as_ref().map(|r| r.max_retries).unwrap_or(0);

                // Rebuild the per-step snapshot for this run.
                self.reset_step_states();
                self.run_cancelled = false;
                // Stream the fresh (all-pending) snapshot so observers see the run begin.
                self.publish_steps();

                // A fresh per-run store for step-to-step data flow, plus the task
                // identity captured once so building each attempt's context is cheap.
                let run_store = Blackboard::new();
                let task_name: Option<Arc<str>> = self.name.as_deref().map(Arc::from);
                let task_id = self.task_id;
                let blackboard = self.blackboard.clone();
                let cancel = self.cancel.clone();

                let mut attempts_total: usize = 0;
                let mut failure: Option<(usize, String)> = None;
                let mut stopped = false;
                // Iterate by index so `self` is free between step invocations, letting us
                // stream each transition to the shared cell as it happens.
                for index in 0..self.steps.len() {
                    if stopped {
                        break;
                    }
                    // The description is fixed per step; capture it for logging so we do
                    // not hold a borrow of `self.steps` across the awaited step future.
                    let step_desc = self.steps[index].to_string();
                    // Attempt the step, retrying `Error` failures and timeouts according
                    // to the retry policy. `ErrorDelete`, success and cancellation all end
                    // the attempt loop.
                    let mut attempt: u32 = 0;
                    loop {
                        if cancel.is_cancelled() {
                            step_log!(
                                self.task_id,
                                index,
                                log::Level::Info,
                                "Not started, the task is being stopped - {}",
                                step_desc
                            );
                            self.step_states[index].status = StepStatus::Cancelled;
                            self.run_cancelled = true;
                            if attempt > 0 {
                                // Stopped during a retry backoff: the step had started.
                                self.publish_steps();
                                self.emit_step_finished(run_id, index);
                            }
                            stopped = true;
                            break;
                        }
                        attempt += 1;
                        attempts_total += 1;
                        self.step_states[index].status = StepStatus::Running;
                        self.step_states[index].attempts = attempt;
                        self.publish_steps();
                        self.emit(SchedulerEvent::StepStarted {
                            task_id,
                            task_name: self.name.clone(),
                            run_id,
                            step_index: index,
                            attempt,
                        });

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
                            cancel: cancel.clone(),
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
                                    call_step(&mut self.steps[index].function, ctx),
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
                                        Err(TaskStepStatusErr::Error(format!(
                                            "timed out after {:?}",
                                            duration
                                        )))
                                    }
                                }
                            }
                            None => call_step(&mut self.steps[index].function, ctx).await,
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
                                self.emit_step_finished(run_id, index);
                                break;
                            }
                            Err(err) => {
                                self.step_states[index].error = Some(err.reason().to_string());
                                if !err.is_fatal() && cancel.is_cancelled() {
                                    // A step that gives up because the task is being
                                    // stopped is interrupted, not failed: no retry and no
                                    // `on_failure`.
                                    step_log!(
                                        self.task_id,
                                        index,
                                        log::Level::Info,
                                        "Stopped early, the task is being stopped: {} - {}",
                                        err,
                                        step_desc
                                    );
                                    self.step_states[index].status = StepStatus::Cancelled;
                                    self.run_cancelled = true;
                                    self.publish_steps();
                                    self.emit_step_finished(run_id, index);
                                    stopped = true;
                                    break;
                                }
                                if err.is_fatal() {
                                    step_log!(
                                        self.task_id,
                                        index,
                                        log::Level::Error,
                                        "Execution failed and task is marked for deletion: {} - {}",
                                        err,
                                        step_desc
                                    );
                                    self.step_states[index].status = StepStatus::Failed;
                                    self.status = Status::ForceRemoved;
                                } else if (attempt as usize) < max_attempts {
                                    // `retry` is guaranteed `Some` here: `max_attempts > 1`
                                    // only when a retry policy is set.
                                    let delay = retry.as_ref().unwrap().jittered_delay(attempt - 1);
                                    step_log!(
                                        self.task_id,
                                        index,
                                        log::Level::Warn,
                                        "Execution failed, retrying (attempt {}/{}) after {:?}: {} - {}",
                                        attempt,
                                        max_attempts,
                                        delay,
                                        err,
                                        step_desc
                                    );
                                    self.publish_steps();
                                    if !delay.is_zero() {
                                        // A stop request cuts the backoff short; the loop
                                        // head then marks the step cancelled.
                                        tokio::select! {
                                            _ = tokio::time::sleep(delay) => {}
                                            _ = cancel.cancelled() => {}
                                        }
                                    }
                                    continue;
                                } else {
                                    step_log!(
                                        self.task_id,
                                        index,
                                        log::Level::Error,
                                        "Execution failed: {} - {}",
                                        err,
                                        step_desc
                                    );
                                    self.step_states[index].status = StepStatus::Failed;
                                    self.status = Status::Failed;
                                }
                                failure = Some((index, err.reason().to_string()));
                                self.publish_steps();
                                self.emit_step_finished(run_id, index);
                                stopped = true;
                                break;
                            }
                        }
                    }
                }
                // Any step still pending was never reached, either because an earlier
                // step failed or because the task is being stopped.
                let unreached = if self.run_cancelled {
                    StepStatus::Cancelled
                } else {
                    StepStatus::Skipped
                };
                for step_state in self.step_states.iter_mut() {
                    if step_state.status == StepStatus::Pending {
                        step_state.status = unreached.clone();
                    }
                }
                // Stream the final per-step outcome (including any skipped steps).
                self.publish_steps();
                // Avoid underflow in case of a task without steps.
                if self.steps.is_empty() {
                    self.status = Status::Executed
                }
                // An interrupted run did not complete; mark it failed so it is never
                // mistaken for a success. The task is about to stop anyway.
                if self.run_cancelled && self.status != Status::ForceRemoved {
                    self.status = Status::Failed;
                }

                // Reduce the total executions (if set). Saturate at zero so a task
                // constructed with `repeat(0)` cannot underflow. An interrupted run does
                // not count.
                if !self.run_cancelled {
                    self.repeats = self.repeats.map(|r| r.saturating_sub(1));
                }

                let outcome = self.run_outcome();
                let (failed_step, error) = match failure {
                    Some((index, reason)) => (Some(index), Some(reason)),
                    None => (None, None),
                };
                self.last_run = Some(RunRecord {
                    run_id,
                    started_at,
                    finished_at: Some(Utc::now()),
                    outcome: Some(outcome),
                    attempts: attempts_total,
                    error,
                    failed_step,
                });

                Ok(())
            }
        }
    }

    /// Publish [`SchedulerEvent::StepFinished`] for the step at `index`.
    fn emit_step_finished(&self, run_id: usize, index: usize) {
        self.emit(SchedulerEvent::StepFinished {
            task_id: self.task_id,
            task_name: self.name.clone(),
            run_id,
            step: self.step_states[index].clone(),
        });
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
                                log::Level::Info,
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
                    log::Level::Debug,
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

/// Skip the run if it is a scheduled run that starts later than the task's
/// [`MissedRunPolicy`] allows. Returns whether it was skipped.
async fn skip_if_missed<T>(task: &mut Task<T>, shared: &TaskShared, kind: RunKind) -> bool
where
    T: TimeZone + Send + 'static,
    <T as TimeZone>::Offset: Send,
{
    // Triggered and queued runs were asked for explicitly and are never skipped.
    if kind != RunKind::Scheduled {
        return false;
    }
    let Some(slot) = task.missed_slot(Utc::now()) else {
        return false;
    };
    task_log!(
        task.task_id,
        log::Level::Info,
        "Skipping the run scheduled for {}: it started later than the missed-run policy allows",
        slot
    );
    shared.emit(SchedulerEvent::RunSkipped {
        task_id: shared.id,
        task_name: shared.name.clone(),
        scheduled_for: slot,
    });
    task.skip_to_next_slot().await;
    publish(task, shared);
    true
}

/// Handle one run request: apply the missed-run policy, wait for concurrency permits,
/// execute the task, record the run, fire its callbacks, reschedule it and republish
/// the result.
async fn run_once<T>(task: &mut Task<T>, shared: &TaskShared, kind: RunKind)
where
    T: TimeZone + Send + 'static,
    <T as TimeZone>::Offset: Send,
{
    if skip_if_missed(task, shared, kind).await {
        return;
    }

    // Take the pool and scheduler-wide permits. `waiting` is only reported when a
    // permit is not free right away. A stop request while waiting abandons the run
    // before it starts.
    let limiter = &shared.links.limiter;
    let pool = shared.pool.as_deref();
    let permits = match limiter.try_acquire(pool) {
        Some(permits) => permits,
        None => {
            shared.waiting.store(true, Ordering::SeqCst);
            let acquired = tokio::select! {
                biased;
                _ = shared.cancel.cancelled() => None,
                permits = limiter.acquire(pool) => Some(permits),
            };
            shared.waiting.store(false, Ordering::SeqCst);
            let Some(permits) = acquired else {
                return;
            };
            // The wait may have pushed the run past its grace period.
            if skip_if_missed(task, shared, kind).await {
                return;
            }
            permits
        }
    };

    let started_at = Utc::now();
    let run_id = shared.start_run(started_at);
    shared.running.store(true, Ordering::SeqCst);
    shared.emit(SchedulerEvent::RunStarted {
        task_id: shared.id,
        task_name: shared.name.clone(),
        run_id,
    });
    let ran = task.run_task(run_id).await.is_ok();
    // Release the permits and publish the record before the callbacks run, so a slow
    // callback does not hold up other runs and observers already see the result.
    drop(permits);
    match task.last_report().filter(|_| ran) {
        Some(report) => {
            shared.finish_run(&report.record);
            shared.emit(SchedulerEvent::RunFinished {
                report: report.clone(),
            });
            task.notify_run(&report).await;
        }
        // The task was not runnable; close the record opened above.
        None => shared.abandon_run(Utc::now()),
    }
    // `OverlapPolicy::Queue`: a slot that came due while this run was in progress runs
    // next, once.
    if task.overlap == OverlapPolicy::Queue && task.slot_passed_since(&started_at) {
        shared.pending.store(true, Ordering::SeqCst);
    }
    task.reschedule_and_notify().await;
    shared.running.store(false, Ordering::SeqCst);
    publish(task, shared);
}

/// Drive a `Task` in its own Tokio task.
///
/// The task initializes itself if needed, publishes its state to `shared`, then runs
/// once for each [`TaskCmd::Run`] it receives and drains runs queued while it was busy.
/// It exits when it reaches a terminal state or is asked to stop; on a stop it fires
/// `on_finish` with the stop reason. Runs never block the scheduler: the scheduler
/// fire-and-forgets `Run` and observes progress through `shared`.
pub(crate) async fn run_task<T>(mut task: Task<T>, shared: Arc<TaskShared>)
where
    T: TimeZone + Send + 'static,
    <T as TimeZone>::Offset: Send,
{
    // Wire the shared cell as the sink so step transitions and events stream live.
    task.sink = Some(shared.clone());
    if task.status == Status::Init {
        task.init();
    }
    publish(&task, &shared);

    if task.is_terminal() {
        // The schedule has no execution at all.
        task.fire_finish(FinishReason::Completed).await;
    } else if let Some(mut receiver) = task.receiver.take() {
        'driver: loop {
            let first = tokio::select! {
                biased;
                _ = shared.cancel.cancelled() => break 'driver,
                cmd = receiver.recv() => match cmd {
                    Some(TaskCmd::Run(kind)) => kind,
                    None => break 'driver,
                },
            };
            let mut kind = first;
            loop {
                run_once(&mut task, &shared, kind).await;
                if task.is_terminal() || shared.cancel.is_cancelled() {
                    break 'driver;
                }
                // Take a queued request while still holding `dispatched`, so no other
                // sender can dispatch in between.
                if shared.pending.swap(false, Ordering::SeqCst) {
                    kind = RunKind::Queued;
                    continue;
                }
                shared.dispatched.store(false, Ordering::SeqCst);
                // A trigger may have set `pending` after the swap above while it still
                // saw `dispatched` held. Whoever wins the flag now owns that request
                // (see `SchedulerHandle::trigger`).
                if shared.pending.load(Ordering::SeqCst)
                    && shared
                        .dispatched
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    if shared.pending.swap(false, Ordering::SeqCst) {
                        kind = RunKind::Queued;
                        continue;
                    }
                    shared.dispatched.store(false, Ordering::SeqCst);
                }
                break;
            }
            // The task is idle with a new next execution time.
            shared.links.wake.notify_one();
        }
        if !task.is_terminal() {
            // Stopped from outside: removal or shutdown.
            let reason = shared.stop_reason();
            task_log!(task.task_id, log::Level::Debug, "Stopped ({:?})", reason);
            task.fire_finish(reason).await;
        }
    }

    let reason = if task.is_terminal() {
        task.terminal_reason()
    } else {
        shared.stop_reason()
    };
    shared.report_removed(reason);
    shared.finished.store(true, Ordering::SeqCst);
    shared.links.wake.notify_one();
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
        task.add_step_default(|_ctx| async { Err(Error("boom".to_string())) });
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
        task.add_step_default(|_ctx| async { Err(Error("boom".to_string())) });
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
        task.add_step_default(|_ctx| async { Err(ErrorDelete("fatal".to_string())) });
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

        let shared = Arc::new(TaskShared::new(&task, SchedulerLinks::default()));
        let join = tokio::spawn(run_task(task, shared.clone()));

        // After self-initialization the task publishes Scheduled + a next execution.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        {
            let state = shared.state.lock().unwrap();
            assert_eq!(state.status, Status::Scheduled);
            assert!(state.next_exec.is_some());
        }

        // Dispatch a single run; with repeat(1) the task then retires and the loop ends.
        tx.send(TaskCmd::Run(RunKind::Scheduled)).await.unwrap();
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
                Err(TaskStepStatusErr::error("step 2 failed"))
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
            Err(TaskStepStatusErr::error_delete("fatal"))
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
                    Err(Error("boom".to_string()))
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
                Err(Error("boom".to_string()))
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
                Err(ErrorDelete("fatal".to_string()))
            }
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.status, Status::ForceRemoved);
        // Called exactly once: no retries.
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
        task.on_success = Some(boxed_callback(move |_| {
            let s = s.clone();
            async move {
                s.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let f = failure.clone();
        task.on_failure = Some(boxed_callback(move |_| {
            let f = f.clone();
            async move {
                f.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let fin = finish.clone();
        task.on_finish = Some(boxed_callback(move |_| {
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
        task.add_step_default(|_ctx| async { Err(ErrorDelete("fatal".to_string())) });

        let f = failure.clone();
        task.on_failure = Some(boxed_callback(move |_| {
            let f = f.clone();
            async move {
                f.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let fin = finish.clone();
        task.on_finish = Some(boxed_callback(move |_| {
            let fin = fin.clone();
            async move {
                fin.fetch_add(1, Ordering::SeqCst);
            }
        }));

        task.set_id(0);
        task.init();

        // The run fires on_failure; ForceRemoved is terminal, so the reschedule fires on_finish.
        task.run_and_notify(0).await;
        task.reschedule_and_notify().await;

        assert_eq!(failure.load(Ordering::SeqCst), 1);
        assert_eq!(finish.load(Ordering::SeqCst), 1);
    }

    /// Step states track each step's outcome; a step after a failure is skipped. (Layer 0)
    #[tokio::test]
    async fn test_step_states_track_outcomes() {
        let mut task = Task::new("* * * * * * *", Some("Steps"), Some(1), Utc).unwrap();
        task.add_step("ok", |_ctx| async { Ok(Success) });
        task.add_step("boom", |_ctx| async { Err(Error("boom".to_string())) });
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
        assert_eq!(task.last_run.as_ref().unwrap().attempts, 2);
        assert_eq!(task.step_states[1].error.as_deref(), Some("boom"));
        assert_eq!(task.last_run.as_ref().unwrap().failed_step, Some(1));
        assert_eq!(
            task.last_run.as_ref().unwrap().error.as_deref(),
            Some("boom")
        );
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

        let shared = TaskShared::new(&task, SchedulerLinks::default());
        for _ in 0..3 {
            run_once(&mut task, &shared, RunKind::Scheduled).await;
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

        let shared = TaskShared::new(&task, SchedulerLinks::default());
        run_once(&mut task, &shared, RunKind::Scheduled).await;

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

        let shared = Arc::new(TaskShared::new(&task, SchedulerLinks::default()));
        let join = tokio::spawn(run_task(task, shared.clone()));

        // Dispatch a run and wait until step 1 is mid-flight.
        tx.send(TaskCmd::Run(RunKind::Scheduled)).await.unwrap();
        ready.notified().await;

        // Live snapshot: step 0 already succeeded while step 1 is still running.
        {
            let steps = shared.steps.lock().unwrap();
            assert_eq!(steps[0].status, StepStatus::Succeeded);
            assert_eq!(steps[1].status, StepStatus::Running);
            assert_eq!(steps[1].attempts, 1);
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
            attempts: 1,
            error: None,
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
            error: Some("boom".to_string()),
            failed_step: Some(1),
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
                    Err(Error("boom".to_string()))
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

    /// Collect the events published so far on a receiver.
    fn drain_events(rx: &mut broadcast::Receiver<SchedulerEvent>) -> Vec<SchedulerEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// A failing step's reason, attempt count and index reach the step state and the
    /// run record. (P4)
    #[tokio::test]
    async fn test_error_reason_recorded() {
        use crate::retry::RetryPolicy;
        let mut task = Task::new("* * * * * *", None, Some(1), Utc).unwrap();
        task.set_retry_policy(RetryPolicy::fixed(1, Duration::from_millis(0)));
        task.add_step("ok", |_ctx| async { Ok(Success) });
        task.add_step("db", |_ctx| async {
            Err(TaskStepStatusErr::error("database unavailable"))
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());

        let step = &task.step_states[1];
        assert_eq!(step.status, StepStatus::Failed);
        assert_eq!(step.attempts, 2);
        assert_eq!(step.error.as_deref(), Some("database unavailable"));
        assert_eq!(task.step_states[0].attempts, 1);
        assert_eq!(task.step_states[0].error, None);

        let record = task.last_run.clone().unwrap();
        assert_eq!(record.outcome, Some(RunOutcome::Failed));
        assert_eq!(record.failed_step, Some(1));
        assert_eq!(record.error.as_deref(), Some("database unavailable"));
        assert_eq!(record.attempts, 3);
        assert!(record.finished_at.is_some());
    }

    /// A timed-out attempt reports the timeout as its reason. (P4)
    #[tokio::test]
    async fn test_timeout_reason_recorded() {
        let mut task = Task::new("* * * * * *", None, Some(1), Utc).unwrap();
        task.set_timeout(Duration::from_millis(20));
        task.add_step_default(|_ctx| async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(Success)
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        let error = task.step_states[0].error.clone().unwrap();
        assert!(error.starts_with("timed out after"), "got {error}");
    }

    /// `TaskStepStatusErr` helpers expose the reason and severity. (P4)
    #[test]
    fn test_step_error_helpers() {
        let err = TaskStepStatusErr::error("retry me");
        assert_eq!(err, Error("retry me".to_string()));
        assert_eq!(err.reason(), "retry me");
        assert_eq!(err.to_string(), "retry me");
        assert!(!err.is_fatal());
        let fatal = TaskStepStatusErr::error_delete("gone");
        assert_eq!(fatal, ErrorDelete("gone".to_string()));
        assert!(fatal.is_fatal());
    }

    /// `on_success` and `on_failure` receive the run report, `on_finish` the reason and
    /// the last run. (P4)
    #[tokio::test]
    async fn test_callbacks_receive_reports() {
        let reports = Arc::new(Mutex::new(Vec::<RunReport>::new()));
        let finishes = Arc::new(Mutex::new(Vec::<FinishReport>::new()));
        let mut task = Task::new("* * * * * *", None, Some(2), Utc).unwrap();
        task.set_id(4);
        task.set_name("reporter");
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        task.add_step("flip", move |_ctx| {
            let c = c.clone();
            async move {
                // Succeed on the first run, fail on the second.
                if c.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(Success)
                } else {
                    Err(TaskStepStatusErr::error("second run fails"))
                }
            }
        });
        let r = reports.clone();
        let r2 = reports.clone();
        let f = finishes.clone();
        task.set_callbacks(
            Some(boxed_callback(move |report: RunReport| {
                let r = r.clone();
                async move { r.lock().unwrap().push(report) }
            })),
            Some(boxed_callback(move |report: RunReport| {
                let r = r2.clone();
                async move { r.lock().unwrap().push(report) }
            })),
            Some(boxed_callback(move |report: FinishReport| {
                let f = f.clone();
                async move { f.lock().unwrap().push(report) }
            })),
        );
        task.init();
        for run_id in 0..2 {
            assert!(task.run_and_notify(run_id).await.is_some());
            task.reschedule_and_notify().await;
        }

        let reports = reports.lock().unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].task_id, 4);
        assert_eq!(reports[0].task_name.as_deref(), Some("reporter"));
        assert_eq!(reports[0].record.outcome, Some(RunOutcome::Success));
        assert_eq!(reports[1].record.run_id, 1);
        assert_eq!(reports[1].record.outcome, Some(RunOutcome::Failed));
        assert_eq!(reports[1].record.error.as_deref(), Some("second run fails"));
        assert_eq!(reports[1].steps[0].status, StepStatus::Failed);

        let finishes = finishes.lock().unwrap();
        assert_eq!(finishes.len(), 1, "on_finish fires once");
        assert_eq!(finishes[0].reason, FinishReason::Completed);
        assert_eq!(finishes[0].last_run.as_ref().unwrap().run_id, 1);
    }

    /// A task stopped before its run starts marks every step cancelled, records a
    /// cancelled run and fires neither `on_success` nor `on_failure`. (P1)
    #[tokio::test]
    async fn test_cancelled_before_run() {
        let fired = Arc::new(AtomicUsize::new(0));
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.add_step("a", |_ctx| async { Ok(Success) });
        task.add_step("b", |_ctx| async { Ok(Success) });
        let f1 = fired.clone();
        let f2 = fired.clone();
        task.set_callbacks(
            Some(boxed_callback(move |_| {
                let f = f1.clone();
                async move {
                    f.fetch_add(1, Ordering::SeqCst);
                }
            })),
            Some(boxed_callback(move |_| {
                let f = f2.clone();
                async move {
                    f.fetch_add(1, Ordering::SeqCst);
                }
            })),
            None,
        );
        task.init();
        task.cancel.cancel();

        let report = task.run_and_notify(0).await.unwrap();
        assert_eq!(report.record.outcome, Some(RunOutcome::Cancelled));
        assert!(report
            .steps
            .iter()
            .all(|s| s.status == StepStatus::Cancelled && s.attempts == 0));
        assert_eq!(fired.load(Ordering::SeqCst), 0);
    }

    /// A step that gives up after seeing the stop request is cancelled rather than
    /// failed: it is not retried and later steps do not run. (P1)
    #[tokio::test]
    async fn test_cooperative_cancellation_mid_run() {
        use crate::retry::RetryPolicy;
        let token = CancelToken::new();
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.cancel = token.clone();
        task.set_retry_policy(RetryPolicy::fixed(3, Duration::from_millis(0)));
        let second_ran = Arc::new(AtomicBool::new(false));
        task.add_step("long", move |ctx| {
            let token = token.clone();
            async move {
                assert!(!ctx.is_cancelled());
                // Simulate a stop request arriving while the step is waiting.
                token.cancel();
                ctx.cancelled().await;
                assert!(ctx.is_cancelled());
                Err(TaskStepStatusErr::error("interrupted"))
            }
        });
        let s = second_ran.clone();
        task.add_step("after", move |_ctx| {
            let s = s.clone();
            async move {
                s.store(true, Ordering::SeqCst);
                Ok(Success)
            }
        });
        task.init();
        assert!(task.run_task(0).await.is_ok());

        assert_eq!(task.step_states[0].status, StepStatus::Cancelled);
        assert_eq!(task.step_states[0].attempts, 1, "no retry after a stop");
        assert_eq!(task.step_states[0].error.as_deref(), Some("interrupted"));
        assert_eq!(task.step_states[1].status, StepStatus::Cancelled);
        assert!(!second_ran.load(Ordering::SeqCst));
        assert_eq!(task.run_outcome(), RunOutcome::Cancelled);
        assert_eq!(task.last_run.as_ref().unwrap().failed_step, None);
    }

    /// A stop request cuts a retry backoff short. (P1)
    #[tokio::test]
    async fn test_cancel_interrupts_backoff() {
        use crate::retry::RetryPolicy;
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.set_retry_policy(RetryPolicy::fixed(5, Duration::from_secs(30)));
        task.add_step_default(|_ctx| async { Err(TaskStepStatusErr::error("flaky")) });
        task.init();
        let token = task.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        });
        tokio::time::timeout(Duration::from_secs(2), task.run_task(0))
            .await
            .expect("the backoff sleep must end on cancellation")
            .unwrap();
        assert_eq!(task.step_states[0].status, StepStatus::Cancelled);
        assert_eq!(task.run_outcome(), RunOutcome::Cancelled);
    }

    /// `missed_slot` reports a late slot only under `Skip` and only past the grace
    /// period. (P3)
    #[test]
    fn test_missed_slot_policy() {
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.init();
        let now = Utc::now();
        task.next_exec = Some(now - chrono::Duration::seconds(10));

        assert_eq!(task.missed_slot(now), None, "RunOnce never skips");
        task.set_missed_runs(MissedRunPolicy::Skip {
            grace: Duration::from_secs(1),
        });
        assert_eq!(task.missed_slot(now), task.next_exec_utc());
        task.set_missed_runs(MissedRunPolicy::Skip {
            grace: Duration::from_secs(60),
        });
        assert_eq!(task.missed_slot(now), None, "within the grace period");
        // A slot in the future is never late.
        task.next_exec = Some(now + chrono::Duration::seconds(10));
        task.set_missed_runs(MissedRunPolicy::Skip {
            grace: Duration::ZERO,
        });
        assert_eq!(task.missed_slot(now), None);
    }

    /// `slot_passed_since` detects a slot that came due after the given time. (P3)
    #[test]
    fn test_slot_passed_since() {
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.init();
        assert!(task.slot_passed_since(&(Utc::now() - chrono::Duration::seconds(3))));
        // Stay clear of a second boundary: the function reads the clock again, and a
        // boundary crossed in between would make a per-second slot "pass".
        if Utc::now().timestamp_subsec_millis() > 800 {
            std::thread::sleep(Duration::from_millis(250));
        }
        // The next per-second slot after "now" lies in the future.
        assert!(!task.slot_passed_since(&Utc::now()));
    }

    /// A late scheduled run is skipped under `Skip`, publishing `RunSkipped` and moving
    /// to a future slot, while a triggered run of the same late slot executes. (P3)
    #[tokio::test]
    async fn test_run_once_applies_missed_run_policy() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.set_missed_runs(MissedRunPolicy::Skip {
            grace: Duration::from_secs(1),
        });
        task.add_step_default(move |_ctx| {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                Ok(Success)
            }
        });
        task.init();
        let shared = TaskShared::new(&task, SchedulerLinks::default());
        let mut events = shared.links.events.subscribe();

        let late = Utc::now() - chrono::Duration::seconds(30);
        task.next_exec = Some(late);
        run_once(&mut task, &shared, RunKind::Scheduled).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the late run is skipped");
        assert!(task.next_exec_utc().unwrap() > Utc::now() - chrono::Duration::seconds(1));
        assert!(drain_events(&mut events).iter().any(|e| matches!(
            e,
            SchedulerEvent::RunSkipped { scheduled_for, .. } if *scheduled_for == late
        )));
        assert!(shared.history.lock().unwrap().is_empty());

        task.next_exec = Some(late);
        run_once(&mut task, &shared, RunKind::Triggered).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "a triggered run always executes"
        );
    }

    /// `run_once` publishes run and step events in order. (P2)
    #[tokio::test]
    async fn test_run_once_publishes_events() {
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.set_id(2);
        task.set_name("evented");
        task.add_step("only", |_ctx| async { Ok(Success) });
        task.init();
        let shared = Arc::new(TaskShared::new(&task, SchedulerLinks::default()));
        task.sink = Some(shared.clone());
        let mut events = shared.links.events.subscribe();

        run_once(&mut task, &shared, RunKind::Scheduled).await;
        let events = drain_events(&mut events);
        assert!(matches!(
            &events[0],
            SchedulerEvent::RunStarted { task_id: 2, task_name: Some(n), run_id: 0 } if n == "evented"
        ));
        assert!(matches!(
            &events[1],
            SchedulerEvent::StepStarted {
                step_index: 0,
                attempt: 1,
                ..
            }
        ));
        assert!(matches!(
            &events[2],
            SchedulerEvent::StepFinished { step, .. } if step.status == StepStatus::Succeeded
        ));
        match &events[3] {
            SchedulerEvent::RunFinished { report } => {
                assert_eq!(report.task_id, 2);
                assert_eq!(report.record.outcome, Some(RunOutcome::Success));
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
        assert_eq!(events.len(), 4);
        assert_eq!(events[3].task_id(), Some(2));
    }

    /// `last_outcome` is kept when history is disabled and is not cleared while a run is
    /// in progress. (F8)
    #[tokio::test]
    async fn test_last_outcome_without_history() {
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.set_history_limit(0);
        task.add_step_default(|_ctx| async { Ok(Success) });
        task.init();
        let shared = TaskShared::new(&task, SchedulerLinks::default());
        run_once(&mut task, &shared, RunKind::Scheduled).await;
        assert!(shared.history.lock().unwrap().is_empty());
        assert_eq!(
            *shared.last_outcome.lock().unwrap(),
            Some(RunOutcome::Success)
        );
        // Starting another run leaves the previous outcome in place.
        shared.start_run(Utc::now());
        assert_eq!(
            *shared.last_outcome.lock().unwrap(),
            Some(RunOutcome::Success)
        );
    }

    /// `abandon_run` closes an unfinished record as cancelled. (P1)
    #[test]
    fn test_abandon_run_marks_cancelled() {
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.init();
        let shared = TaskShared::new(&task, SchedulerLinks::default());
        shared.start_run(Utc::now());
        shared.running.store(true, Ordering::SeqCst);
        shared.abandon_run(Utc::now());
        let history = shared.history.lock().unwrap();
        assert_eq!(history[0].outcome, Some(RunOutcome::Cancelled));
        assert!(history[0].finished_at.is_some());
        assert!(!shared.running.load(Ordering::SeqCst));
        assert_eq!(
            *shared.last_outcome.lock().unwrap(),
            Some(RunOutcome::Cancelled)
        );
    }

    /// A schedule with no execution at all retires the task at once and fires
    /// `on_finish` with `Completed`.
    #[tokio::test]
    async fn test_driver_without_upcoming_fires_finish() {
        let reasons = Arc::new(Mutex::new(Vec::<FinishReason>::new()));
        let (_tx, rx) = mpsc::channel(1);
        // A fixed year in the past has no upcoming slot.
        let mut task = Task::new("0 0 0 1 1 * 2000", None, None, Utc).unwrap();
        task.set_receiver(rx);
        let r = reasons.clone();
        task.set_callbacks(
            None,
            None,
            Some(boxed_callback(move |report: FinishReport| {
                let r = r.clone();
                async move { r.lock().unwrap().push(report.reason) }
            })),
        );
        let shared = Arc::new(TaskShared::new(&task, SchedulerLinks::default()));
        let mut events = shared.links.events.subscribe();
        run_task(task, shared.clone()).await;

        assert!(shared.finished.load(Ordering::SeqCst));
        assert_eq!(*reasons.lock().unwrap(), vec![FinishReason::Completed]);
        assert!(drain_events(&mut events).iter().any(|e| matches!(
            e,
            SchedulerEvent::TaskRemoved {
                reason: FinishReason::Completed,
                ..
            }
        )));
    }

    /// An idle driver exits on a stop request and fires `on_finish` with the recorded
    /// reason, publishing `TaskRemoved` once. (P1, F2)
    #[tokio::test]
    async fn test_driver_stops_on_request() {
        let reasons = Arc::new(Mutex::new(Vec::<FinishReason>::new()));
        let (_tx, rx) = mpsc::channel(1);
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.set_receiver(rx);
        let r = reasons.clone();
        task.set_callbacks(
            None,
            None,
            Some(boxed_callback(move |report: FinishReport| {
                let r = r.clone();
                async move { r.lock().unwrap().push(report.reason) }
            })),
        );
        task.init();
        let shared = Arc::new(TaskShared::new(&task, SchedulerLinks::default()));
        let mut events = shared.links.events.subscribe();
        let join = tokio::spawn(run_task(task, shared.clone()));

        tokio::time::sleep(Duration::from_millis(30)).await;
        shared.request_stop(FinishReason::Removed);
        // A later reason does not override the first.
        shared.request_stop(FinishReason::Shutdown);
        tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .expect("driver must exit on a stop request")
            .unwrap();

        assert_eq!(*reasons.lock().unwrap(), vec![FinishReason::Removed]);
        let removed: Vec<_> = drain_events(&mut events)
            .into_iter()
            .filter(|e| matches!(e, SchedulerEvent::TaskRemoved { .. }))
            .collect();
        assert_eq!(removed.len(), 1);
        // Reporting again is a no-op.
        shared.report_removed(FinishReason::Shutdown);
        assert!(events.try_recv().is_err());
    }

    /// A grace period below one second is raised to one second, so on-time runs that
    /// start a few milliseconds after their slot are not skipped. (review #6)
    #[test]
    fn test_skip_grace_has_a_floor() {
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.init();
        task.set_missed_runs(MissedRunPolicy::Skip {
            grace: Duration::ZERO,
        });
        let now = Utc::now();
        task.next_exec = Some(now - chrono::Duration::milliseconds(500));
        assert_eq!(task.missed_slot(now), None, "500 ms late is on time");
        task.next_exec = Some(now - chrono::Duration::milliseconds(1500));
        assert_eq!(task.missed_slot(now), task.next_exec_utc());
    }

    /// A scheduled run that waited for a permit past its grace period is skipped once
    /// the permit arrives. (review #7)
    #[tokio::test]
    async fn test_skip_is_checked_again_after_waiting_for_a_permit() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let mut task = Task::new("0 0 0 1 1 * 2099", None, None, Utc).unwrap();
        task.set_missed_runs(MissedRunPolicy::Skip {
            grace: Duration::from_secs(1),
        });
        task.add_step_default(move |_ctx| {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                Ok(Success)
            }
        });
        task.init();
        let links = SchedulerLinks::default();
        links.limiter.set_global(1);
        let shared = Arc::new(TaskShared::new(&task, links));
        let mut events = shared.links.events.subscribe();
        // Due right now, so the first check lets it through.
        task.next_exec = Some(Utc::now());
        let blocker = shared.links.limiter.acquire(None).await;

        let s = shared.clone();
        let join = tokio::spawn(async move {
            run_once(&mut task, &s, RunKind::Scheduled).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(shared.waiting.load(Ordering::SeqCst));
        tokio::time::sleep(Duration::from_millis(1400)).await;
        drop(blocker);
        join.await.unwrap();

        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(!shared.waiting.load(Ordering::SeqCst));
        assert!(drain_events(&mut events)
            .iter()
            .any(|e| matches!(e, SchedulerEvent::RunSkipped { .. })));
    }

    /// `waiting` is not reported when a permit is free. (review of P5)
    #[tokio::test]
    async fn test_waiting_not_set_when_permit_is_free() {
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        let links = SchedulerLinks::default();
        links.limiter.set_global(1);
        let shared = Arc::new(TaskShared::new(&task, links));
        let seen = Arc::new(AtomicBool::new(false));
        let (s, w) = (shared.clone(), seen.clone());
        task.add_step_default(move |_ctx| {
            let (s, w) = (s.clone(), w.clone());
            async move {
                w.store(s.waiting.load(Ordering::SeqCst), Ordering::SeqCst);
                Ok(Success)
            }
        });
        task.init();
        run_once(&mut task, &shared, RunKind::Triggered).await;
        assert!(!seen.load(Ordering::SeqCst));
        assert_eq!(shared.runs.load(Ordering::SeqCst), 1);
    }

    /// Permits are released and the run is recorded before `on_success` runs, so a
    /// slow callback does not hold up other runs. (review #8)
    #[tokio::test]
    async fn test_permits_released_before_callbacks() {
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        let links = SchedulerLinks::default();
        links.limiter.set_global(1);
        let shared = Arc::new(TaskShared::new(&task, links));
        let observed = Arc::new(Mutex::new(None));
        let (s, o) = (shared.clone(), observed.clone());
        task.add_step_default(|_ctx| async { Ok(Success) });
        task.set_callbacks(
            Some(boxed_callback(move |_| {
                let (s, o) = (s.clone(), o.clone());
                async move {
                    let permit_free = s.links.limiter.try_acquire(None).is_some();
                    let recorded = s.history.lock().unwrap()[0].outcome.clone();
                    *o.lock().unwrap() = Some((permit_free, recorded));
                }
            })),
            None,
            None,
        );
        task.init();
        run_once(&mut task, &shared, RunKind::Triggered).await;
        assert_eq!(
            *observed.lock().unwrap(),
            Some((true, Some(RunOutcome::Success)))
        );
    }

    /// A panicking step fails the attempt with the panic message; retries still apply.
    /// (review #3)
    #[tokio::test]
    async fn test_step_panic_becomes_error() {
        use crate::retry::RetryPolicy;
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut task = Task::new("* * * * * *", None, None, Utc).unwrap();
        task.set_retry_policy(RetryPolicy::fixed(1, Duration::from_millis(0)));
        task.add_step_default(move |_ctx| {
            // Panics while creating the future on the first attempt, succeeds after.
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("sync boom");
            }
            async { Ok(Success) }
        });
        task.add_step_default(|_ctx| async { panic!("async boom") });
        task.init();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.step_states[0].status, StepStatus::Succeeded);
        assert_eq!(task.step_states[0].attempts, 2);
        assert_eq!(task.step_states[1].status, StepStatus::Failed);
        assert_eq!(
            task.step_states[1].error.as_deref(),
            Some("step panicked: async boom")
        );
    }

    /// A cancelled run does not use up a repeat and leaves the status for the stop
    /// reason to decide. (review #5)
    #[tokio::test]
    async fn test_cancelled_run_keeps_repeat() {
        let mut task = Task::new("* * * * * *", None, Some(1), Utc).unwrap();
        task.add_step_default(|_ctx| async { Ok(Success) });
        task.init();
        task.cancel.cancel();
        assert!(task.run_task(0).await.is_ok());
        assert_eq!(task.repeats, Some(1));
        task.reschedule_and_notify().await;
        assert!(
            !task.is_terminal(),
            "no Completed transition after a cancel"
        );
    }
}

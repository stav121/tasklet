use crate::errors::{TaskError, TaskResult};
use crate::events::{SchedulerEvent, EVENT_CAPACITY};
use crate::generator::TaskGenerator;
use crate::limits::Limiter;
use crate::task::{
    run_task, FinishReason, RunKind, RunOutcome, RunRecord, SchedulerLinks, Status, StepState,
    Task, TaskCmd, TaskShared,
};
use crate::{scheduler_log, task_log};
use chrono::prelude::*;
use chrono::Utc;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The default time stopped tasks get to finish their current run.
const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(5);

/// How long to wait for an aborted driver to actually stop before closing its books.
const ABORT_SETTLE: Duration = Duration::from_millis(100);

/// A request sent from a [`TaskSpawner`] to a running scheduler asking it to add a task.
///
/// The optional `reply` channel carries the assigned id (or the rejection error) back
/// to the caller of [`TaskSpawner::spawn_get_id`].
struct SpawnRequest<T>
where
    T: TimeZone + Clone + Send + 'static,
{
    task: Task<T>,
    reply: Option<oneshot::Sender<Result<usize, TaskError>>>,
}

/// A cloneable handle for adding tasks to a *running* [`TaskScheduler`] at runtime.
///
/// The type-erased [`SchedulerHandle`] cannot add tasks because a `Task<T>` carries the
/// scheduler's timezone type `T` and its step closures. A `TaskSpawner<T>` keeps that
/// type, so it can hand fully-built tasks to the scheduler from anywhere (another task,
/// an API handler, a signal handler). Obtain one with [`TaskScheduler::spawner`] before
/// calling [`run`](TaskScheduler::run).
///
/// Spawned tasks are picked up as soon as the scheduler loop wakes, which a spawn
/// triggers, so they are only processed while the scheduler is running.
///
/// # Examples
///
/// ```
/// # use tasklet::{TaskBuilder, TaskScheduler};
/// # use std::time::Duration;
/// # tokio_test::block_on(async {
/// let mut scheduler = TaskScheduler::new(50, chrono::Local);
/// let spawner = scheduler.spawner();
///
/// // Add a task from another task once the scheduler is running.
/// tokio::spawn(async move {
///     let _ = spawner.spawn(
///         TaskBuilder::new(chrono::Local)
///             .every("* * * * * * *")
///             .add_step_default(|_ctx| async { Ok(tasklet::task::TaskStepStatusOk::Success) })
///             .build(),
///     );
/// });
///
/// scheduler.run_until(tokio::time::sleep(Duration::from_millis(150))).await;
/// # });
/// ```
pub struct TaskSpawner<T>
where
    T: TimeZone + Clone + Send + 'static,
{
    tx: mpsc::UnboundedSender<SpawnRequest<T>>,
    wake: Arc<Notify>,
}

// A manual `Clone` avoids an unnecessary `T: Clone` bound the derive would add: the
// fields are an `UnboundedSender` and an `Arc`, which are always cloneable.
impl<T> Clone for TaskSpawner<T>
where
    T: TimeZone + Clone + Send + 'static,
{
    fn clone(&self) -> Self {
        TaskSpawner {
            tx: self.tx.clone(),
            wake: self.wake.clone(),
        }
    }
}

impl<T> TaskSpawner<T>
where
    T: TimeZone + Clone + Send + 'static,
    <T as TimeZone>::Offset: Send,
{
    /// Queue a task to be added to the running scheduler.
    ///
    /// A build error in `task` is returned immediately. The task is otherwise queued
    /// fire-and-forget; if it is rejected on the scheduler side (duplicate name, unknown
    /// pool) the rejection is logged and published as
    /// [`SchedulerEvent::TaskRejected`]. Use [`spawn_get_id`](Self::spawn_get_id) to
    /// await the assigned id or the rejection.
    ///
    /// Returns [`TaskError::ExecutionError`] once the scheduler has stopped (its
    /// `run` or `run_until` returned).
    pub fn spawn(&self, task: TaskResult<Task<T>>) -> TaskResult<()> {
        let task = task?;
        self.tx
            .send(SpawnRequest { task, reply: None })
            .map_err(|_| TaskError::ExecutionError("scheduler is no longer running".to_string()))?;
        self.wake.notify_one();
        Ok(())
    }

    /// Queue a task and await the id the scheduler assigns it.
    ///
    /// A build error is returned immediately; a duplicate name is reported as
    /// [`TaskError::DuplicateTaskName`], an unknown pool as [`TaskError::UnknownPool`],
    /// and a stopped scheduler as [`TaskError::ExecutionError`].
    pub async fn spawn_get_id(&self, task: TaskResult<Task<T>>) -> TaskResult<usize> {
        let task = task?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(SpawnRequest {
                task,
                reply: Some(reply_tx),
            })
            .map_err(|_| TaskError::ExecutionError("scheduler is no longer running".to_string()))?;
        self.wake.notify_one();
        reply_rx.await.map_err(|_| {
            TaskError::ExecutionError("scheduler dropped the spawn request".to_string())
        })?
    }
}

/// The scheduler's record of one registered task.
///
/// Holds the sender used to dispatch runs, the shared state the task publishes, and
/// either the driver's join handle or, if the task was added outside a Tokio runtime,
/// the task itself until the scheduler starts.
struct TaskHandle<T>
where
    T: TimeZone + Send + 'static,
{
    id: usize,
    name: Option<String>,
    join: Option<JoinHandle<()>>,
    unstarted: Option<Task<T>>,
    sender: mpsc::Sender<TaskCmd>,
    shared: Arc<TaskShared>,
}

/// A task that was removed and is finishing its current run within the grace period.
struct Retiring {
    join: Option<JoinHandle<()>>,
    shared: Arc<TaskShared>,
    deadline: Instant,
    /// Set once the driver has been aborted; its books are closed when it has stopped,
    /// so a late write from the driver cannot overwrite the cancelled record.
    aborted: bool,
}

/// A cheaply-clonable entry shared with [`SchedulerHandle`] so it can observe and
/// control a task without touching the scheduler-private join handle. Rebuilt from the
/// live task set whenever the set changes.
#[derive(Clone, Debug)]
struct RegEntry {
    id: usize,
    name: Option<String>,
    sender: mpsc::Sender<TaskCmd>,
    shared: Arc<TaskShared>,
}

impl RegEntry {
    /// Compute a point-in-time [`TaskState`] snapshot from the shared cell.
    fn to_state(&self) -> TaskState {
        let (status, next_exec) = {
            let state = self.shared.state.lock().unwrap();
            (state.status.clone(), state.next_exec)
        };
        TaskState {
            id: self.id,
            name: self.name.clone(),
            status,
            next_exec,
            running: self.shared.running.load(Ordering::SeqCst),
            waiting: self.shared.waiting.load(Ordering::SeqCst),
            paused: self.shared.paused.load(Ordering::SeqCst),
            last_outcome: self.shared.last_outcome.lock().unwrap().clone(),
            run_count: self.shared.runs.load(Ordering::SeqCst),
        }
    }
}

/// A snapshot of a single task's state, as reported by [`SchedulerHandle`].
///
/// The values are read from the task's live shared state at the time of the call.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct TaskState {
    /// The task's id, as assigned by the scheduler.
    pub id: usize,
    /// The task's unique name, if one was set via [`TaskBuilder::name`](crate::TaskBuilder::name).
    pub name: Option<String>,
    /// The task's lifecycle status.
    pub status: Status,
    /// The task's next execution time, normalized to UTC (`None` if not scheduled).
    pub next_exec: Option<DateTime<Utc>>,
    /// Whether a run of this task is currently in progress.
    pub running: bool,
    /// Whether a run is waiting for a concurrency permit
    /// ([`TaskScheduler::max_concurrent_runs`] or a pool limit).
    pub waiting: bool,
    /// Whether the task is currently paused (its schedule is not dispatched).
    pub paused: bool,
    /// The outcome of the most recent finished run, if any. Kept even when the task's
    /// history is disabled.
    pub last_outcome: Option<RunOutcome>,
    /// The total number of runs started over the task's lifetime.
    pub run_count: usize,
}

/// A cloneable handle used to control and observe a running [`TaskScheduler`].
///
/// Obtain one with [`TaskScheduler::handle`] *before* calling
/// [`TaskScheduler::run`], then call [`SchedulerHandle::shutdown`] from anywhere
/// (another task, a signal handler, etc.) to request a graceful stop, query the
/// live task set with [`SchedulerHandle::task_count`] / [`SchedulerHandle::statuses`],
/// or receive changes as they happen with [`SchedulerHandle::subscribe`].
///
/// # Examples
///
/// ```
/// # use tasklet::TaskScheduler;
/// # tokio_test::block_on(async {
/// let mut scheduler = TaskScheduler::default(chrono::Utc);
/// let handle = scheduler.handle();
///
/// // Stop the scheduler shortly after it starts.
/// let stopper = tokio::spawn(async move {
///     handle.shutdown();
/// });
///
/// scheduler.run().await; // returns once the shutdown is requested
/// stopper.await.unwrap();
/// # });
/// ```
#[derive(Clone, Debug)]
pub struct SchedulerHandle {
    notify: Arc<Notify>,
    registry: Arc<Mutex<Vec<RegEntry>>>,
    events: broadcast::Sender<SchedulerEvent>,
    wake: Arc<Notify>,
}

impl SchedulerHandle {
    /// Request a graceful shutdown of the associated scheduler.
    ///
    /// The scheduler stops dispatching, asks every task to stop (steps see it through
    /// [`TaskContext::is_cancelled`](crate::TaskContext::is_cancelled)), waits up to its
    /// [grace period](TaskScheduler::grace_period) for runs in progress to finish, aborts
    /// any that are still running, and returns from [`TaskScheduler::run`]. Each task's
    /// `on_finish` callback fires with [`FinishReason::Shutdown`] unless it was aborted.
    ///
    /// Calling this before the scheduler starts is fine: the request is remembered and
    /// the loop exits right after it starts.
    pub fn shutdown(&self) {
        // `notify_one` stores a permit if there is no current waiter, so a shutdown
        // requested before `run()` reaches its await point is not lost.
        self.notify.notify_one();
    }

    /// Subscribe to the scheduler's [`SchedulerEvent`]s.
    ///
    /// The receiver gets every event published after this call. See
    /// [`events`](crate::events) for the delivery guarantees.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::{SchedulerEvent, TaskBuilder, TaskScheduler};
    /// # use tasklet::task::TaskStepStatusOk::Success;
    /// # tokio_test::block_on(async {
    /// let mut scheduler = TaskScheduler::new(50, chrono::Utc);
    /// let mut events = scheduler.handle().subscribe();
    /// scheduler
    ///     .add_task(
    ///         TaskBuilder::new(chrono::Utc)
    ///             .every("* * * * * * *")
    ///             .add_step_default(|_ctx| async { Ok(Success) })
    ///             .build(),
    ///     )
    ///     .unwrap();
    /// assert!(matches!(
    ///     events.recv().await,
    ///     Ok(SchedulerEvent::TaskAdded { task_id: 0, .. })
    /// ));
    /// # });
    /// ```
    pub fn subscribe(&self) -> broadcast::Receiver<SchedulerEvent> {
        self.events.subscribe()
    }

    /// The number of tasks currently registered with the scheduler.
    pub fn task_count(&self) -> usize {
        self.registry.lock().unwrap().len()
    }

    /// A snapshot of every registered task's [`TaskState`].
    pub fn statuses(&self) -> Vec<TaskState> {
        self.registry
            .lock()
            .unwrap()
            .iter()
            .map(RegEntry::to_state)
            .collect()
    }

    /// The full [`TaskState`] of the task with the given id, if it is still registered.
    pub fn state_of(&self, id: usize) -> Option<TaskState> {
        self.registry
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .map(RegEntry::to_state)
    }

    /// The [`Status`] of the task with the given id, if it is still registered.
    pub fn status_of(&self, id: usize) -> Option<Status> {
        self.state_of(id).map(|s| s.status)
    }

    /// The id of the task registered under the given name, if any.
    pub fn id_of_name(&self, name: &str) -> Option<usize> {
        self.registry
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.name.as_deref() == Some(name))
            .map(|e| e.id)
    }

    /// The [`Status`] of the task registered under the given name, if any.
    pub fn status_of_name(&self, name: &str) -> Option<Status> {
        self.id_of_name(name).and_then(|id| self.status_of(id))
    }

    /// The recent [`RunRecord`] history of the task with the given id, oldest first. A
    /// run in progress appears as a record without `finished_at` and `outcome`.
    ///
    /// Returns an empty vector if the task is unknown or has no retained history. Once
    /// the scheduler has stopped no task is registered any more; read the last run from
    /// [`FinishReport::last_run`](crate::FinishReport::last_run) or the
    /// [`SchedulerEvent::RunFinished`] events instead.
    pub fn history(&self, id: usize) -> Vec<RunRecord> {
        self.registry
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.shared.history.lock().unwrap().iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The per-step state of the task with the given id: live while a run is in
    /// progress, the final state of the most recent run otherwise.
    ///
    /// Returns an empty vector if the task is unknown.
    pub fn step_states(&self, id: usize) -> Vec<StepState> {
        self.registry
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.shared.steps.lock().unwrap().clone())
            .unwrap_or_default()
    }

    /// Pause the task with the given id: its schedule stops being dispatched until
    /// [`resume`](Self::resume) is called. A run already in progress is not interrupted,
    /// and [`trigger`](Self::trigger) still runs a paused task.
    ///
    /// Returns `true` if the task was found.
    pub fn pause(&self, id: usize) -> bool {
        self.with_entry(id, |e| {
            e.shared.paused.store(true, Ordering::SeqCst);
            e.shared.emit(SchedulerEvent::TaskPaused {
                task_id: e.id,
                task_name: e.name.clone(),
            });
        })
    }

    /// Resume a paused task so its schedule is dispatched again.
    ///
    /// The task's next execution time is not advanced while it is paused. If that time
    /// passed during the pause, the task's [`MissedRunPolicy`](crate::MissedRunPolicy)
    /// decides: the default runs it once straight away; `Skip` drops it if it is later
    /// than the grace period. Returns `true` if the task was found.
    pub fn resume(&self, id: usize) -> bool {
        let found = self.with_entry(id, |e| {
            e.shared.paused.store(false, Ordering::SeqCst);
            e.shared.emit(SchedulerEvent::TaskResumed {
                task_id: e.id,
                task_name: e.name.clone(),
            });
        });
        self.wake.notify_one();
        found
    }

    /// Run the task with the given id now, regardless of its schedule, whether it is
    /// paused, and its [`MissedRunPolicy`](crate::MissedRunPolicy). If a run is already
    /// in progress or about to start, one more run is queued after it (at most one
    /// queued). Returns `true` if the task was found.
    pub fn trigger(&self, id: usize) -> bool {
        self.with_entry(id, |e| {
            e.shared.emit(SchedulerEvent::TaskTriggered {
                task_id: e.id,
                task_name: e.name.clone(),
            });
            let shared = &e.shared;
            let claim = || {
                shared
                    .dispatched
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            };
            if claim() {
                // The task is idle, so its single-slot channel is empty.
                let _ = e.sender.try_send(TaskCmd::Run(RunKind::Triggered));
                return;
            }
            // A run is in flight: queue one more. The driver takes `pending` before it
            // releases `dispatched` and re-checks it afterwards, but it may have released
            // the flag between our claim attempt and this store, so try to claim again.
            // Whoever holds the flag consumes the request; the other side sees it gone.
            shared.pending.store(true, Ordering::SeqCst);
            if claim() {
                if shared.pending.swap(false, Ordering::SeqCst) {
                    let _ = e.sender.try_send(TaskCmd::Run(RunKind::Triggered));
                } else {
                    shared.dispatched.store(false, Ordering::SeqCst);
                }
            }
        })
    }

    /// Remove the task with the given id.
    ///
    /// The task is unregistered on the scheduler's next round. A run in progress is asked
    /// to stop (through [`TaskContext::is_cancelled`](crate::TaskContext::is_cancelled))
    /// and gets the scheduler's [grace period](TaskScheduler::grace_period) to finish
    /// before it is aborted. `on_finish` fires with [`FinishReason::Removed`]. Returns
    /// `true` if the task was found.
    pub fn remove(&self, id: usize) -> bool {
        let found = self.with_entry(id, |e| {
            e.shared.remove_requested.store(true, Ordering::SeqCst);
            e.shared.request_stop(FinishReason::Removed);
        });
        self.wake.notify_one();
        found
    }

    /// Pause the task registered under the given name. Returns `true` if found.
    pub fn pause_name(&self, name: &str) -> bool {
        self.id_of_name(name)
            .map(|id| self.pause(id))
            .unwrap_or(false)
    }

    /// Resume the task registered under the given name. Returns `true` if found.
    pub fn resume_name(&self, name: &str) -> bool {
        self.id_of_name(name)
            .map(|id| self.resume(id))
            .unwrap_or(false)
    }

    /// Trigger the task registered under the given name. Returns `true` if found.
    pub fn trigger_name(&self, name: &str) -> bool {
        self.id_of_name(name)
            .map(|id| self.trigger(id))
            .unwrap_or(false)
    }

    /// Remove the task registered under the given name. Returns `true` if found.
    pub fn remove_name(&self, name: &str) -> bool {
        self.id_of_name(name)
            .map(|id| self.remove(id))
            .unwrap_or(false)
    }

    /// Run `f` against the entry with the given id, if present. Returns whether it was found.
    fn with_entry<F>(&self, id: usize, f: F) -> bool
    where
        F: FnOnce(&RegEntry),
    {
        match self.registry.lock().unwrap().iter().find(|e| e.id == id) {
            Some(entry) => {
                f(entry);
                true
            }
            None => false,
        }
    }
}

/// Task scheduler and executor.
///
/// `TaskScheduler` is `Send`, so it can be moved into `tokio::spawn` and driven from a
/// background task.
pub struct TaskScheduler<T>
where
    T: TimeZone + Clone + Send + 'static,
{
    /// The registered tasks.
    handles: Vec<TaskHandle<T>>,
    /// Removed tasks that are still finishing their current run.
    retiring: Vec<Retiring>,
    /// The (optional) task generation function.
    task_gen: Option<TaskGenerator<T>>,
    /// The longest time the loop sleeps without re-checking, in ms.
    sleep: usize,
    /// The id that should be assigned to the next appended task.
    next_id: usize,
    /// The main timezone used for the scheduler.
    timezone: T,
    /// Notified when a graceful shutdown is requested.
    shutdown: Arc<Notify>,
    /// The observable/controllable registry shared with every [`SchedulerHandle`].
    registry: Arc<Mutex<Vec<RegEntry>>>,
    /// Sender cloned into every [`TaskSpawner`] handed out via [`spawner`](Self::spawner).
    spawn_tx: mpsc::UnboundedSender<SpawnRequest<T>>,
    /// Receiver drained each round for tasks queued by a [`TaskSpawner`].
    spawn_rx: mpsc::UnboundedReceiver<SpawnRequest<T>>,
    /// Resources shared with every task: event channel, wake-up signal, limits.
    links: SchedulerLinks,
    /// How long stopped tasks get to finish their current run before being aborted.
    grace: Duration,
}

/// `TaskScheduler` implementation.
impl<T> TaskScheduler<T>
where
    T: TimeZone + Clone + Send + 'static,
    <T as TimeZone>::Offset: Send,
{
    /// Create a new instance of `TaskSchedule` with default sleep and no tasks to execute.
    ///
    /// # Arguments
    ///
    /// * timezone - the scheduler's timezone.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::TaskScheduler;
    /// let _ = TaskScheduler::default(chrono::Utc);
    /// ```
    pub fn default(timezone: T) -> TaskScheduler<T> {
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        TaskScheduler {
            handles: Vec::new(),
            retiring: Vec::new(),
            /* Originally empty, no registered tasks. */
            task_gen: None,
            sleep: 1000,
            timezone,
            next_id: 0,
            shutdown: Arc::new(Notify::new()),
            registry: Arc::new(Mutex::new(Vec::new())),
            spawn_tx,
            spawn_rx,
            links: SchedulerLinks {
                events: broadcast::channel(EVENT_CAPACITY).0,
                wake: Arc::new(Notify::new()),
                limiter: Arc::new(Limiter::default()),
            },
            grace: DEFAULT_GRACE_PERIOD,
        }
    }

    /// Return a cloneable [`SchedulerHandle`] to control, observe and shut down the
    /// scheduler.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::TaskScheduler;
    /// let scheduler = TaskScheduler::default(chrono::Utc);
    /// let _handle = scheduler.handle();
    /// ```
    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            notify: self.shutdown.clone(),
            registry: self.registry.clone(),
            events: self.links.events.clone(),
            wake: self.links.wake.clone(),
        }
    }

    /// Return a cloneable [`TaskSpawner`] that can add tasks to this scheduler while it
    /// is running.
    ///
    /// Unlike [`SchedulerHandle`], a spawner keeps the scheduler's timezone type `T`, so
    /// it can hand fully-built `Task<T>` values to the running loop. Obtain it before
    /// calling [`run`](Self::run).
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::TaskScheduler;
    /// let scheduler = TaskScheduler::default(chrono::Utc);
    /// let _spawner = scheduler.spawner();
    /// ```
    pub fn spawner(&self) -> TaskSpawner<T> {
        TaskSpawner {
            tx: self.spawn_tx.clone(),
            wake: self.links.wake.clone(),
        }
    }

    /// Create a new instance of `TaskScheduler` with no tasks to execute.
    ///
    /// The scheduler wakes when the next task is due, so `sleep` does not delay runs. It
    /// is the longest the loop sleeps without re-checking the clock, which bounds how
    /// late a run can start after the system clock jumps forward.
    /// [`default`](Self::default) uses 1000 ms.
    ///
    /// # Arguments
    ///
    /// * sleep     - The maximum idle wait (in ms).
    /// * timezone  - The scheduler's timezone.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::TaskScheduler;
    /// // Re-check the clock at least once per second.
    /// let _ = TaskScheduler::new(1000, chrono::Local);
    /// ```
    pub fn new(sleep: usize, timezone: T) -> TaskScheduler<T> {
        TaskScheduler {
            sleep,
            ..TaskScheduler::default(timezone)
        }
    }

    /// Set how long tasks get to finish their current run when they are removed or the
    /// scheduler shuts down. A run still in progress after that is aborted and recorded
    /// as [`RunOutcome::Cancelled`]. Defaults to 5 seconds; `Duration::ZERO` aborts at
    /// once.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tasklet::TaskScheduler;
    /// let mut scheduler = TaskScheduler::default(chrono::Utc);
    /// scheduler.grace_period(Duration::from_secs(30));
    /// ```
    pub fn grace_period(&mut self, grace: Duration) -> &mut TaskScheduler<T> {
        self.grace = grace;
        self
    }

    /// Limit how many task runs may execute at the same time across the whole
    /// scheduler. Runs over the limit wait (reported as [`TaskState::waiting`]) until
    /// another run finishes. Values below 1 are treated as 1.
    ///
    /// The limit applies from each task's next run on, including tasks added earlier.
    /// Calling it again replaces the limit; runs holding a permit from the old limit
    /// keep it until they finish.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::TaskScheduler;
    /// let mut scheduler = TaskScheduler::default(chrono::Utc);
    /// scheduler.max_concurrent_runs(4);
    /// ```
    pub fn max_concurrent_runs(&mut self, max: usize) -> &mut TaskScheduler<T> {
        self.links.limiter.set_global(max);
        self
    }

    /// Create (or replace) a named concurrency pool. At most `max` runs of tasks in the
    /// pool (see [`TaskBuilder::pool`](crate::TaskBuilder::pool)) execute at the same
    /// time. A pool run also counts against [`max_concurrent_runs`](Self::max_concurrent_runs).
    /// Values below 1 are treated as 1.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::TaskScheduler;
    /// let mut scheduler = TaskScheduler::default(chrono::Utc);
    /// scheduler.add_pool("database", 2).add_pool("http", 8);
    /// ```
    pub fn add_pool(&mut self, name: &str, max: usize) -> &mut TaskScheduler<T> {
        self.links.limiter.set_pool(name, max);
        self
    }

    /// Set a `TaskGenerator` instance for the TaskScheduler.
    ///
    /// # Arguments
    ///
    /// * task_gen - a `TaskGenerator` instance.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::{TaskScheduler, TaskGenerator};
    /// // Create a new `TaskScheduler` instance and attach an `TaskGenerator` to it.
    /// let mut scheduler = TaskScheduler::default(chrono::Local);
    /// let generator = TaskGenerator::new("1 * * * * * *", chrono::Local, || None).unwrap();
    /// scheduler.set_task_gen(generator);
    /// ```
    pub fn set_task_gen(&mut self, task_gen: TaskGenerator<T>) -> &mut TaskScheduler<T> {
        self.task_gen = Some(task_gen);
        self
    }

    /// Add a new task in the execution queue.
    ///
    /// # Arguments
    ///
    /// * task - a `Task` instance.
    ///
    /// If the task has a name (set via [`TaskBuilder::name`](crate::TaskBuilder::name)),
    /// it must be unique within the scheduler; a duplicate is rejected with
    /// [`TaskError::DuplicateTaskName`]. A task in a pool that does not exist is rejected
    /// with [`TaskError::UnknownPool`].
    ///
    /// Tasks can be added outside a Tokio runtime; they start when the scheduler runs.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::{TaskScheduler, Task};
    /// // Create a new `TaskScheduler` and attach a task to it.
    /// let mut scheduler = TaskScheduler::default(chrono::Local);
    /// // Add a task that executes every second forever.
    /// scheduler.add_task(Task::new("* * * * * * *", None, None, chrono::Local)).unwrap();
    /// ```
    pub fn add_task(
        &mut self,
        task: TaskResult<Task<T>>,
    ) -> Result<&mut TaskScheduler<T>, TaskError> {
        self.add_task_get_id(task)?;
        Ok(self)
    }

    /// Add a new task and return the id the scheduler assigned to it.
    ///
    /// This is the same as [`add_task`](Self::add_task) except it returns the new
    /// task's id instead of `&mut Self`, so callers can address an unnamed task
    /// afterwards through a [`SchedulerHandle`] (pause, resume, trigger, remove).
    ///
    /// The task is visible through every [`SchedulerHandle`] as soon as this returns,
    /// with its first execution time already computed.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::{TaskScheduler, Task};
    /// let mut scheduler = TaskScheduler::default(chrono::Local);
    /// let id = scheduler
    ///     .add_task_get_id(Task::new("* * * * * * *", None, None, chrono::Local))
    ///     .unwrap();
    /// assert_eq!(id, 0);
    /// ```
    pub fn add_task_get_id(&mut self, task: TaskResult<Task<T>>) -> Result<usize, TaskError> {
        let mut task = task?;

        // Enforce name uniqueness so a name is a stable, addressable identity.
        if let Some(name) = task.name.as_deref() {
            if self.handles.iter().any(|h| h.name.as_deref() == Some(name)) {
                return Err(TaskError::DuplicateTaskName(name.to_string()));
            }
        }
        if let Some(pool) = task.pool.as_deref() {
            if !self.links.limiter.has_pool(pool) {
                return Err(TaskError::UnknownPool(pool.to_string()));
            }
        }

        let id = self.next_id;

        // A buffer of one is enough: a `Run` is only sent by whoever flips the task's
        // `dispatched` flag, so at most one is ever in flight.
        let (sender, receiver) = mpsc::channel(1);

        task.set_receiver(receiver);
        task.set_id(id);
        // Initialize here rather than in the task's driver so the first execution time
        // is observable immediately, even before the scheduler runs.
        task.init();
        let name = task.name.clone();
        let shared = Arc::new(TaskShared::new(&task, self.links.clone()));

        let (join, unstarted) = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => (Some(runtime.spawn(run_task(task, shared.clone()))), None),
            Err(_) => (None, Some(task)),
        };

        self.handles.push(TaskHandle {
            id,
            name: name.clone(),
            join,
            unstarted,
            sender,
            shared,
        });
        self.next_id += 1;

        let _ = self.links.events.send(SchedulerEvent::TaskAdded {
            task_id: id,
            task_name: name,
        });
        self.refresh_registry();
        self.links.wake.notify_one();
        Ok(id)
    }

    /// Spawn the drivers of tasks that were added outside a Tokio runtime.
    fn start_unstarted(&mut self) {
        for handle in &mut self.handles {
            if let Some(mut task) = handle.unstarted.take() {
                // The first execution time was computed when the task was added, which
                // may be long before the scheduler started; recompute it from now.
                task.status = Status::Init;
                handle.join = Some(tokio::spawn(run_task(task, handle.shared.clone())));
            }
        }
    }

    /// Log and publish a task that could not be added.
    fn reject(&self, task_name: Option<String>, error: &TaskError, source: &str) {
        scheduler_log!(
            log::Level::Warn,
            "Rejected a task from the {} ({}): {}",
            source,
            task_name.as_deref().unwrap_or("unnamed"),
            error
        );
        let _ = self.links.events.send(SchedulerEvent::TaskRejected {
            task_name,
            reason: error.to_string(),
        });
    }

    /// Add every task queued by a [`TaskSpawner`] since the last round, replying with the
    /// assigned id (or the rejection error) to any caller awaiting one.
    fn drain_spawns(&mut self) {
        while let Ok(request) = self.spawn_rx.try_recv() {
            let task_name = request.task.name.clone();
            let result = self.add_task_get_id(Ok(request.task));
            if let Err(error) = &result {
                self.reject(task_name, error, "spawner");
            }
            if let Some(reply) = request.reply {
                // The receiver may have gone away; that is fine.
                let _ = reply.send(result);
            }
        }
    }

    /// Reap tasks that have stopped, start the retirement of removed tasks, abort
    /// retiring tasks whose grace period ran out, and dispatch a run to every idle task
    /// that is due. Dispatch is fire-and-forget: the scheduler never waits for a run,
    /// so a slow task cannot delay any other task's schedule.
    fn dispatch_round(&mut self) {
        let now = Utc::now();
        let instant_now = Instant::now();

        // Split off tasks that finished on their own or were asked to leave.
        let mut kept = Vec::with_capacity(self.handles.len());
        for handle in std::mem::take(&mut self.handles) {
            if handle.shared.finished.load(Ordering::SeqCst) {
                task_log!(handle.id, log::Level::Debug, "Removing finished task");
            } else if handle.join.as_ref().is_some_and(|j| j.is_finished()) {
                // The driver stopped without finishing, which only a panic inside the
                // scheduler machinery can cause (steps and callbacks are guarded).
                task_log!(
                    handle.id,
                    log::Level::Error,
                    "Task driver stopped unexpectedly, removing the task"
                );
                handle.shared.abandon_run(now);
                handle.shared.report_removed(FinishReason::Panicked);
            } else if handle.shared.remove_requested.load(Ordering::SeqCst) {
                task_log!(handle.id, log::Level::Debug, "Removing task on request");
                if handle.join.is_none() {
                    // Never started, so there is no run to wait for.
                    handle.shared.report_removed(FinishReason::Removed);
                }
                self.retiring.push(Retiring {
                    join: handle.join,
                    shared: handle.shared,
                    deadline: instant_now + self.grace,
                    aborted: false,
                });
            } else {
                kept.push(handle);
            }
        }
        self.handles = kept;

        // Abort removed tasks that did not stop within the grace period.
        self.retiring.retain_mut(|retiring| {
            let done = retiring.join.as_ref().is_none_or(|j| j.is_finished());
            if done {
                // A driver that finished normally already closed its books; these calls
                // only matter after an abort or a crash.
                if retiring.aborted || !retiring.shared.finished.load(Ordering::SeqCst) {
                    close_books(&retiring.shared, now);
                }
                return false;
            }
            if !retiring.aborted && instant_now >= retiring.deadline {
                task_log!(
                    retiring.shared.id,
                    log::Level::Warn,
                    "Did not stop within the grace period, aborting"
                );
                if let Some(join) = &retiring.join {
                    join.abort();
                }
                retiring.aborted = true;
                // Look again shortly, once the abort has taken effect.
                retiring.deadline = instant_now + ABORT_SETTLE;
            }
            true
        });

        // Dispatch due tasks.
        for handle in &self.handles {
            let shared = &handle.shared;
            if handle.join.is_none()
                || shared.paused.load(Ordering::SeqCst)
                || shared.running.load(Ordering::SeqCst)
            {
                continue;
            }
            let due = match shared.state.lock().unwrap().next_exec {
                Some(next) => now >= next,
                None => false,
            };
            if !due {
                continue;
            }
            // Whoever flips `dispatched` sends the run, so a concurrent trigger cannot
            // queue a duplicate.
            if shared
                .dispatched
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let _ = handle.sender.try_send(TaskCmd::Run(RunKind::Scheduled));
            }
        }

        self.refresh_registry();
    }

    /// Rebuild the shared registry from the live task set so every [`SchedulerHandle`]
    /// observes and controls the current tasks.
    fn refresh_registry(&self) {
        let snapshot = self
            .handles
            .iter()
            .map(|handle| RegEntry {
                id: handle.id,
                name: handle.name.clone(),
                sender: handle.sender.clone(),
                shared: handle.shared.clone(),
            })
            .collect();
        *self.registry.lock().unwrap() = snapshot;
    }

    /// Execute the `TaskGenerator` instance (if set and due).
    ///
    /// A generated task is added like any other; one that cannot be added is logged
    /// and published as [`SchedulerEvent::TaskRejected`].
    fn run_task_gen(&mut self) -> bool {
        let generated = match self.task_gen {
            Some(ref mut tg) if tg.next_exec <= Utc::now().with_timezone(&self.timezone) => {
                tg.run()
            }
            _ => return false,
        };
        if self.task_gen.as_ref().is_some_and(|tg| tg.exhausted) {
            scheduler_log!(
                log::Level::Info,
                "Task generator has no further execution and was removed"
            );
            self.task_gen = None;
        }
        match generated {
            Some(Ok(task)) => {
                let task_name = task.name.clone();
                match self.add_task_get_id(Ok(task)) {
                    Ok(_) => true,
                    Err(error) => {
                        self.reject(task_name, &error, "generator");
                        false
                    }
                }
            }
            Some(Err(error)) => {
                self.reject(None, &error, "generator");
                false
            }
            None => false,
        }
    }

    /// When the loop must next wake up: the earliest due time of an idle task, the
    /// generator, or a retiring task's deadline, capped at the maximum idle wait.
    fn next_wake(&self) -> Instant {
        let instant_now = Instant::now();
        let mut wake = instant_now + Duration::from_millis(self.sleep as u64);

        let utc_now = Utc::now();
        let to_instant = |at: DateTime<Utc>| {
            // A time in the past means "now"; the round that follows dispatches it.
            instant_now
                + at.signed_duration_since(utc_now)
                    .to_std()
                    .unwrap_or_default()
        };

        for handle in &self.handles {
            let shared = &handle.shared;
            // Busy and paused tasks report back through the wake signal when they
            // become dispatchable, so they do not need a timer.
            if handle.join.is_none()
                || shared.paused.load(Ordering::SeqCst)
                || shared.running.load(Ordering::SeqCst)
                || shared.dispatched.load(Ordering::SeqCst)
            {
                continue;
            }
            if let Some(next) = shared.state.lock().unwrap().next_exec {
                wake = wake.min(to_instant(next));
            }
        }
        if let Some(tg) = &self.task_gen {
            wake = wake.min(to_instant(tg.next_exec.with_timezone(&Utc)));
        }
        for retiring in &self.retiring {
            wake = wake.min(retiring.deadline);
        }
        wake
    }

    /// Stop every task: ask each to stop, wait up to the grace period for runs in
    /// progress, then abort whatever is left.
    async fn drain(&mut self) {
        for handle in &self.handles {
            handle.shared.request_stop(FinishReason::Shutdown);
        }
        for retiring in &self.retiring {
            retiring.shared.cancel.cancel();
        }

        let deadline = Instant::now() + self.grace;
        let drivers = std::mem::take(&mut self.handles)
            .into_iter()
            .map(|h| (h.join, h.shared, deadline))
            .chain(
                std::mem::take(&mut self.retiring)
                    .into_iter()
                    // A removed task keeps the deadline it already had.
                    .map(|r| (r.join, r.shared, r.deadline.min(deadline))),
            );
        for (join, shared, deadline) in drivers {
            let Some(mut join) = join else {
                // Never started.
                shared.report_removed(shared.stop_reason());
                continue;
            };
            match tokio::time::timeout_at(deadline, &mut join).await {
                Ok(Ok(())) => {}
                // The driver panicked or was aborted earlier.
                Ok(Err(_)) => close_books(&shared, Utc::now()),
                Err(_) => {
                    task_log!(
                        shared.id,
                        log::Level::Warn,
                        "Did not stop within the grace period, aborting"
                    );
                    join.abort();
                    // Let the abort take effect so the driver cannot write after the
                    // books are closed.
                    let _ = tokio::time::timeout(ABORT_SETTLE, &mut join).await;
                    close_books(&shared, Utc::now());
                }
            }
        }
        self.registry.lock().unwrap().clear();

        // A spawner used after this point gets an error instead of a request that is
        // never processed.
        self.spawn_rx.close();
        while let Ok(request) = self.spawn_rx.try_recv() {
            if let Some(reply) = request.reply {
                let _ = reply.send(Err(TaskError::ExecutionError(
                    "scheduler is no longer running".to_string(),
                )));
            }
        }
    }

    /// Run one iteration of the scheduler flow: start tasks added outside the runtime,
    /// run the generator (if due), add spawned tasks and dispatch the current round.
    fn tick(&mut self) {
        self.start_unstarted();
        self.run_task_gen();
        self.drain_spawns();
        self.dispatch_round();
    }

    /// Main execution loop.
    ///
    /// Executes the main flow of the scheduler until a shutdown is requested through a
    /// [`SchedulerHandle`] obtained via [`TaskScheduler::handle`]. If no handle is ever
    /// used to request a shutdown, this runs forever.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::TaskScheduler;
    /// # tokio_test::block_on(async {
    /// let mut scheduler = TaskScheduler::default(chrono::Utc);
    /// let handle = scheduler.handle();
    /// tokio::spawn(async move { handle.shutdown(); });
    /// scheduler.run().await;
    /// # });
    /// ```
    pub async fn run(&mut self) {
        let shutdown = self.shutdown.clone();
        self.run_until(async move { shutdown.notified().await })
            .await;
    }

    /// Run the scheduler until the provided `shutdown` future resolves.
    ///
    /// This is the general form of [`TaskScheduler::run`]; it lets callers drive the
    /// shutdown with any future, for example an OS signal such as `ctrl_c`. When the
    /// future resolves the scheduler stops gracefully, as described on
    /// [`SchedulerHandle::shutdown`], and this returns.
    ///
    /// The loop sleeps until the next task is due and wakes early when a task is added,
    /// spawned, resumed, removed or finishes a run, so runs start on time rather than
    /// on the next polling tick.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::TaskScheduler;
    /// # use std::time::Duration;
    /// # tokio_test::block_on(async {
    /// let mut scheduler = TaskScheduler::new(50, chrono::Utc);
    /// // Stop after a short while.
    /// scheduler
    ///     .run_until(tokio::time::sleep(Duration::from_millis(120)))
    ///     .await;
    /// # });
    /// ```
    pub async fn run_until<F>(&mut self, shutdown: F)
    where
        F: Future<Output = ()>,
    {
        scheduler_log!(
            log::Level::Info,
            "Scheduler started with {} task(s) in queue",
            self.handles.len()
        );
        let _ = self.links.events.send(SchedulerEvent::SchedulerStarted);

        tokio::pin!(shutdown);
        let wake = self.links.wake.clone();

        loop {
            self.tick();
            let deadline = self.next_wake();
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    scheduler_log!(log::Level::Info, "Shutdown requested, stopping scheduler");
                    break;
                }
                _ = wake.notified() => {}
                _ = tokio::time::sleep_until(deadline) => {}
            }
        }

        self.drain().await;
        scheduler_log!(log::Level::Info, "Scheduler stopped");
        let _ = self.links.events.send(SchedulerEvent::SchedulerStopped);
    }
}

/// Close the books of a task whose driver was aborted or stopped unexpectedly: a run
/// in progress is recorded as cancelled and the task's removal is published.
fn close_books(shared: &TaskShared, now: DateTime<Utc>) {
    shared.abandon_run(now);
    shared.report_removed(shared.stop_reason());
    shared.finished.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::task::TaskStepStatusErr::ErrorDelete;
    use crate::task::TaskStepStatusOk::Success;
    use crate::task::{MissedRunPolicy, OverlapPolicy, StepStatus, TaskStepStatusErr};
    use crate::TaskBuilder;
    use chrono::Local;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// Build a task that increments `counter` once per run.
    fn counting_task(
        counter: &Arc<AtomicUsize>,
        repeats: Option<usize>,
    ) -> TaskResult<Task<Local>> {
        let c = counter.clone();
        let mut builder = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .add_step_default(move |_ctx| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(Success)
                }
            });
        if let Some(r) = repeats {
            builder = builder.repeat(r);
        }
        builder.build()
    }

    /// Build a task that counts its runs and is never due on its own (2099), for tests
    /// that must control every dispatch.
    fn never_due_counting_task(counter: &Arc<AtomicUsize>) -> TaskResult<Task<Local>> {
        let c = counter.clone();
        TaskBuilder::new(Local)
            .every("0 0 0 1 1 * 2099")
            .add_step_default(move |_ctx| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(Success)
                }
            })
            .build()
    }

    /// A finite task runs the expected number of times and is then reaped. (X1)
    #[tokio::test]
    async fn test_scheduler_runs_finite_task_and_reaps() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler
            .add_task(counting_task(&counter, Some(1)))
            .unwrap();

        let handle = scheduler.handle();
        let observed = Arc::new(AtomicUsize::new(usize::MAX));
        let obs = observed.clone();
        let observer = tokio::spawn(async move {
            // After the single run has completed, the task must be gone.
            tokio::time::sleep(Duration::from_millis(1600)).await;
            obs.store(handle.task_count(), Ordering::SeqCst);
        });

        tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.run_until(tokio::time::sleep(Duration::from_millis(2000))),
        )
        .await
        .expect("scheduler did not stop");
        observer.await.unwrap();

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "task should run exactly once"
        );
        assert_eq!(
            observed.load(Ordering::SeqCst),
            0,
            "finished task should be reaped"
        );
    }

    /// A slow-running task must not delay other tasks' schedules. (X1 headline)
    #[tokio::test]
    async fn test_slow_task_does_not_block_others() {
        let slow = Arc::new(AtomicUsize::new(0));
        let fast = Arc::new(AtomicUsize::new(0));

        let mut scheduler = TaskScheduler::new(100, Local);

        // A slow task: records that it started, then blocks for two seconds.
        let s = slow.clone();
        let slow_task = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .add_step_default(move |_ctx| {
                let s = s.clone();
                async move {
                    s.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Ok(Success)
                }
            })
            .build();
        scheduler.add_task(slow_task).unwrap();
        // A fast task on the same one-second cadence.
        scheduler.add_task(counting_task(&fast, None)).unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.run_until(tokio::time::sleep(Duration::from_millis(2500))),
        )
        .await
        .expect("scheduler did not stop");

        // The fast task keeps ticking even while the slow one is blocked.
        assert!(
            fast.load(Ordering::SeqCst) >= 2,
            "fast task was blocked by the slow one: {} runs",
            fast.load(Ordering::SeqCst)
        );
        // The slow task started once; Skip (default) prevents a second overlapping run.
        assert_eq!(
            slow.load(Ordering::SeqCst),
            1,
            "slow task should not overlap itself under Skip"
        );
    }

    /// `dispatch_round` never sends a run to a task whose previous run is still in
    /// progress; overlap is decided by the task itself when the run ends. (X1)
    #[tokio::test]
    async fn test_dispatch_skips_running_task() {
        let mut scheduler = TaskScheduler::new(1000, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();

        // Simulate a due task whose previous run is still in progress.
        {
            let shared = &scheduler.handles[0].shared;
            shared.running.store(true, Ordering::SeqCst);
            shared.state.lock().unwrap().next_exec =
                Some(Utc::now() - chrono::Duration::seconds(1));
        }
        scheduler.dispatch_round();

        let shared = &scheduler.handles[0].shared;
        assert!(!shared.dispatched.load(Ordering::SeqCst));
        assert!(!shared.pending.load(Ordering::SeqCst));
    }

    /// `dispatch_round` dispatches a run to an idle, due task. (X1)
    #[tokio::test]
    async fn test_dispatch_runs_idle_due_task() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(1000, Local);
        scheduler.add_task(counting_task(&counter, None)).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Force the task due while idle.
        scheduler.handles[0].shared.state.lock().unwrap().next_exec =
            Some(Utc::now() - chrono::Duration::seconds(1));
        scheduler.dispatch_round();

        // Give the dispatched run a moment to execute.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// A force-removed task is reaped without disturbing other tasks. (X1)
    #[tokio::test]
    async fn test_scheduler_force_removal_isolated() {
        let survivor = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);

        let mut doomed = Task::new("* * * * * * *", None, None, Local).unwrap();
        doomed.add_step_default(|_ctx| async { Err(ErrorDelete("fatal".to_string())) });
        scheduler.add_task(Ok(doomed)).unwrap();
        scheduler.add_task(counting_task(&survivor, None)).unwrap();

        let handle = scheduler.handle();
        let observed = Arc::new(AtomicUsize::new(usize::MAX));
        let obs = observed.clone();
        let observer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1600)).await;
            obs.store(handle.task_count(), Ordering::SeqCst);
        });

        tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.run_until(tokio::time::sleep(Duration::from_millis(2000))),
        )
        .await
        .expect("scheduler did not stop");
        observer.await.unwrap();

        // Only the surviving task remains, and it kept running.
        assert_eq!(
            observed.load(Ordering::SeqCst),
            1,
            "doomed task should be reaped"
        );
        assert!(survivor.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn test_scheduler_with_generator() {
        // Create a new scheduler instance.
        let mut scheduler = TaskScheduler::new(500, Local);

        // Add a task generator function that does now.
        scheduler.set_task_gen(TaskGenerator::new("* * * * * * *", Local, || None).unwrap());

        // Should start with zero tasks.
        assert_eq!(scheduler.handles.len(), 0);

        // Execute the task generator.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        scheduler.run_task_gen();

        // The number of tasks should be zero again.
        assert_eq!(scheduler.handles.len(), 0);

        // Update the generator to actually create a new task.
        scheduler.set_task_gen(
            TaskGenerator::new("* * * * * * *", Local, || {
                // Run at second "1" of every minute.

                // Create the task that will execute 2 total times.
                // Return the task for the execution queue.
                Some(TaskBuilder::new(Local).every("* * * * * * *").build())
            })
            .unwrap(),
        );

        // Execute the task generator.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        scheduler.run_task_gen();

        // The number of tasks should be zero again.
        assert_eq!(scheduler.handles.len(), 1);
    }

    #[test]
    fn test_task_builder_invalid_schedule() {
        // Test with valid schedule
        let result = TaskBuilder::new(chrono::Utc).every("* * * * * * *").build();
        assert!(result.is_ok());

        // Test with invalid schedule
        let result = TaskBuilder::new(chrono::Utc).every("invalid cron").build();
        assert!(result.is_err());

        // Test that the error is the correct type
        match result {
            Err(TaskError::InvalidCronExpression(_)) => {} // Expected
            _ => panic!("Expected InvalidCronExpression error"),
        }
    }

    /// `run()` must return once a shutdown is requested through the handle.
    #[tokio::test]
    async fn test_scheduler_graceful_shutdown() {
        let mut scheduler = TaskScheduler::new(50, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();

        let handle = scheduler.handle();
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            handle.shutdown();
        });

        // Should return in bounded time rather than looping forever.
        tokio::time::timeout(Duration::from_secs(5), scheduler.run())
            .await
            .expect("scheduler did not stop after shutdown was requested");
        stopper.await.unwrap();

        // Tasks are drained on shutdown.
        assert_eq!(scheduler.handles.len(), 0);
    }

    /// A shutdown requested before `run()` starts must still stop the scheduler.
    #[tokio::test]
    async fn test_scheduler_shutdown_requested_before_run() {
        let mut scheduler = TaskScheduler::new(50, Local);
        let handle = scheduler.handle();
        // Request shutdown up-front; the stored notification must not be lost.
        handle.shutdown();

        tokio::time::timeout(Duration::from_secs(5), scheduler.run())
            .await
            .expect("scheduler ignored a shutdown requested before run()");
    }

    /// `run_until` stops when the provided future resolves.
    #[tokio::test]
    async fn test_scheduler_run_until() {
        let mut scheduler = TaskScheduler::new(50, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.run_until(tokio::time::sleep(Duration::from_millis(120))),
        )
        .await
        .expect("run_until did not stop when its shutdown future resolved");
        assert_eq!(scheduler.handles.len(), 0);
    }

    /// The handle reports the live task count and per-task status while running. (O1)
    #[tokio::test]
    async fn test_handle_status_queries() {
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap()
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();

        let handle = scheduler.handle();
        // The registry is populated at `add_task` time, so both tasks are visible
        // through the handle before `run()` is ever called.
        assert_eq!(handle.task_count(), 2);

        let probe = handle.clone();
        let observer = tokio::spawn(async move {
            // Once the tasks have initialized and been observed at least once.
            tokio::time::sleep(Duration::from_millis(500)).await;
            (
                probe.task_count(),
                probe.statuses(),
                probe.status_of(0),
                probe.status_of(1),
                probe.status_of(99),
            )
        });

        tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.run_until(tokio::time::sleep(Duration::from_millis(800))),
        )
        .await
        .expect("scheduler did not stop");
        let (count, statuses, s0, s1, s99) = observer.await.unwrap();

        assert_eq!(count, 2);
        assert_eq!(statuses.len(), 2);
        assert!(statuses.iter().all(|t| t.status == Status::Scheduled));
        assert!(statuses.iter().all(|t| t.next_exec.is_some()));
        assert_eq!(s0, Some(Status::Scheduled));
        assert_eq!(s1, Some(Status::Scheduled));
        assert_eq!(s99, None);
    }

    /// A finished task drops out of the handle's snapshot. (O1)
    #[tokio::test]
    async fn test_handle_status_drops_finished_task() {
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, Some(1), Local))
            .unwrap();
        let handle = scheduler.handle();

        let probe = handle.clone();
        let observed = Arc::new(AtomicUsize::new(usize::MAX));
        let obs = observed.clone();
        let observer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1600)).await;
            obs.store(probe.task_count(), Ordering::SeqCst);
        });

        tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.run_until(tokio::time::sleep(Duration::from_millis(2000))),
        )
        .await
        .expect("scheduler did not stop");
        observer.await.unwrap();

        assert_eq!(observed.load(Ordering::SeqCst), 0);
        assert_eq!(handle.status_of(0), None);
    }

    /// A duplicate task name is rejected. (Layer 0)
    #[tokio::test]
    async fn test_add_task_rejects_duplicate_name() {
        let mut scheduler = TaskScheduler::new(1000, Local);
        let t1 = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .name("dup")
            .build();
        let t2 = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .name("dup")
            .build();
        scheduler.add_task(t1).unwrap();
        match scheduler.add_task(t2) {
            Err(TaskError::DuplicateTaskName(name)) => assert_eq!(name, "dup"),
            _ => panic!("expected DuplicateTaskName error"),
        }
        // A different name is accepted.
        let t3 = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .name("other")
            .build();
        assert!(scheduler.add_task(t3).is_ok());
    }

    /// `add_task_get_id` returns the assigned id and still enforces name uniqueness. (§4.1)
    #[tokio::test]
    async fn test_add_task_get_id_returns_ids() {
        let mut scheduler = TaskScheduler::new(1000, Local);
        let id0 = scheduler
            .add_task_get_id(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        let id1 = scheduler
            .add_task_get_id(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);

        // A propagated build error surfaces rather than assigning an id.
        assert!(scheduler
            .add_task_get_id(Task::new("not a cron", None, None, Local))
            .is_err());

        // Duplicate names are still rejected.
        let named = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .name("solo")
            .build();
        assert_eq!(scheduler.add_task_get_id(named).unwrap(), 2);
        let dup = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .name("solo")
            .build();
        assert!(matches!(
            scheduler.add_task_get_id(dup),
            Err(TaskError::DuplicateTaskName(_))
        ));
    }

    /// Pausing a task stops its schedule from being dispatched; resuming restores it. (Layer 0)
    #[tokio::test]
    async fn test_control_pause_and_resume() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(1000, Local);
        // Never due on its own, so only the forced times below can dispatch it. A
        // per-second schedule could come due during the sleep on a slow machine.
        scheduler
            .add_task(never_due_counting_task(&counter))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Populate the registry so the handle can address the task.
        scheduler.dispatch_round();

        let handle = scheduler.handle();
        assert!(handle.pause(0));

        // Force the task due and dispatch: a paused task must not run.
        scheduler.handles[0].shared.state.lock().unwrap().next_exec =
            Some(Utc::now() - chrono::Duration::seconds(1));
        scheduler.dispatch_round();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "paused task must not run"
        );
        assert!(handle.state_of(0).unwrap().paused);

        // Resume and dispatch: the task runs.
        assert!(handle.resume(0));
        scheduler.handles[0].shared.state.lock().unwrap().next_exec =
            Some(Utc::now() - chrono::Duration::seconds(1));
        scheduler.dispatch_round();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1, "resumed task should run");
    }

    /// Triggering runs an idle task immediately, regardless of its schedule. (Layer 0)
    #[tokio::test]
    async fn test_control_trigger_runs_idle_task() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(1000, Local);
        // Never due on its own, so the only run is the triggered one.
        scheduler
            .add_task(never_due_counting_task(&counter))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Populate the registry.
        scheduler.dispatch_round();

        let handle = scheduler.handle();
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(handle.trigger(0));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "trigger should run the task now"
        );
        // Triggering an unknown id reports not-found.
        assert!(!handle.trigger(99));
    }

    /// Requesting removal reaps the task on the next round. (Layer 0)
    #[tokio::test]
    async fn test_control_remove_reaps_task() {
        let mut scheduler = TaskScheduler::new(1000, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        scheduler.dispatch_round();

        let handle = scheduler.handle();
        assert_eq!(handle.task_count(), 1);
        assert!(handle.remove(0));
        // Next round reaps it.
        scheduler.dispatch_round();
        assert_eq!(scheduler.handles.len(), 0);
        assert_eq!(handle.task_count(), 0);
        assert!(!handle.remove(0));
    }

    /// The handle resolves names and exposes run history and step states. (Layer 0)
    #[tokio::test]
    async fn test_handle_name_history_and_steps() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        let c = counter.clone();
        let task = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .name("worker")
            .add_step("do work", move |_ctx| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(Success)
                }
            })
            .build();
        scheduler.add_task(task).unwrap();

        let probe = scheduler.handle();
        let observer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1600)).await;
            let id = probe.id_of_name("worker");
            let status = probe.status_of_name("worker");
            let history = id.map(|i| probe.history(i)).unwrap_or_default();
            let steps = id.map(|i| probe.step_states(i)).unwrap_or_default();
            (id, status, history, steps)
        });

        tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.run_until(tokio::time::sleep(Duration::from_millis(2000))),
        )
        .await
        .expect("scheduler did not stop");
        let (id, status, history, steps) = observer.await.unwrap();

        assert_eq!(id, Some(0));
        assert!(status.is_some());
        assert!(!history.is_empty(), "history should have records");
        assert!(history
            .iter()
            .any(|r| r.outcome == Some(RunOutcome::Success)));
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].description.as_deref(), Some("do work"));
        assert_eq!(steps[0].status, StepStatus::Succeeded);
        assert!(counter.load(Ordering::SeqCst) >= 1);
        // Unknown name resolves to nothing.
        assert_eq!(scheduler.handle().id_of_name("nope"), None);
    }

    /// `TaskState` carries the new observable fields (name, run_count, last_outcome). (Layer 0)
    #[tokio::test]
    async fn test_task_state_exposes_new_fields() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        let c = counter.clone();
        let task = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .name("named")
            .add_step_default(move |_ctx| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(Success)
                }
            })
            .build();
        scheduler.add_task(task).unwrap();

        let probe = scheduler.handle();
        let observer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1600)).await;
            probe.state_of(0)
        });

        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(2000)))
            .await;
        let state = observer.await.unwrap().expect("state should exist");

        assert_eq!(state.name.as_deref(), Some("named"));
        assert!(state.run_count >= 1);
        assert_eq!(state.last_outcome, Some(RunOutcome::Success));
        assert!(!state.paused);
    }

    /// `TaskState` round-trips through serde. (Layer 0, `serde` feature)
    #[cfg(feature = "serde")]
    #[test]
    fn test_serde_roundtrip_task_state() {
        let state = TaskState {
            id: 1,
            name: Some("t".to_string()),
            status: Status::Scheduled,
            next_exec: None,
            running: false,
            waiting: false,
            paused: true,
            last_outcome: Some(RunOutcome::Success),
            run_count: 5,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: TaskState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 1);
        assert_eq!(back.name.as_deref(), Some("t"));
        assert_eq!(back.status, Status::Scheduled);
        assert!(back.paused);
        assert_eq!(back.last_outcome, Some(RunOutcome::Success));
        assert_eq!(back.run_count, 5);
    }

    /// A `TaskSpawner` adds a task to a running scheduler, which then executes it. (0.5.0)
    #[tokio::test]
    async fn test_spawner_adds_task_at_runtime() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        let spawner = scheduler.spawner();
        let handle = scheduler.handle();

        // Nothing registered up front.
        assert_eq!(handle.task_count(), 0);

        let c = counter.clone();
        let injector = tokio::spawn(async move {
            // Let the scheduler start, then inject a task from outside.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let task = TaskBuilder::new(Local)
                .every("* * * * * * *")
                .add_step_default(move |_ctx| {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(Success)
                    }
                })
                .build();
            spawner.spawn(task).expect("spawn should be accepted");
        });

        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(1500)))
            .await;
        injector.await.unwrap();

        assert!(
            counter.load(Ordering::SeqCst) >= 1,
            "spawned task should have executed at least once"
        );
    }

    /// `spawn_get_id` resolves with the id the scheduler assigns the task. (0.5.0)
    #[tokio::test]
    async fn test_spawner_get_id_returns_assigned_id() {
        let mut scheduler = TaskScheduler::new(50, Local);
        // Pre-register one task so the next assigned id is 1.
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        let spawner = scheduler.spawner();

        let getter = tokio::spawn(async move {
            let task = TaskBuilder::new(Local).every("* * * * * * *").build();
            spawner.spawn_get_id(task).await
        });

        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(400)))
            .await;

        let id = getter.await.unwrap().expect("spawn_get_id should succeed");
        assert_eq!(id, 1, "the injected task should be assigned the next id");
    }

    /// A spawned task whose name collides with an existing one is rejected. (0.5.0)
    #[tokio::test]
    async fn test_spawner_rejects_duplicate_name() {
        let mut scheduler = TaskScheduler::new(50, Local);
        scheduler
            .add_task(
                TaskBuilder::new(Local)
                    .every("* * * * * * *")
                    .name("dup")
                    .build(),
            )
            .unwrap();
        let spawner = scheduler.spawner();

        let getter = tokio::spawn(async move {
            let task = TaskBuilder::new(Local)
                .every("* * * * * * *")
                .name("dup")
                .build();
            spawner.spawn_get_id(task).await
        });

        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(400)))
            .await;

        let result = getter.await.unwrap();
        assert!(
            matches!(result, Err(TaskError::DuplicateTaskName(name)) if name == "dup"),
            "a duplicate name must be rejected on the scheduler side"
        );
    }

    /// Collect every event already published on `rx`.
    fn drain_events(rx: &mut broadcast::Receiver<SchedulerEvent>) -> Vec<SchedulerEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// A step that sleeps for `ms` and counts its runs.
    fn sleeping_task(ms: u64, runs: &Arc<AtomicUsize>) -> TaskBuilder<Local> {
        let r = runs.clone();
        TaskBuilder::new(Local)
            .every("* * * * * * *")
            .add_step("sleep", move |_ctx| {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok(Success)
                }
            })
    }

    /// `TaskScheduler` is `Send` and can be driven from `tokio::spawn`. (F3)
    #[tokio::test]
    async fn test_scheduler_is_send_and_spawnable() {
        fn assert_send<X: Send>(_: &X) {}
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        // A generator closure no longer prevents `Send`.
        scheduler.set_task_gen(TaskGenerator::new("0 0 0 1 1 * 2099", Local, || None).unwrap());
        scheduler
            .add_task(counting_task(&counter, Some(1)))
            .unwrap();
        assert_send(&scheduler);

        let handle = scheduler.handle();
        let driver = tokio::spawn(async move {
            scheduler.run().await;
        });
        tokio::time::sleep(Duration::from_millis(1300)).await;
        handle.shutdown();
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("spawned scheduler did not stop")
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// Tasks can be added outside a Tokio runtime; they start when the scheduler runs. (F5)
    #[test]
    fn test_add_task_outside_runtime() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        let id = scheduler
            .add_task_get_id(counting_task(&counter, Some(1)))
            .unwrap();
        let handle = scheduler.handle();
        // Already initialized and observable before any runtime exists.
        let state = handle.state_of(id).unwrap();
        assert_eq!(state.status, Status::Scheduled);
        assert!(state.next_exec.is_some());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            scheduler
                .run_until(tokio::time::sleep(Duration::from_millis(1300)))
                .await;
        });
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// Removing a task that was never started reports it as removed. (F5, F2)
    #[test]
    fn test_remove_unstarted_task() {
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        let handle = scheduler.handle();
        let mut events = handle.subscribe();
        assert!(handle.remove(0));
        scheduler.dispatch_round();
        assert_eq!(handle.task_count(), 0);
        assert!(drain_events(&mut events).iter().any(|e| matches!(
            e,
            SchedulerEvent::TaskRemoved {
                task_id: 0,
                reason: FinishReason::Removed,
                ..
            }
        )));
    }

    /// Shutdown waits for a run in progress to finish, records it, and fires
    /// `on_finish` with `Shutdown`. (P1, F1)
    #[tokio::test]
    async fn test_shutdown_waits_for_run_in_progress() {
        let runs = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let f = finished.clone();
        let r = reasons.clone();
        let mut scheduler = TaskScheduler::new(100, Local);
        let task = sleeping_task(400, &runs)
            .add_step("after", move |_ctx| {
                let f = f.clone();
                async move {
                    f.fetch_add(1, Ordering::SeqCst);
                    Ok(Success)
                }
            })
            .on_finish(move |report| {
                let r = r.clone();
                async move { r.lock().unwrap().push(report.reason) }
            })
            .build();
        let id = scheduler.add_task_get_id(task).unwrap();
        let handle = scheduler.handle();
        let probe = handle.clone();
        let stopper = tokio::spawn(async move {
            // Wait until the first run is in progress, then stop.
            while !probe.state_of(id).is_some_and(|s| s.running) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let history = probe.history(id);
            probe.shutdown();
            history
        });
        tokio::time::timeout(Duration::from_secs(5), scheduler.run())
            .await
            .expect("scheduler did not stop");
        let history_mid_run = stopper.await.unwrap();

        assert_eq!(history_mid_run.len(), 1);
        assert_eq!(history_mid_run[0].outcome, None, "in progress when stopped");
        // The step that was running completed; the next step saw the stop request and
        // did not start.
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(finished.load(Ordering::SeqCst), 0);
        assert_eq!(*reasons.lock().unwrap(), vec![FinishReason::Shutdown]);
        assert_eq!(scheduler.handles.len(), 0);
    }

    /// A step that ignores the stop request is aborted once the grace period ends, and
    /// its run is recorded as cancelled. (P1, F1)
    #[tokio::test]
    async fn test_shutdown_aborts_after_grace_period() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler.grace_period(Duration::from_millis(100));
        let id = scheduler
            .add_task_get_id(sleeping_task(30_000, &runs).build())
            .unwrap();
        let handle = scheduler.handle();
        let mut events = handle.subscribe();
        let probe = handle.clone();
        let stopper = tokio::spawn(async move {
            while !probe.state_of(id).is_some_and(|s| s.running) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            probe.shutdown();
        });
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), scheduler.run())
            .await
            .expect("scheduler did not stop");
        stopper.await.unwrap();
        // The scheduler waited about one grace period after the request, not 30 s.
        assert!(started.elapsed() < Duration::from_secs(3));
        let events = drain_events(&mut events);
        assert!(events.iter().any(|e| matches!(
            e,
            SchedulerEvent::TaskRemoved {
                reason: FinishReason::Shutdown,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SchedulerEvent::RunFinished { .. })),
            "the aborted run never finished"
        );
    }

    /// A step that watches for cancellation stops at once on shutdown; its run is
    /// recorded as cancelled and the scheduler does not wait for the grace period. (P1)
    #[tokio::test]
    async fn test_shutdown_cooperative_step_stops_early() {
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler.grace_period(Duration::from_secs(30));
        let task = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .add_step("wait", |ctx| async move {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(30)) => Ok(Success),
                    _ = ctx.cancelled() => Err(TaskStepStatusErr::error("stopped")),
                }
            })
            .build();
        let id = scheduler.add_task_get_id(task).unwrap();
        let handle = scheduler.handle();
        let mut events = handle.subscribe();
        let probe = handle.clone();
        let o = outcomes.clone();
        let stopper = tokio::spawn(async move {
            while !probe.state_of(id).is_some_and(|s| s.running) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            probe.shutdown();
            o.lock().unwrap().push(());
        });
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), scheduler.run())
            .await
            .expect("scheduler did not stop");
        stopper.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
        let report = drain_events(&mut events)
            .into_iter()
            .find_map(|e| match e {
                SchedulerEvent::RunFinished { report } => Some(report),
                _ => None,
            })
            .expect("the run finished");
        assert_eq!(report.record.outcome, Some(RunOutcome::Cancelled));
        assert_eq!(report.steps[0].status, StepStatus::Cancelled);
    }

    /// Removing a task mid-run lets the run finish, then fires `on_finish` with
    /// `Removed`. (P1, F2)
    #[tokio::test]
    async fn test_remove_lets_run_finish() {
        let runs = Arc::new(AtomicUsize::new(0));
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let r = reasons.clone();
        let mut scheduler = TaskScheduler::new(100, Local);
        let task = sleeping_task(300, &runs)
            .on_finish(move |report| {
                let r = r.clone();
                async move { r.lock().unwrap().push((report.reason, report.last_run)) }
            })
            .build();
        let id = scheduler.add_task_get_id(task).unwrap();
        let handle = scheduler.handle();
        let mut events = handle.subscribe();
        let probe = handle.clone();
        let remover = tokio::spawn(async move {
            while !probe.state_of(id).is_some_and(|s| s.running) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(probe.remove(id));
            tokio::time::sleep(Duration::from_millis(50)).await;
            probe.task_count()
        });
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(2500)))
            .await;
        assert_eq!(remover.await.unwrap(), 0, "unregistered on the next round");

        let reasons = reasons.lock().unwrap();
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].0, FinishReason::Removed);
        assert_eq!(
            reasons[0].1.as_ref().and_then(|r| r.outcome.clone()),
            Some(RunOutcome::Success),
            "the run in progress completed"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no run after removal");
        let removed: Vec<_> = drain_events(&mut events)
            .into_iter()
            .filter(|e| matches!(e, SchedulerEvent::TaskRemoved { .. }))
            .collect();
        assert_eq!(removed.len(), 1);
    }

    /// A removed task that ignores the stop request is aborted after the grace period
    /// while the scheduler keeps running. (P1, F2)
    #[tokio::test]
    async fn test_remove_aborts_after_grace_period() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler.grace_period(Duration::from_millis(100));
        let id = scheduler
            .add_task_get_id(sleeping_task(30_000, &runs).build())
            .unwrap();
        let handle = scheduler.handle();
        let mut events = handle.subscribe();
        let probe = handle.clone();
        tokio::spawn(async move {
            while !probe.state_of(id).is_some_and(|s| s.running) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            probe.remove(id);
        });
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(1800)))
            .await;
        let events = drain_events(&mut events);
        let removed_at = events.iter().position(|e| {
            matches!(
                e,
                SchedulerEvent::TaskRemoved {
                    reason: FinishReason::Removed,
                    ..
                }
            )
        });
        assert!(
            removed_at.is_some(),
            "the task was aborted and reported removed"
        );
        assert!(scheduler.retiring.is_empty(), "aborted before shutdown");
    }

    /// The event stream reports a task's whole life in order. (P2)
    #[tokio::test]
    async fn test_event_stream_lifecycle() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        let mut events = scheduler.handle().subscribe();
        scheduler
            .add_task(counting_task(&counter, Some(1)))
            .unwrap();
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(1500)))
            .await;

        let kinds: Vec<&'static str> = drain_events(&mut events)
            .iter()
            .map(|e| match e {
                SchedulerEvent::SchedulerStarted => "scheduler-started",
                SchedulerEvent::SchedulerStopped => "scheduler-stopped",
                SchedulerEvent::TaskAdded { .. } => "added",
                SchedulerEvent::RunStarted { .. } => "run-started",
                SchedulerEvent::StepStarted { .. } => "step-started",
                SchedulerEvent::StepFinished { .. } => "step-finished",
                SchedulerEvent::RunFinished { .. } => "run-finished",
                SchedulerEvent::TaskRemoved { reason, .. } => {
                    assert_eq!(*reason, FinishReason::Completed);
                    "removed"
                }
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "added",
                "scheduler-started",
                "run-started",
                "step-started",
                "step-finished",
                "run-finished",
                "removed",
                "scheduler-stopped"
            ]
        );
    }

    /// Pause, resume and trigger publish events. (P2, F11)
    #[tokio::test]
    async fn test_control_events() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(1000, Local);
        scheduler.add_task(counting_task(&counter, None)).unwrap();
        let handle = scheduler.handle();
        let mut events = handle.subscribe();
        handle.pause(0);
        // Trigger runs a paused task.
        handle.trigger(0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.resume(0);
        assert!(!handle.pause(42));

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "trigger ran the paused task"
        );
        let events = drain_events(&mut events);
        assert!(matches!(
            events[0],
            SchedulerEvent::TaskPaused { task_id: 0, .. }
        ));
        assert!(matches!(
            events[1],
            SchedulerEvent::TaskTriggered { task_id: 0, .. }
        ));
        assert!(events
            .iter()
            .any(|e| matches!(e, SchedulerEvent::TaskResumed { task_id: 0, .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, SchedulerEvent::RunFinished { .. })));
    }

    /// A generated task that cannot be added is published as `TaskRejected` instead of
    /// being dropped silently. (F4)
    #[tokio::test]
    async fn test_generator_rejection_is_reported() {
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler
            .add_task(
                TaskBuilder::new(Local)
                    .every("* * * * * * *")
                    .name("taken")
                    .build(),
            )
            .unwrap();
        scheduler.set_task_gen(
            TaskGenerator::new("* * * * * * *", Local, || {
                Some(
                    TaskBuilder::new(Local)
                        .every("* * * * * * *")
                        .name("taken")
                        .build(),
                )
            })
            .unwrap(),
        );
        let mut events = scheduler.handle().subscribe();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!scheduler.run_task_gen());
        let rejected = drain_events(&mut events).into_iter().find_map(|e| match e {
            SchedulerEvent::TaskRejected { task_name, reason } => Some((task_name, reason)),
            _ => None,
        });
        let (name, reason) = rejected.expect("rejection was published");
        assert_eq!(name.as_deref(), Some("taken"));
        assert!(reason.contains("taken"));
        assert_eq!(scheduler.handles.len(), 1);
    }

    /// A fire-and-forget spawn that is rejected is published as `TaskRejected`. (F4)
    #[tokio::test]
    async fn test_spawn_rejection_is_reported() {
        let mut scheduler = TaskScheduler::new(100, Local);
        let spawner = scheduler.spawner();
        let mut events = scheduler.handle().subscribe();
        spawner
            .spawn(
                TaskBuilder::new(Local)
                    .every("* * * * * * *")
                    .pool("nope")
                    .build(),
            )
            .unwrap();
        scheduler.drain_spawns();
        assert!(drain_events(&mut events).iter().any(|e| matches!(
            e,
            SchedulerEvent::TaskRejected { reason, .. } if reason.contains("nope")
        )));
    }

    /// Runs start on their scheduled second even when the maximum idle wait is far
    /// longer than the schedule's interval. (P3)
    #[tokio::test]
    async fn test_timer_driven_dispatch_is_on_time() {
        let counter = Arc::new(AtomicUsize::new(0));
        // A one-minute idle wait: with polling, nothing would run in this test.
        let mut scheduler = TaskScheduler::new(60_000, Local);
        let id = scheduler
            .add_task_get_id(counting_task(&counter, None))
            .unwrap();
        let handle = scheduler.handle();
        let probe = handle.clone();
        let observer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2600)).await;
            probe.history(id)
        });
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(2800)))
            .await;
        let history = observer.await.unwrap();

        assert!(
            history.len() >= 2,
            "expected 2+ runs, got {}",
            history.len()
        );
        // With the one-minute idle wait, only a timer aimed at the slot can start these
        // runs inside the window. The bound leaves room for a slow CI runner.
        for record in &history {
            let late_ms = record.started_at.timestamp_subsec_millis();
            assert!(late_ms < 500, "run started {late_ms} ms after its slot");
        }
    }

    /// Under `MissedRunPolicy::Skip`, resuming after a long pause skips the missed slot
    /// instead of running it late, while the default `RunOnce` runs it once. (P3, F10)
    #[tokio::test]
    async fn test_resume_after_pause_follows_missed_run_policy() {
        async fn runs_after_resume(policy: MissedRunPolicy) -> (usize, Vec<SchedulerEvent>) {
            let counter = Arc::new(AtomicUsize::new(0));
            let c = counter.clone();
            let mut scheduler = TaskScheduler::new(1000, Local);
            let task = TaskBuilder::new(Local)
                .every("* * * * * * *")
                .missed_runs(policy)
                .add_step_default(move |_ctx| {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(Success)
                    }
                })
                .build();
            scheduler.add_task(task).unwrap();
            let handle = scheduler.handle();
            let mut events = handle.subscribe();
            handle.pause(0);
            // Several per-second slots pass while paused.
            tokio::time::sleep(Duration::from_millis(2500)).await;
            scheduler.dispatch_round();
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "paused task must not run"
            );
            handle.resume(0);
            scheduler.dispatch_round();
            tokio::time::sleep(Duration::from_millis(150)).await;
            (counter.load(Ordering::SeqCst), drain_events(&mut events))
        }

        let (runs, events) = runs_after_resume(MissedRunPolicy::Skip {
            grace: Duration::from_millis(500),
        })
        .await;
        assert_eq!(runs, 0, "Skip drops the late slot");
        assert!(events
            .iter()
            .any(|e| matches!(e, SchedulerEvent::RunSkipped { task_id: 0, .. })));

        let (runs, events) = runs_after_resume(MissedRunPolicy::RunOnce).await;
        assert_eq!(runs, 1, "RunOnce runs the late slot once");
        assert!(!events
            .iter()
            .any(|e| matches!(e, SchedulerEvent::RunSkipped { .. })));
    }

    /// `OverlapPolicy::Queue` does not queue an extra run when no slot came due during
    /// the run. Before 0.6.0 any run longer than one poll period got a spurious
    /// second run. (F13)
    #[tokio::test]
    async fn test_queue_without_missed_slot_runs_once() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let mut scheduler = TaskScheduler::new(100, Local);
        let task = TaskBuilder::new(Local)
            // Hourly: no slot can pass during the 1.5 s run below.
            .every("0 0 * * * * *")
            .overlap(OverlapPolicy::Queue)
            .add_step_default(move |_ctx| {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    Ok(Success)
                }
            })
            .build();
        scheduler.add_task(task).unwrap();
        // Make the task due now.
        scheduler.handles[0].shared.state.lock().unwrap().next_exec =
            Some(Utc::now() - chrono::Duration::milliseconds(10));
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(2500)))
            .await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    /// `OverlapPolicy::Queue` runs a slot that passed during a run right after it, while
    /// `Skip` waits for the next future slot. (C4)
    #[tokio::test]
    async fn test_queue_runs_slot_missed_during_run() {
        async fn gap_between_runs(policy: OverlapPolicy) -> chrono::Duration {
            let runs = Arc::new(AtomicUsize::new(0));
            let mut scheduler = TaskScheduler::new(100, Local);
            let id = scheduler
                .add_task_get_id(sleeping_task(1200, &runs).overlap(policy).build())
                .unwrap();
            let probe = scheduler.handle();
            let observer = tokio::spawn(async move {
                loop {
                    let history = probe.history(id);
                    if history.len() >= 2 {
                        return history;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
            scheduler
                .run_until(async {
                    let _ = tokio::time::timeout(Duration::from_secs(6), async {
                        while !observer.is_finished() {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    })
                    .await;
                })
                .await;
            let history = observer.await.unwrap();
            history[1].started_at - history[0].finished_at.unwrap()
        }

        let queued = gap_between_runs(OverlapPolicy::Queue).await;
        assert!(
            queued < chrono::Duration::milliseconds(300),
            "Queue should rerun at once, gap was {queued}"
        );
        let skipped = gap_between_runs(OverlapPolicy::Skip).await;
        assert!(
            skipped > chrono::Duration::milliseconds(400),
            "Skip should wait for the next slot, gap was {skipped}"
        );
    }

    /// `max_concurrent_runs` caps the number of simultaneous runs; runs over the limit
    /// are reported as waiting. (P5)
    #[tokio::test]
    async fn test_max_concurrent_runs() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler.max_concurrent_runs(1);
        for _ in 0..3 {
            let a = active.clone();
            let p = peak.clone();
            scheduler
                .add_task(
                    TaskBuilder::new(Local)
                        .every("* * * * * * *")
                        .add_step_default(move |_ctx| {
                            let a = a.clone();
                            let p = p.clone();
                            async move {
                                let now = a.fetch_add(1, Ordering::SeqCst) + 1;
                                p.fetch_max(now, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                a.fetch_sub(1, Ordering::SeqCst);
                                Ok(Success)
                            }
                        })
                        .build(),
                )
                .unwrap();
        }
        let probe = scheduler.handle();
        let saw_waiting = Arc::new(AtomicBool::new(false));
        let w = saw_waiting.clone();
        let observer = tokio::spawn(async move {
            for _ in 0..150 {
                if probe.statuses().iter().any(|s| s.waiting) {
                    w.store(true, Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(1800)))
            .await;
        observer.await.unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        assert!(saw_waiting.load(Ordering::SeqCst));
    }

    /// A pool limits only its own tasks, and an unknown pool is rejected. (P5)
    #[tokio::test]
    async fn test_pool_limits() {
        let pool_active = Arc::new(AtomicUsize::new(0));
        let pool_peak = Arc::new(AtomicUsize::new(0));
        let free_runs = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        assert!(matches!(
            scheduler.add_task(TaskBuilder::new(Local).pool("db").build()),
            Err(TaskError::UnknownPool(name)) if name == "db"
        ));
        scheduler.add_pool("db", 1);
        for _ in 0..2 {
            let a = pool_active.clone();
            let p = pool_peak.clone();
            scheduler
                .add_task(
                    TaskBuilder::new(Local)
                        .every("* * * * * * *")
                        .pool("db")
                        .add_step_default(move |_ctx| {
                            let a = a.clone();
                            let p = p.clone();
                            async move {
                                let now = a.fetch_add(1, Ordering::SeqCst) + 1;
                                p.fetch_max(now, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                a.fetch_sub(1, Ordering::SeqCst);
                                Ok(Success)
                            }
                        })
                        .build(),
                )
                .unwrap();
        }
        scheduler.add_task(counting_task(&free_runs, None)).unwrap();
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(1500)))
            .await;
        assert_eq!(pool_peak.load(Ordering::SeqCst), 1);
        assert!(free_runs.load(Ordering::SeqCst) >= 1);
    }

    /// Stopping a task while it waits for a permit abandons the run before it starts. (P1, P5)
    #[tokio::test]
    async fn test_stop_while_waiting_for_permit() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler.max_concurrent_runs(1);
        scheduler.grace_period(Duration::from_millis(200));
        let blocker = scheduler
            .add_task_get_id(sleeping_task(30_000, &runs).build())
            .unwrap();
        let waiter_runs = Arc::new(AtomicUsize::new(0));
        let w = waiter_runs.clone();
        // Never due on its own: it only runs when triggered below, after the blocker
        // already holds the single permit. Letting both come due on the same slot would
        // make the outcome depend on which driver reaches the semaphore first.
        let waiter = scheduler
            .add_task_get_id(
                TaskBuilder::new(Local)
                    .every("0 0 0 1 1 * 2099")
                    .add_step_default(move |_ctx| {
                        let w = w.clone();
                        async move {
                            w.fetch_add(1, Ordering::SeqCst);
                            Ok(Success)
                        }
                    })
                    .build(),
            )
            .unwrap();
        let probe = scheduler.handle();
        let saw_waiting = Arc::new(AtomicBool::new(false));
        let seen = saw_waiting.clone();
        tokio::spawn(async move {
            while !probe.state_of(blocker).is_some_and(|s| s.running) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            probe.trigger(waiter);
            while !probe.state_of(waiter).is_some_and(|s| s.waiting) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            seen.store(true, Ordering::SeqCst);
            probe.remove(waiter);
        });
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(1800)))
            .await;
        assert!(
            saw_waiting.load(Ordering::SeqCst),
            "the waiter never waited"
        );
        assert_eq!(waiter_runs.load(Ordering::SeqCst), 0);
    }

    /// Events and reports round-trip through serde. (P2, `serde` feature)
    #[cfg(feature = "serde")]
    #[test]
    fn test_serde_roundtrip_events() {
        use crate::task::{FinishReport, RunReport};
        let record = RunRecord {
            run_id: 1,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            outcome: Some(RunOutcome::Cancelled),
            attempts: 1,
            error: Some("stopped".to_string()),
            failed_step: None,
        };
        let events = vec![
            SchedulerEvent::RunFinished {
                report: RunReport {
                    task_id: 3,
                    task_name: Some("t".to_string()),
                    record: record.clone(),
                    steps: Vec::new(),
                },
            },
            SchedulerEvent::TaskRemoved {
                task_id: 3,
                task_name: None,
                reason: FinishReason::Shutdown,
            },
            SchedulerEvent::TaskRejected {
                task_name: None,
                reason: "duplicate".to_string(),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(
                serde_json::from_str::<SchedulerEvent>(&json).unwrap(),
                event
            );
        }
        let finish = FinishReport {
            task_id: 3,
            task_name: None,
            reason: FinishReason::Removed,
            last_run: Some(record),
        };
        let json = serde_json::to_string(&finish).unwrap();
        assert_eq!(serde_json::from_str::<FinishReport>(&json).unwrap(), finish);
    }

    /// A generator whose schedule has no further execution is dropped instead of being
    /// treated as due on every loop iteration. (review #1)
    #[tokio::test]
    async fn test_exhausted_generator_is_dropped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        // A single slot, two seconds from now (truncated to the second, so 1-2 s away).
        // One second would leave a slow machine no time to build the generator before
        // the slot has passed.
        let slot = Local::now() + chrono::Duration::seconds(2);
        let expression = slot.format("%S %M %H %d %m * %Y").to_string();
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler.set_task_gen(
            TaskGenerator::new(&expression, Local, move || {
                c.fetch_add(1, Ordering::SeqCst);
                None
            })
            .unwrap(),
        );
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(3500)))
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the single slot ran once");
        assert!(
            scheduler.task_gen.is_none(),
            "the exhausted generator was dropped"
        );
    }

    /// A trigger that lands while a run is finishing is never lost. Stress test on a
    /// multi-thread runtime. (review #2, #9)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_trigger_during_run_completion_is_not_lost() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let mut scheduler = TaskScheduler::new(1000, Local);
        let task = TaskBuilder::new(Local)
            // Far in the future: every run below comes from a trigger.
            .every("0 0 0 1 1 * 2099")
            .add_step_default(move |_ctx| {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok(Success)
                }
            })
            .build();
        let id = scheduler.add_task_get_id(task).unwrap();
        let handle = scheduler.handle();
        let driver = tokio::spawn(async move {
            scheduler.run().await;
        });
        for iteration in 0..300 {
            let before = runs.load(Ordering::SeqCst);
            assert!(handle.trigger(id));
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while runs.load(Ordering::SeqCst) <= before {
                assert!(
                    std::time::Instant::now() < deadline,
                    "trigger {iteration} was lost"
                );
                tokio::task::yield_now().await;
            }
        }
        handle.shutdown();
        driver.await.unwrap();
    }

    /// A driver that stops without finishing is detected, its run closed as cancelled
    /// and its removal reported. (review #3)
    #[tokio::test]
    async fn test_crashed_driver_is_reaped() {
        let mut scheduler = TaskScheduler::new(1000, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        let handle = scheduler.handle();
        let mut events = handle.subscribe();
        // Replace the real driver with one that has already stopped, as a panic would.
        let shared = scheduler.handles[0].shared.clone();
        shared.start_run_for_test();
        scheduler.handles[0].join.take().unwrap().abort();
        let stopped = tokio::spawn(async {});
        tokio::time::sleep(Duration::from_millis(20)).await;
        scheduler.handles[0].join = Some(stopped);
        scheduler.dispatch_round();

        assert_eq!(handle.task_count(), 0);
        assert_eq!(
            shared.history.lock().unwrap()[0].outcome,
            Some(RunOutcome::Cancelled)
        );
        assert!(drain_events(&mut events).iter().any(|e| matches!(
            e,
            SchedulerEvent::TaskRemoved {
                reason: FinishReason::Panicked,
                ..
            }
        )));
    }

    /// A panicking step fails its run with the panic message and the task keeps
    /// running; a panicking callback is ignored. (review #3)
    #[tokio::test]
    async fn test_panicking_step_and_callback_do_not_kill_the_task() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut scheduler = TaskScheduler::new(100, Local);
        let task = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .add_step("explode", move |_ctx| {
                let c = c.clone();
                async move {
                    if c.fetch_add(1, Ordering::SeqCst) == 0 {
                        panic!("boom");
                    }
                    Ok(Success)
                }
            })
            .on_failure(|_| async { panic!("callback boom") })
            .build();
        let id = scheduler.add_task_get_id(task).unwrap();
        let probe = scheduler.handle();
        let observer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2600)).await;
            probe.history(id)
        });
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(2800)))
            .await;
        let history = observer.await.unwrap();
        assert!(history.len() >= 2, "the task kept running after the panic");
        assert_eq!(history[0].outcome, Some(RunOutcome::Failed));
        assert_eq!(history[0].error.as_deref(), Some("step panicked: boom"));
        assert_eq!(history[1].outcome, Some(RunOutcome::Success));
    }

    /// A spawner used after the scheduler stopped gets an error instead of hanging.
    /// (review #4)
    #[tokio::test]
    async fn test_spawn_after_shutdown_fails() {
        let mut scheduler = TaskScheduler::new(100, Local);
        let spawner = scheduler.spawner();
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(50)))
            .await;
        assert!(matches!(
            spawner.spawn(TaskBuilder::new(Local).build()),
            Err(TaskError::ExecutionError(_))
        ));
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            spawner.spawn_get_id(TaskBuilder::new(Local).build()),
        )
        .await
        .expect("spawn_get_id must not hang after shutdown");
        assert!(matches!(result, Err(TaskError::ExecutionError(_))));
    }

    /// Removing a task during its last repeat reports `Removed`, not `Completed`.
    /// (review #5)
    #[tokio::test]
    async fn test_remove_during_last_repeat_reports_removed() {
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let r = reasons.clone();
        let mut scheduler = TaskScheduler::new(100, Local);
        let task = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .repeat(1)
            .add_step("wait", |ctx| async move {
                ctx.cancelled().await;
                Err(TaskStepStatusErr::error("stopped"))
            })
            .on_finish(move |report| {
                let r = r.clone();
                async move { r.lock().unwrap().push(report.reason) }
            })
            .build();
        let id = scheduler.add_task_get_id(task).unwrap();
        let handle = scheduler.handle();
        let mut events = handle.subscribe();
        let probe = handle.clone();
        tokio::spawn(async move {
            while !probe.state_of(id).is_some_and(|s| s.running) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            probe.remove(id);
        });
        scheduler
            .run_until(tokio::time::sleep(Duration::from_millis(1800)))
            .await;
        assert_eq!(*reasons.lock().unwrap(), vec![FinishReason::Removed]);
        assert!(drain_events(&mut events).iter().any(|e| matches!(
            e,
            SchedulerEvent::TaskRemoved {
                reason: FinishReason::Removed,
                ..
            }
        )));
    }

    /// A task added outside a runtime computes its first execution time again when the
    /// scheduler starts, so a slot that passed in between is not run late. (review #12)
    #[test]
    fn test_unstarted_task_reinitializes_on_start() {
        let mut scheduler = TaskScheduler::new(100, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        let handle = scheduler.handle();
        let stale = handle.state_of(0).unwrap().next_exec.unwrap();
        std::thread::sleep(Duration::from_millis(2200));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let probe = handle.clone();
        let fresh = runtime.block_on(async move {
            // The scheduler stops only after the observer has read the state, so a slow
            // machine cannot make the read happen after shutdown cleared the registry.
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let observer = tokio::spawn(async move {
                // The first scheduler round restarts the task; wait until it has
                // published a fresh time.
                loop {
                    let next = probe.state_of(0).unwrap().next_exec.unwrap();
                    if next != stale {
                        let _ = done_tx.send(());
                        return next;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
            scheduler
                .run_until(async {
                    let _ = done_rx.await;
                })
                .await;
            observer.await.unwrap()
        });
        assert!(fresh > stale + chrono::Duration::seconds(1));
    }
}

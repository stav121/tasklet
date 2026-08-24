use crate::errors::{TaskError, TaskResult};
use crate::generator::TaskGenerator;
use crate::task::{
    run_task, OverlapPolicy, RunOutcome, RunRecord, Status, StepState, Task, TaskCmd, TaskShared,
};
use crate::{scheduler_log, task_log};
use chrono::prelude::*;
use chrono::Utc;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;

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
/// Spawned tasks are picked up on the next scheduler round, so they are only processed
/// while the scheduler is running (like tasks produced by a
/// [`TaskGenerator`](crate::TaskGenerator)).
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
}

// A manual `Clone` avoids an unnecessary `T: Clone` bound the derive would add: the
// only field is an `UnboundedSender`, which is always cloneable.
impl<T> Clone for TaskSpawner<T>
where
    T: TimeZone + Clone + Send + 'static,
{
    fn clone(&self) -> Self {
        TaskSpawner {
            tx: self.tx.clone(),
        }
    }
}

impl<T> TaskSpawner<T>
where
    T: TimeZone + Clone + Send + 'static,
    <T as TimeZone>::Offset: Send,
{
    /// Queue a task to be added on the scheduler's next round.
    ///
    /// A build error in `task` is returned immediately. The task is otherwise queued
    /// fire-and-forget; if it carries a name that collides with an existing task it is
    /// rejected on the scheduler side (observable via the handle rather than here). Use
    /// [`spawn_get_id`](Self::spawn_get_id) to await the assigned id or the rejection.
    ///
    /// Returns [`TaskError::ExecutionError`] if the scheduler has already stopped.
    pub fn spawn(&self, task: TaskResult<Task<T>>) -> TaskResult<()> {
        let task = task?;
        self.tx
            .send(SpawnRequest { task, reply: None })
            .map_err(|_| TaskError::ExecutionError("scheduler is no longer running".to_string()))
    }

    /// Queue a task and await the id the scheduler assigns it.
    ///
    /// Resolves once the scheduler processes the request on its next round. A build
    /// error is returned immediately; a duplicate name is reported as
    /// [`TaskError::DuplicateTaskName`]; a stopped scheduler as
    /// [`TaskError::ExecutionError`].
    pub async fn spawn_get_id(&self, task: TaskResult<Task<T>>) -> TaskResult<usize> {
        let task = task?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(SpawnRequest {
                task,
                reply: Some(reply_tx),
            })
            .map_err(|_| TaskError::ExecutionError("scheduler is no longer running".to_string()))?;
        reply_rx.await.map_err(|_| {
            TaskError::ExecutionError("scheduler dropped the spawn request".to_string())
        })?
    }
}

/// Handler for a running task.
///
/// Holds the task's join handle, the sender used to dispatch runs, and the shared
/// state the task publishes so the scheduler can observe it without a blocking
/// request/response round-trip.
#[derive(Debug)]
pub struct TaskHandle {
    id: usize,
    name: Option<String>,
    handle: JoinHandle<()>,
    sender: mpsc::Sender<TaskCmd>,
    shared: Arc<TaskShared>,
    overlap: OverlapPolicy,
}

/// A cheaply-clonable entry shared with [`SchedulerHandle`] so it can observe and
/// control a task without touching the scheduler-private join handle. Rebuilt from the
/// live task set at the end of every scheduler round.
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
        let last_outcome = self
            .shared
            .history
            .lock()
            .unwrap()
            .back()
            .and_then(|r| r.outcome.clone());
        TaskState {
            id: self.id,
            name: self.name.clone(),
            status,
            next_exec,
            running: self.shared.running.load(Ordering::SeqCst),
            paused: self.shared.paused.load(Ordering::SeqCst),
            last_outcome,
            run_count: self.shared.runs.load(Ordering::SeqCst),
        }
    }
}

/// A snapshot of a single task's state, as reported by [`SchedulerHandle`].
///
/// This is a read-only view computed from the task's live shared state, refreshed at
/// the end of every scheduler round.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct TaskState {
    /// The task's id, as assigned by the scheduler.
    pub id: usize,
    /// The task's unique name, if one was set via [`TaskBuilder::name`](crate::TaskBuilder::name).
    pub name: Option<String>,
    /// The task's lifecycle status as of the last completed round.
    pub status: Status,
    /// The task's next execution time, normalized to UTC (`None` if not scheduled).
    pub next_exec: Option<DateTime<Utc>>,
    /// Whether a run of this task is currently in progress.
    pub running: bool,
    /// Whether the task is currently paused (its schedule is not dispatched).
    pub paused: bool,
    /// The outcome of the most recent completed run, if any.
    pub last_outcome: Option<RunOutcome>,
    /// The total number of runs started over the task's lifetime.
    pub run_count: usize,
}

/// A cloneable handle used to control and observe a running [`TaskScheduler`].
///
/// Obtain one with [`TaskScheduler::handle`] *before* calling
/// [`TaskScheduler::run`], then call [`SchedulerHandle::shutdown`] from anywhere
/// (another task, a signal handler, etc.) to request a graceful stop, or query the
/// live task set with [`SchedulerHandle::task_count`] / [`SchedulerHandle::statuses`].
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
}

impl SchedulerHandle {
    /// Request a graceful shutdown of the associated scheduler.
    ///
    /// The scheduler finishes the current execution round, drains its tasks and
    /// returns from [`TaskScheduler::run`]. Calling this before the scheduler
    /// starts is fine — the request is remembered and the loop exits on its first
    /// idle window.
    pub fn shutdown(&self) {
        // `notify_one` stores a permit if there is no current waiter, so a shutdown
        // requested before `run()` reaches its await point is not lost.
        self.notify.notify_one();
    }

    /// The number of tasks currently registered with the scheduler.
    ///
    /// The value reflects the most recently completed round.
    pub fn task_count(&self) -> usize {
        self.registry.lock().unwrap().len()
    }

    /// A snapshot of every live task's [`TaskState`], as of the last completed round.
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

    /// The recent [`RunRecord`] history of the task with the given id, oldest first.
    ///
    /// Returns an empty vector if the task is unknown or has no retained history.
    pub fn history(&self, id: usize) -> Vec<RunRecord> {
        self.registry
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.shared.history.lock().unwrap().iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The per-step state of the last completed run of the task with the given id.
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
    /// [`resume`](Self::resume) is called. A run already in progress is not interrupted.
    ///
    /// Returns `true` if the task was found.
    pub fn pause(&self, id: usize) -> bool {
        self.with_entry(id, |e| e.shared.paused.store(true, Ordering::SeqCst))
    }

    /// Resume a paused task so its schedule is dispatched again. Returns `true` if the
    /// task was found.
    pub fn resume(&self, id: usize) -> bool {
        self.with_entry(id, |e| e.shared.paused.store(false, Ordering::SeqCst))
    }

    /// Trigger an immediate run of the task with the given id, regardless of its
    /// schedule. If a run is already in progress, the trigger is queued to run once the
    /// current run finishes (at most one queued). Returns `true` if the task was found.
    pub fn trigger(&self, id: usize) -> bool {
        self.with_entry(id, |e| {
            if e.shared.running.load(Ordering::SeqCst) {
                e.shared.pending.store(true, Ordering::SeqCst);
            } else {
                // Fire-and-forget; a full channel means a run is already queued.
                let _ = e.sender.try_send(TaskCmd::Run);
            }
        })
    }

    /// Request removal of the task with the given id. The scheduler reaps it on its
    /// next round. Returns `true` if the task was found.
    pub fn remove(&self, id: usize) -> bool {
        self.with_entry(id, |e| {
            e.shared.remove_requested.store(true, Ordering::SeqCst)
        })
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

    /// Request removal of the task registered under the given name. Returns `true` if found.
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
pub struct TaskScheduler<T>
where
    T: TimeZone + Clone + Send + 'static,
{
    /// The task handles from the registered tasks.
    handles: Vec<TaskHandle>,
    /// The (optional) task generation function.
    task_gen: Option<TaskGenerator<T>>,
    /// The sleep time in ms.
    sleep: usize,
    /// The id that should be assigned to the next appended task.
    next_id: usize,
    /// The main timezone used for the scheduler.
    timezone: T,
    /// Notified when a graceful shutdown is requested.
    shutdown: Arc<Notify>,
    /// The observable/controllable registry, rebuilt each round and shared with every
    /// [`SchedulerHandle`].
    registry: Arc<Mutex<Vec<RegEntry>>>,
    /// Sender cloned into every [`TaskSpawner`] handed out via [`spawner`](Self::spawner).
    spawn_tx: mpsc::UnboundedSender<SpawnRequest<T>>,
    /// Receiver drained each round for tasks queued by a [`TaskSpawner`].
    spawn_rx: mpsc::UnboundedReceiver<SpawnRequest<T>>,
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
            /* Originally empty, no registered tasks. */
            task_gen: None,
            sleep: 1000,
            timezone,
            next_id: 0,
            shutdown: Arc::new(Notify::new()),
            registry: Arc::new(Mutex::new(Vec::new())),
            spawn_tx,
            spawn_rx,
        }
    }

    /// Return a cloneable [`SchedulerHandle`] that can request a graceful shutdown.
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
        }
    }

    /// Return a cloneable [`TaskSpawner`] that can add tasks to this scheduler while it
    /// is running.
    ///
    /// Unlike [`SchedulerHandle`], a spawner keeps the scheduler's timezone type `T`, so
    /// it can hand fully-built `Task<T>` values to the running loop. Obtain it before
    /// calling [`run`](Self::run); queued tasks are added on the next round.
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
        }
    }

    /// Create a new instance of `TaskScheduler` with no tasks to execute.
    ///
    /// # Arguments
    ///
    /// * sleep     - The execution frequency (in ms).
    /// * timezone  - The scheduler's timezone.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::TaskScheduler;
    /// // Create a new `TaskScheduler` instance that executes every 1000ms.
    /// let _ = TaskScheduler::new(1000, chrono::Local);
    /// ```
    pub fn new(sleep: usize, timezone: T) -> TaskScheduler<T> {
        TaskScheduler {
            sleep,
            ..TaskScheduler::default(timezone)
        }
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
    /// [`TaskError::DuplicateTaskName`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::{TaskScheduler, Task};
    /// # tokio_test::block_on( async {
    /// // Create a new `TaskScheduler` and attach a task to it.
    /// let mut scheduler = TaskScheduler::default(chrono::Local);
    /// // Add a task that executes every second forever.
    /// scheduler.add_task(Task::new("* * * * * * *", None, None, chrono::Local)).unwrap();
    /// # });
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
    /// If the task has a name (set via [`TaskBuilder::name`](crate::TaskBuilder::name)),
    /// it must be unique within the scheduler; a duplicate is rejected with
    /// [`TaskError::DuplicateTaskName`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use tasklet::{TaskScheduler, Task};
    /// # tokio_test::block_on( async {
    /// let mut scheduler = TaskScheduler::default(chrono::Local);
    /// let id = scheduler
    ///     .add_task_get_id(Task::new("* * * * * * *", None, None, chrono::Local))
    ///     .unwrap();
    /// assert_eq!(id, 0);
    /// # });
    /// ```
    pub fn add_task_get_id(&mut self, task: TaskResult<Task<T>>) -> Result<usize, TaskError> {
        let mut task = task?;

        // Enforce name uniqueness so a name is a stable, addressable identity.
        if let Some(name) = task.name.as_deref() {
            if self.handles.iter().any(|h| h.name.as_deref() == Some(name)) {
                return Err(TaskError::DuplicateTaskName(name.to_string()));
            }
        }

        let id = self.next_id;

        // A buffer of one is enough: the scheduler only dispatches a run when
        // the task is idle, so at most one `Run` is ever in flight.
        let (sender, receiver) = mpsc::channel(1);

        task.set_receiver(receiver);
        task.set_id(id);
        let overlap = task.overlap;
        let name = task.name.clone();
        let shared = Arc::new(TaskShared::new(task.history_limit));
        let handle = tokio::spawn(run_task(task, shared.clone()));

        // Push the handle
        self.handles.push(TaskHandle {
            id,
            name,
            handle,
            sender,
            shared,
            overlap,
        });

        // Increase the id of the next task.
        self.next_id += 1;

        // Publish the new task into the shared registry immediately so a
        // `SchedulerHandle` can observe and control it before `run()` is called, not
        // only after the first scheduling round.
        self.refresh_registry();
        Ok(id)
    }

    /// Add every task queued by a [`TaskSpawner`] since the last round, replying with the
    /// assigned id (or the rejection error) to any caller awaiting one.
    fn drain_spawns(&mut self) {
        while let Ok(request) = self.spawn_rx.try_recv() {
            let result = self.add_task_get_id(Ok(request.task));
            if let Some(reply) = request.reply {
                // The receiver may have gone away; that is fine.
                let _ = reply.send(result);
            }
        }
    }

    /// Run a single scheduling round.
    ///
    /// Reaps any task that has reached a terminal state, then dispatches a run to
    /// every task whose next execution time is due. Dispatch is fire-and-forget: the
    /// scheduler never waits for a task's run to complete, so a slow task cannot
    /// delay any other task's schedule. The overlap policy decides what to do when a
    /// task is still running at its next due time.
    fn dispatch_round(&mut self) {
        let now = Utc::now();

        // Reap tasks that have reached a terminal state or been removed on request.
        self.handles.retain(|handle| {
            let finished = handle.shared.finished.load(Ordering::SeqCst);
            let removed = handle.shared.remove_requested.load(Ordering::SeqCst);
            if finished || removed {
                let reason = if removed {
                    "Removing task on request"
                } else {
                    "Removing finished task"
                };
                task_log!(handle.id, log::Level::Debug, "{}", reason);
                handle.handle.abort();
                false
            } else {
                true
            }
        });

        // Dispatch due tasks.
        for handle in &self.handles {
            // A paused task's schedule is not dispatched.
            if handle.shared.paused.load(Ordering::SeqCst) {
                continue;
            }

            let due = match handle.shared.state.lock().unwrap().next_exec {
                Some(next) => now >= next,
                None => false,
            };
            if !due {
                continue;
            }

            if handle.shared.running.load(Ordering::SeqCst) {
                // A previous run is still in progress: apply the overlap policy.
                match handle.overlap {
                    OverlapPolicy::Skip => { /* drop this occurrence */ }
                    OverlapPolicy::Queue => {
                        handle.shared.pending.store(true, Ordering::SeqCst);
                    }
                }
            } else {
                // Fire-and-forget. `try_send` never blocks; a full channel means a run
                // is already queued for this idle task, so dropping the duplicate is
                // the correct behaviour.
                let _ = handle.sender.try_send(TaskCmd::Run);
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

    /// Execute the `TaskGenerator` instance (if set).
    ///
    /// This function will spawn the task, create its handle and attach it to the scheduler.
    fn run_task_gen(&mut self) -> bool {
        match self.task_gen {
            Some(ref mut tg) => {
                // Execute only if it's time to execute it.
                if tg.next_exec <= Utc::now().with_timezone(&self.timezone) {
                    return match tg.run() {
                        Some(t) => {
                            let _ = self.add_task(t);
                            true
                        }
                        None => false,
                    };
                }
                false
            }
            None => false,
        }
    }

    /// Abort every registered task and clear the handle list.
    ///
    /// Used on shutdown to drain the scheduler cleanly. Each task is just parked on
    /// its receiver, so aborting is safe.
    fn shutdown_tasks(&mut self) {
        for handle in &self.handles {
            handle.handle.abort();
        }
        self.handles.clear();
        self.registry.lock().unwrap().clear();
    }

    /// Run one iteration of the scheduler flow: run the generator (if due) and
    /// dispatch the current round. Newly generated tasks initialize themselves.
    fn tick(&mut self) {
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
    /// At first all the tasks are initialized and then the execution loop is entered.
    /// If a task generation/discovery method is provided, it is executed on every loop.
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
    /// future resolves the current round finishes, the tasks are drained and this
    /// returns.
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

        tokio::pin!(shutdown);

        // Tasks initialize themselves when spawned, so there is no separate init phase.

        // Drive the loop with an `Interval` rather than sleeping for a fixed amount
        // *after* each round. `sleep` would add the duration of `tick()` to every
        // cycle, so the poll cadence would slowly drift and long rounds could push
        // ticks past their intended time. An interval anchors ticks to a fixed
        // wall-clock cadence; `Skip` collapses missed ticks (when a round overruns
        // the period) instead of firing a burst of catch-up ticks. (B7)
        let mut interval = tokio::time::interval(Duration::from_millis(self.sleep as u64));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Wake up on either a due tick or a shutdown request, whichever comes
            // first. The first `interval.tick()` resolves immediately, so the first
            // round runs without delay.
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    scheduler_log!(log::Level::Info, "Shutdown requested, stopping scheduler");
                    break;
                }
                _ = interval.tick() => {
                    self.tick();
                }
            }
        }

        self.shutdown_tasks();
        scheduler_log!(log::Level::Info, "Scheduler stopped");
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::task::StepStatus;
    use crate::task::TaskStepStatusErr::ErrorDelete;
    use crate::task::TaskStepStatusOk::Success;
    use crate::TaskBuilder;
    use chrono::Local;
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

    /// `dispatch_round` does not start a new run when one is in progress under Skip. (X1)
    #[tokio::test]
    async fn test_dispatch_skips_running_task() {
        let mut scheduler = TaskScheduler::new(1000, Local);
        scheduler
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        // Allow the task to self-initialize.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Simulate a due task whose previous run is still in progress.
        {
            let shared = &scheduler.handles[0].shared;
            shared.running.store(true, Ordering::SeqCst);
            shared.state.lock().unwrap().next_exec =
                Some(Utc::now() - chrono::Duration::seconds(1));
        }
        scheduler.dispatch_round();

        // Skip: nothing is queued.
        assert!(!scheduler.handles[0].shared.pending.load(Ordering::SeqCst));
    }

    /// `dispatch_round` queues a missed occurrence when the policy is Queue. (X1)
    #[tokio::test]
    async fn test_dispatch_queues_running_task() {
        let mut scheduler = TaskScheduler::new(1000, Local);
        let task = TaskBuilder::new(Local)
            .every("* * * * * * *")
            .overlap(OverlapPolicy::Queue)
            .build();
        scheduler.add_task(task).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        {
            let shared = &scheduler.handles[0].shared;
            shared.running.store(true, Ordering::SeqCst);
            shared.state.lock().unwrap().next_exec =
                Some(Utc::now() - chrono::Duration::seconds(1));
        }
        scheduler.dispatch_round();

        // Queue: the missed occurrence is recorded.
        assert!(scheduler.handles[0].shared.pending.load(Ordering::SeqCst));
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
        doomed.add_step_default(|_ctx| async { Err(ErrorDelete) });
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
        scheduler.add_task(counting_task(&counter, None)).unwrap();
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
        scheduler.add_task(counting_task(&counter, None)).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Populate the registry; the task is not due for ~1s.
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
}

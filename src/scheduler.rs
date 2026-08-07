use crate::errors::{TaskError, TaskResult};
use crate::generator::TaskGenerator;
use crate::task::{run_task, OverlapPolicy, Status, Task, TaskCmd, TaskShared};
use crate::{scheduler_log, task_log};
use chrono::prelude::*;
use chrono::Utc;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

/// Handler for a running task.
///
/// Holds the task's join handle, the sender used to dispatch runs, and the shared
/// state the task publishes so the scheduler can observe it without a blocking
/// request/response round-trip.
#[derive(Debug)]
pub struct TaskHandle {
    id: usize,
    handle: JoinHandle<()>,
    sender: mpsc::Sender<TaskCmd>,
    shared: Arc<TaskShared>,
    overlap: OverlapPolicy,
}

/// A snapshot of a single task's state, as reported by [`SchedulerHandle`].
///
/// This is a read-only view refreshed at the end of every scheduler round.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TaskState {
    /// The task's id, as assigned by the scheduler.
    pub id: usize,
    /// The task's lifecycle status as of the last completed round.
    pub status: Status,
    /// The task's next execution time, normalized to UTC (`None` if not scheduled).
    pub next_exec: Option<DateTime<Utc>>,
    /// Whether a run of this task is currently in progress.
    pub running: bool,
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
    status: Arc<Mutex<Vec<TaskState>>>,
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
        self.status.lock().unwrap().len()
    }

    /// A snapshot of every live task's [`TaskState`], as of the last completed round.
    pub fn statuses(&self) -> Vec<TaskState> {
        self.status.lock().unwrap().clone()
    }

    /// The [`Status`] of the task with the given id, if it is still registered.
    pub fn status_of(&self, id: usize) -> Option<Status> {
        self.status
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.status.clone())
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
    /// Snapshot of the live tasks' states, refreshed each round and read by
    /// [`SchedulerHandle`].
    status: Arc<Mutex<Vec<TaskState>>>,
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
        TaskScheduler {
            handles: Vec::new(),
            /* Originally empty, no registered tasks. */
            task_gen: None,
            sleep: 1000,
            timezone,
            next_id: 0,
            shutdown: Arc::new(Notify::new()),
            status: Arc::new(Mutex::new(Vec::new())),
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
            status: self.status.clone(),
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
        match task {
            Ok(mut task) => {
                // A buffer of one is enough: the scheduler only dispatches a run when
                // the task is idle, so at most one `Run` is ever in flight.
                let (sender, receiver) = mpsc::channel(1);

                task.set_receiver(receiver);
                task.set_id(self.next_id);
                let overlap = task.overlap;
                let shared = Arc::new(TaskShared::new());
                let handle = tokio::spawn(run_task(task, shared.clone()));

                // Push the handle
                self.handles.push(TaskHandle {
                    id: self.next_id,
                    handle,
                    sender,
                    shared,
                    overlap,
                });

                // Increase the id of the next task.
                self.next_id += 1;
                Ok(self)
            }
            Err(e) => Err(e),
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

        // Reap tasks that have reached a terminal state and published `finished`.
        self.handles.retain(|handle| {
            if handle.shared.finished.load(Ordering::SeqCst) {
                task_log!(handle.id, log::Level::Debug, "Removing finished task");
                handle.handle.abort();
                false
            } else {
                true
            }
        });

        // Dispatch due tasks.
        for handle in &self.handles {
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

        self.refresh_status();
    }

    /// Refresh the observable status snapshot from every live task's shared state.
    fn refresh_status(&self) {
        let snapshot = self
            .handles
            .iter()
            .map(|handle| {
                let state = handle.shared.state.lock().unwrap();
                TaskState {
                    id: handle.id,
                    status: state.status.clone(),
                    next_exec: state.next_exec,
                    running: handle.shared.running.load(Ordering::SeqCst),
                }
            })
            .collect();
        *self.status.lock().unwrap() = snapshot;
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
    }

    /// Run one iteration of the scheduler flow: run the generator (if due) and
    /// dispatch the current round. Newly generated tasks initialize themselves.
    fn tick(&mut self) {
        self.run_task_gen();
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
            .add_step_default(move || {
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
            .add_step_default(move || {
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
        doomed.add_step_default(|| async { Err(ErrorDelete) });
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
        // Before running, the snapshot is empty.
        assert_eq!(handle.task_count(), 0);

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
}

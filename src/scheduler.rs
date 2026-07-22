use crate::errors::{TaskError, TaskResult};
use crate::generator::TaskGenerator;
use crate::task::{run_task, Status, Task, TaskCmd, TaskResponse};
use crate::{scheduler_log, task_log};
use chrono::prelude::*;
use chrono::Utc;
use futures::future::join_all;
use futures::StreamExt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;

/// Task execution possible statuses.
#[derive(Debug, PartialEq)]
pub(crate) enum ExecutionStatus {
    Success(usize),
    NoExecution,
    HadError(usize, usize),
}

/// Handler for task threads.
/// Contains the join handle and sender for each task.
///
/// When a task is finished the handle must be destroyed and sender dropped, in order to totally remove the task from the execution context.
///
/// The #id must be set upon the task initialization in order to be easier to query for later use.
#[derive(Debug)]
pub struct TaskHandle {
    id: usize,
    handle: JoinHandle<()>,
    sender: mpsc::Sender<TaskCmd>,
    is_init: bool,
}

/// A cloneable handle used to control a running [`TaskScheduler`].
///
/// Obtain one with [`TaskScheduler::handle`] *before* calling
/// [`TaskScheduler::run`], then call [`SchedulerHandle::shutdown`] from anywhere
/// (another task, a signal handler, etc.) to request a graceful stop.
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
                let (sender, receiver) = mpsc::channel(32);

                task.set_receiver(receiver);
                task.set_id(self.next_id);
                let handle = tokio::spawn(run_task(task));

                // Push the handle
                self.handles.push(TaskHandle {
                    id: self.next_id,
                    handle,
                    sender,
                    is_init: false,
                });

                // Increase the id of the next task.
                self.next_id += 1;
                Ok(self)
            }
            Err(e) => Err(e),
        }
    }

    /// Execute all the tasks in the queue.
    ///
    /// After the execution the tasks are rescheduled and if needed,
    /// removed from the list.
    pub(crate) async fn execute_tasks(&mut self) -> ExecutionStatus {
        let mut receivers: Vec<oneshot::Receiver<TaskResponse>> = Vec::new();

        for handle in &self.handles {
            let (sender, recv) = oneshot::channel();
            let _ = handle.sender.send(TaskCmd::Run { sender }).await;
            receivers.push(recv);
        }

        let err_no: Arc<Mutex<usize>> = Arc::new(Mutex::new(0usize));
        let total_runs: Arc<Mutex<usize>> = Arc::new(Mutex::new(0usize));
        futures::stream::iter(receivers)
            .for_each(|r| async {
                // A task whose sender was dropped (e.g. it panicked) yields a
                // `RecvError`; log and skip it rather than bringing down the scheduler.
                let status = match r.await {
                    Ok(response) => response.status,
                    Err(_) => {
                        scheduler_log!(
                            log::Level::Error,
                            "A task failed to report its run status and will be skipped"
                        );
                        return;
                    }
                };
                match status {
                    Status::Executed => {
                        *total_runs.lock().unwrap() += 1;
                    }
                    Status::Failed => {
                        *err_no.lock().unwrap() += 1;
                        *total_runs.lock().unwrap() += 1;
                    }
                    _ => { /* Do nothing */ }
                };
            })
            .await;

        // Send for reschedule
        receivers = Vec::new();
        for handle in &self.handles {
            let (send, recv) = oneshot::channel();
            let _ = handle
                .sender
                .send(TaskCmd::Reschedule { sender: send })
                .await;
            receivers.push(recv);
        }

        for recv in receivers {
            let res = match recv.await {
                Ok(res) => res,
                Err(_) => {
                    scheduler_log!(
                        log::Level::Error,
                        "A task failed to report its reschedule status and will be skipped"
                    );
                    continue;
                }
            };
            if res.status == Status::Finished || res.status == Status::ForceRemoved {
                for handle in &self.handles {
                    if handle.id == res.id {
                        task_log!(
                            res.id,
                            log::Level::Debug,
                            "Removing task due to {}",
                            if res.status == Status::Finished {
                                "end of execution cycle"
                            } else {
                                "force removal"
                            }
                        );
                        handle.handle.abort();
                    }
                }
                let index = self.handles.iter().position(|x| x.id == res.id).unwrap();
                self.handles.remove(index);
            }
        }

        // Build the response
        if *total_runs.lock().unwrap() > 0 {
            if *err_no.lock().unwrap() == 0 {
                ExecutionStatus::Success(*total_runs.lock().unwrap())
            } else {
                ExecutionStatus::HadError(*total_runs.lock().unwrap(), *err_no.lock().unwrap())
            }
        } else {
            ExecutionStatus::NoExecution
        }
    }

    /// Send an init signal to all the tasks that are not yet initialized.
    pub(crate) async fn init_tasks(&mut self) {
        let mut receivers: Vec<oneshot::Receiver<TaskResponse>> = Vec::new();
        let mut count: usize = 0;

        // Send init signal to all the tasks that are not initialized yet.
        for handle in &self.handles {
            if !handle.is_init {
                let (sender, recv) = oneshot::channel();
                let _ = handle.sender.send(TaskCmd::Init { sender }).await;
                receivers.push(recv);
                count += 1;
            }
        }

        if count > 0 {
            // Await for all receivers to finish
            join_all(receivers).await.iter().for_each(|r| match r {
                Ok(r) => match r.status {
                    Status::Scheduled => {
                        self.handles
                            .iter_mut()
                            .filter(|h| h.id == r.id)
                            .for_each(|h| {
                                task_log!(h.id, log::Level::Info, "Initialized");
                                h.is_init = true;
                            });
                    }
                    _ => {
                        task_log!(r.id, log::Level::Error, "Failed to initialize");
                    }
                },
                Err(_) => {
                    scheduler_log!(
                        log::Level::Error,
                        "A task failed to report its init status and will be skipped"
                    );
                }
            });
        }
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

    /// Run one iteration of the scheduler flow: run the generator (if due),
    /// (re)initialize tasks and execute the current round.
    async fn tick(&mut self) {
        if self.run_task_gen() {
            // Re-initialize the tasks if any new is added
            self.init_tasks().await;
        }
        match self.execute_tasks().await {
            ExecutionStatus::Success(c) => {
                scheduler_log!(
                    log::Level::Info,
                    "Execution round completed successfully for {} task(s)",
                    c
                );
            }
            ExecutionStatus::HadError(c, e) => {
                scheduler_log!(
                    log::Level::Error,
                    "Execution round ran {} task(s) with {} error(s)",
                    c,
                    e
                );
            }
            _ => { /* No executions */ }
        }
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

        // Initialize the tasks
        self.init_tasks().await;

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
                    self.tick().await;
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
    use crate::task::TaskStepStatusErr::{Error, ErrorDelete};
    use crate::task::TaskStepStatusOk::Success;
    use crate::TaskBuilder;
    use chrono::Local;
    use std::time::Duration;

    #[tokio::test]
    async fn test_scheduler_normal_flow() {
        // Create a new scheduler instance.
        let mut scheduler = TaskScheduler::new(500, Local);
        // Add a couple of tasks.
        scheduler
            .add_task(Task::new("* * * * * * *", None, Some(2), Local))
            .unwrap()
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        assert_eq!(scheduler.handles.len(), 2);
        // Initialize the tasks.
        scheduler.init_tasks().await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let status: ExecutionStatus = scheduler.execute_tasks().await;
        assert_eq!(status, ExecutionStatus::Success(2));
        assert_eq!(scheduler.handles.len(), 2);
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let status: ExecutionStatus = scheduler.execute_tasks().await;
        assert_eq!(status, ExecutionStatus::Success(2));
        assert_eq!(scheduler.handles.len(), 1);
    }

    #[tokio::test]
    async fn test_scheduler_normal_force_deletion() {
        // Create a new scheduler instance.
        let mut scheduler = TaskScheduler::new(500, Local);
        // Create a task.
        let mut task = Task::new("* * * * * * *", None, Some(1), Local).unwrap();
        task.add_step_default(|| async { Err(ErrorDelete) });
        // Add a couple of tasks.
        scheduler
            .add_task(Ok(task))
            .unwrap()
            .add_task(Task::new("* * * * * * *", None, None, Local))
            .unwrap();
        assert_eq!(scheduler.handles.len(), 2);
        // Initialize the tasks.
        scheduler.init_tasks().await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        scheduler.execute_tasks().await;
        // The first task should be force-removed at this point
        assert_eq!(scheduler.handles.len(), 1);
        tokio::time::sleep(Duration::from_millis(1000)).await;
        scheduler.execute_tasks().await;
        assert_eq!(scheduler.handles.len(), 1);
    }

    #[tokio::test]
    async fn test_scheduler_normal_flow_no_execution() {
        // Create a new scheduler instance.
        let mut scheduler = TaskScheduler::new(500, Local);
        // Init the scheduler.
        scheduler.init_tasks().await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // Run the scheduler
        let status: ExecutionStatus = scheduler.execute_tasks().await;
        // No tasks should be executed.
        assert_eq!(status, ExecutionStatus::NoExecution);
    }

    #[tokio::test]
    async fn test_scheduler_normal_flow_error_case() {
        // Create a new scheduler instance.
        let mut scheduler = TaskScheduler::new(500, Local);

        // Create a task.
        let mut task = Task::new("* * * * * * *", None, Some(1), Local).unwrap();
        task.add_step_default(|| async { Ok(Success) });
        // Return an error in the second step.
        task.add_step_default(|| async { Err(Error) });

        // Add a task.
        scheduler.add_task(Ok(task)).unwrap();
        assert_eq!(scheduler.handles.len(), 1);
        // Initialize the task.
        scheduler.init_tasks().await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        scheduler.execute_tasks().await;
        // The task should be removed after it's execution circle.
        assert_eq!(scheduler.handles.len(), 0);
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
}

//! An asynchronous, cron-style task scheduler for Tokio.
//!
//! A task is a cron schedule plus a list of async steps. [`TaskBuilder`] builds tasks,
//! [`TaskScheduler`] runs them (each task in its own Tokio task, so a slow task never
//! delays the others), and a [`SchedulerHandle`] observes and controls the scheduler
//! while it runs.
//!
//! # Highlights
//!
//! * **Async steps.** Every step is a closure returning a future, so it can `.await`
//!   I/O and timers without blocking the runtime. Each step receives a [`TaskContext`]
//!   with the task id and name, run id, step index and attempt, a task-level
//!   [`Blackboard`] shared across runs, and a per-run store for passing values between
//!   steps.
//! * **Failures with reasons.** A step fails with
//!   [`TaskStepStatusErr::error`](task::TaskStepStatusErr::error) (retryable) or
//!   [`TaskStepStatusErr::error_delete`](task::TaskStepStatusErr::error_delete)
//!   (removes the task). The reason is kept in [`StepState::error`] and
//!   [`RunRecord::error`].
//! * **Timeouts, retries and callbacks.** Set a per-attempt [`TaskBuilder::timeout`], a
//!   [`RetryPolicy`] (with optional [`Jitter`]) through [`TaskBuilder::retry`], and async
//!   callbacks: [`TaskBuilder::on_success`] and [`on_failure`](TaskBuilder::on_failure)
//!   receive a [`RunReport`], [`on_finish`](TaskBuilder::on_finish) receives a
//!   [`FinishReport`] with the [`FinishReason`].
//! * **Scheduling.** Use cron expressions or the helpers [`TaskBuilder::every_seconds`],
//!   [`every_minutes`](TaskBuilder::every_minutes), [`every_hours`](TaskBuilder::every_hours),
//!   [`hourly_at`](TaskBuilder::hourly_at) and [`daily_at`](TaskBuilder::daily_at). The
//!   scheduler sleeps until the next task is due, so runs start on their slot. An
//!   [`OverlapPolicy`] decides what happens when a slot comes due during a run, and a
//!   [`MissedRunPolicy`] what happens to a run that starts late.
//! * **Concurrency limits.** Cap runs in progress with
//!   [`TaskScheduler::max_concurrent_runs`], or per named pool with
//!   [`TaskScheduler::add_pool`] and [`TaskBuilder::pool`].
//! * **Runtime control.** Name a task with [`TaskBuilder::name`] and pause, resume,
//!   trigger or remove it through a [`SchedulerHandle`], by id or by name. Add tasks to
//!   a running scheduler with a [`TaskSpawner`] from [`TaskScheduler::spawner`], or from
//!   a [`TaskGenerator`].
//! * **Graceful shutdown.** [`SchedulerHandle::shutdown`] or
//!   [`TaskScheduler::run_until`] stop the scheduler. Removed and stopping tasks see the
//!   request through [`TaskContext::is_cancelled`] and [`TaskContext::cancelled`]; runs
//!   in progress get a [grace period](TaskScheduler::grace_period) (5 seconds by default)
//!   and are aborted after it. `TaskScheduler` is `Send`, so it can run inside
//!   `tokio::spawn`.
//! * **Observability.** Read task snapshots ([`SchedulerHandle::statuses`]), run history
//!   ([`SchedulerHandle::history`]) and live step states
//!   ([`SchedulerHandle::step_states`]), or subscribe to a stream of [`SchedulerEvent`]s
//!   with [`SchedulerHandle::subscribe`]. With the `serde` feature, these types implement
//!   `Serialize` and `Deserialize`.
//!
//! Guides and the migration guide for each breaking release are in the
//! [`docs` folder](https://github.com/stav121/tasklet/tree/main/docs) of the repository.
//!
//! # Example
//!
//! ```no_run
//! use tasklet::task::TaskStepStatusErr;
//! use tasklet::task::TaskStepStatusOk::Success;
//! use tasklet::{TaskBuilder, TaskScheduler};
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut scheduler = TaskScheduler::default(chrono::Local);
//!     scheduler
//!         .add_task(
//!             TaskBuilder::new(chrono::Local)
//!                 .every("1 * * * * * *")
//!                 .description("A simple task")
//!                 .add_step("Step 1", |_ctx| async { Ok(Success) })
//!                 .add_step("Step 2", |ctx| async move {
//!                     if ctx.run_id() % 2 == 0 {
//!                         Ok(Success)
//!                     } else {
//!                         Err(TaskStepStatusErr::error("odd run"))
//!                     }
//!                 })
//!                 .on_failure(|report| async move {
//!                     eprintln!("run failed: {:?}", report.record.error);
//!                 })
//!                 .build(),
//!         )
//!         .unwrap();
//!
//!     // Stop after an hour; a run in progress gets the grace period to finish.
//!     let handle = scheduler.handle();
//!     tokio::spawn(async move {
//!         tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
//!         handle.shutdown();
//!     });
//!
//!     scheduler.run().await;
//! }
//! ```

mod blackboard;
mod builders;
mod cancel;
pub mod errors;
pub mod events;
mod generator;
mod limits;
pub mod retry;
mod scheduler;
pub mod task;

pub use blackboard::Blackboard;
pub use builders::TaskBuilder;
pub use errors::{TaskError, TaskResult};
pub use events::SchedulerEvent;
pub use generator::TaskGenerator;
pub use retry::{Backoff, Jitter, RetryPolicy};
pub use scheduler::{SchedulerHandle, TaskScheduler, TaskSpawner, TaskState};
pub use task::{
    FinishReason, FinishReport, MissedRunPolicy, OverlapPolicy, RunOutcome, RunRecord, RunReport,
    Status, StepState, StepStatus, Task, TaskContext,
};

/// Macro for consistent task-related logging
///
/// # Examples
///
/// ```
/// use tasklet::task_log;
/// use log::Level;
///
/// // Log an info message for task 1
/// task_log!(1, Level::Info, "Task started with parameter: {}", "example");
/// ```
#[macro_export]
macro_rules! task_log {
    ($task_id:expr, $level:expr, $message:expr $(, $args:expr)*) => {
        match $level {
            log::Level::Error => log::error!("[Task {}] {}", $task_id, format!($message $(, $args)*)),
            log::Level::Warn => log::warn!("[Task {}] {}", $task_id, format!($message $(, $args)*)),
            log::Level::Info => log::info!("[Task {}] {}", $task_id, format!($message $(, $args)*)),
            log::Level::Debug => log::debug!("[Task {}] {}", $task_id, format!($message $(, $args)*)),
            log::Level::Trace => log::trace!("[Task {}] {}", $task_id, format!($message $(, $args)*)),
        }
    };
}

/// Macro for consistent task step logging
///
/// # Examples
///
/// ```
/// use tasklet::step_log;
/// use log::Level;
///
/// // Log a debug message for task 1, step 2
/// step_log!(1, 2, Level::Debug, "Step completed successfully with result: {}", "success");
/// ```
#[macro_export]
macro_rules! step_log {
    ($task_id:expr, $step_idx:expr, $level:expr, $message:expr $(, $args:expr)*) => {
        match $level {
            log::Level::Error => log::error!("[Task {}-Step {}] {}", $task_id, $step_idx, format!($message $(, $args)*)),
            log::Level::Warn => log::warn!("[Task {}-Step {}] {}", $task_id, $step_idx, format!($message $(, $args)*)),
            log::Level::Info => log::info!("[Task {}-Step {}] {}", $task_id, $step_idx, format!($message $(, $args)*)),
            log::Level::Debug => log::debug!("[Task {}-Step {}] {}", $task_id, $step_idx, format!($message $(, $args)*)),
            log::Level::Trace => log::trace!("[Task {}-Step {}] {}", $task_id, $step_idx, format!($message $(, $args)*)),
        }
    };
}

/// Macro for consistent scheduler logging
///
/// # Examples
///
/// ```
/// use tasklet::scheduler_log;
/// use log::Level;
///
/// // Log a warning message from the scheduler
/// scheduler_log!(Level::Warn, "Failed to execute task with ID: {}", 5);
/// ```
#[macro_export]
macro_rules! scheduler_log {
    ($level:expr, $message:expr $(, $args:expr)*) => {
        match $level {
            log::Level::Error => log::error!("[Scheduler] {}", format!($message $(, $args)*)),
            log::Level::Warn => log::warn!("[Scheduler] {}", format!($message $(, $args)*)),
            log::Level::Info => log::info!("[Scheduler] {}", format!($message $(, $args)*)),
            log::Level::Debug => log::debug!("[Scheduler] {}", format!($message $(, $args)*)),
            log::Level::Trace => log::trace!("[Scheduler] {}", format!($message $(, $args)*)),
        }
    };
}

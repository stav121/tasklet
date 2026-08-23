//! An asynchronous task scheduling library written in Rust.
//!
//! `tasklet` allows you to create scheduled tasks with specific execution patterns and
//! run them asynchronously using Tokio. It supports cron-like scheduling expressions and
//! provides a builder pattern for easy task creation.
//!
//! # Highlights
//!
//! * **Async steps** — every task step is a closure returning a future, so steps can
//!   `.await` real asynchronous work (I/O, timers, ...) without blocking the runtime.
//! * **Graceful shutdown** — obtain a [`SchedulerHandle`] via
//!   [`TaskScheduler::handle`] before running and call `shutdown()` to stop the scheduler
//!   cleanly, or drive shutdown with any future via [`TaskScheduler::run_until`].
//! * **Resilience** — configure a per-step [`TaskBuilder::timeout`], a
//!   [`RetryPolicy`] (with optional [`Jitter`]) via [`TaskBuilder::retry`], and async
//!   lifecycle callbacks ([`TaskBuilder::on_success`] /
//!   [`on_failure`](TaskBuilder::on_failure) / [`on_finish`](TaskBuilder::on_finish)).
//! * **Non-blocking scheduling** — a slow task never delays other tasks. Control what
//!   happens when a run overruns its interval with an [`OverlapPolicy`] via
//!   [`TaskBuilder::overlap`].
//! * **Observability** — query the live task set at runtime through
//!   [`SchedulerHandle::task_count`] / [`SchedulerHandle::statuses`], including per-task
//!   run history ([`SchedulerHandle::history`]) and per-step state
//!   ([`SchedulerHandle::step_states`]), which streams step transitions live during a run.
//! * **Schedule helpers** — build common cadences without raw cron:
//!   [`TaskBuilder::every_seconds`], [`every_minutes`](TaskBuilder::every_minutes),
//!   [`every_hours`](TaskBuilder::every_hours), [`hourly_at`](TaskBuilder::hourly_at) and
//!   [`daily_at`](TaskBuilder::daily_at). Capture a task's id with
//!   [`TaskScheduler::add_task_get_id`].
//! * **Runtime control** — name a task with [`TaskBuilder::name`] and pause, resume,
//!   trigger or remove it at runtime through a [`SchedulerHandle`], by id or by name.
//! * **Shared data** — pass values between tasks and steps with a cheaply-clonable,
//!   typed [`Blackboard`].
//!
//! # Example
//!
//! ```no_run
//! use tasklet::task::TaskStepStatusOk::Success;
//! use tasklet::{TaskBuilder, TaskScheduler};
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut scheduler = TaskScheduler::default(chrono::Local);
//!     let _ = scheduler.add_task(
//!         TaskBuilder::new(chrono::Local)
//!             .every("1 * * * * * *")
//!             .description("A simple task")
//!             .add_step("Step 1", || async { Ok(Success) })
//!             .build(),
//!     );
//!     scheduler.run().await;
//! }
//! ```

mod blackboard;
mod builders;
pub mod errors;
mod generator;
pub mod retry;
mod scheduler;
pub mod task;

pub use blackboard::Blackboard;
pub use builders::TaskBuilder;
pub use errors::{TaskError, TaskResult};
pub use generator::TaskGenerator;
pub use retry::{Backoff, Jitter, RetryPolicy};
pub use scheduler::{SchedulerHandle, TaskScheduler, TaskState};
pub use task::{OverlapPolicy, RunOutcome, RunRecord, Status, StepState, StepStatus, Task};

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

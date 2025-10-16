//! An asynchronous task scheduling library written in Rust.
//!
//! `tasklet` allows you to create scheduled tasks with specific execution patterns and
//! run them asynchronously using Tokio. It supports cron-like scheduling expressions and
//! provides a builder pattern for easy task creation.

mod builders;
pub mod errors;
mod generator;
mod scheduler;
pub mod task;

pub use builders::TaskBuilder;
pub use errors::{TaskError, TaskResult};
pub use generator::TaskGenerator;
pub use scheduler::TaskScheduler;
pub use task::Task;

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

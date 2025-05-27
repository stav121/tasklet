//! Error types for the tasklet library.

use thiserror::Error;

/// Errors that can occur when working with tasks.
#[derive(Error, Debug)]
pub enum TaskError {
    /// The task is not initialized yet.
    #[error("Task not initialized yet")]
    NotInitialized,

    /// The task has already been executed and must be rescheduled.
    #[error("Task already executed and must be rescheduled")]
    AlreadyExecuted,

    /// The task has failed and must be rescheduled.
    #[error("Task failed and must be rescheduled")]
    Failed,

    /// The task has finished and must be removed.
    #[error("Task has finished and must be removed")]
    Finished,

    /// The task has been force removed.
    #[error("Task has been force removed")]
    ForceRemoved,

    /// The task's schedule could not be parsed.
    #[error("Invalid cron expression: {0}")]
    InvalidCronExpression(String),

    /// A required component is missing.
    #[error("Missing required component: {0}")]
    MissingComponent(String),

    /// A generic error occurred during task execution.
    #[error("Task execution error: {0}")]
    ExecutionError(String),
}

/// Result type for task operations.
pub type TaskResult<T> = Result<T, TaskError>;

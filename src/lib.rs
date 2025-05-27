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

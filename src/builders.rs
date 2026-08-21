use crate::errors::{TaskError, TaskResult};
use crate::retry::RetryPolicy;
use crate::task::{
    boxed_callback, CallbackFn, OverlapPolicy, Task, TaskStep, TaskStepStatusErr, TaskStepStatusOk,
    DEFAULT_HISTORY_LIMIT,
};
use chrono::TimeZone;
use cron::Schedule;
use std::future::Future;
use std::time::Duration;

/// Task builder function.
///
/// Used to generate/build a `TaskStep` instance.
pub struct TaskBuilder<T>
where
    T: TimeZone + Send + 'static,
{
    /// An optional task description.
    description: Option<String>,
    /// An optional unique name used to address the task at runtime.
    name: Option<String>,
    /// The maximum number of run records to retain for the task.
    history_limit: usize,
    /// The provided `TaskStep` vector.
    steps: Vec<TaskStep>,
    /// The provided `Schedule`, if not given,
    /// it will be defaulted to once every hour.
    schedule: Option<Schedule>,
    /// The original expression string, for error reporting
    expression: String,
    /// Max number of repeats.
    repeats: Option<usize>,
    /// (Optional) per-step execution timeout.
    timeout: Option<Duration>,
    /// (Optional) retry policy for failing steps.
    retry_policy: Option<RetryPolicy>,
    /// (Optional) callback invoked after a successful execution.
    on_success: Option<Box<CallbackFn>>,
    /// (Optional) callback invoked after a failed execution.
    on_failure: Option<Box<CallbackFn>>,
    /// (Optional) callback invoked once the task reaches a terminal state.
    on_finish: Option<Box<CallbackFn>>,
    /// Behaviour when a run overlaps a still-running one.
    overlap: OverlapPolicy,
    /// The Task/Scheduler timezone.
    timezone: T,
}

impl<T> TaskBuilder<T>
where
    T: TimeZone + Send + 'static,
{
    /// Create a new `TaskBuilder` instance.
    ///
    /// # Arguments
    ///
    /// * timezone  - A valid timezone for the generated `Task`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::TaskBuilder;
    /// let _task_builder = TaskBuilder::new(chrono::Utc);
    /// ```
    pub fn new(timezone: T) -> TaskBuilder<T> {
        TaskBuilder {
            steps: Vec::new(),
            description: None,
            name: None,
            history_limit: DEFAULT_HISTORY_LIMIT,
            schedule: None,
            expression: "* * * * * * *".to_string(), // Default expression
            repeats: None,
            timeout: None,
            retry_policy: None,
            on_success: None,
            on_failure: None,
            on_finish: None,
            overlap: OverlapPolicy::default(),
            timezone,
        }
    }

    /// Set the optional description of the generated `Task`.
    ///
    /// # Arguments
    ///
    /// - description   - A description for the task.
    ///
    /// ```rust
    /// # use tasklet::TaskBuilder;
    /// let _task = TaskBuilder::new(chrono::Local).every("* * * * * * *").description("Description").build().unwrap();
    /// ```
    pub fn description(mut self, description: &str) -> TaskBuilder<T> {
        self.description = Some(description.to_string());
        self
    }

    /// Set a unique name for the generated `Task`.
    ///
    /// A name is a stable, human-friendly identifier that can be used to address the
    /// task at runtime through a [`SchedulerHandle`](crate::SchedulerHandle) (pause,
    /// resume, trigger, remove, query). Names must be unique within a scheduler:
    /// [`add_task`](crate::TaskScheduler::add_task) rejects a duplicate with
    /// [`TaskError::DuplicateTaskName`](crate::TaskError::DuplicateTaskName).
    ///
    /// # Arguments
    ///
    /// * name  - A unique name for the task.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::TaskBuilder;
    /// let _task = TaskBuilder::new(chrono::Local)
    ///     .every("* * * * * * *")
    ///     .name("nightly-report")
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn name(mut self, name: &str) -> TaskBuilder<T> {
        self.name = Some(name.to_string());
        self
    }

    /// Set how many recent run records the generated `Task` retains.
    ///
    /// The scheduler keeps a bounded, per-task history of
    /// [`RunRecord`](crate::task::RunRecord)s observable through a
    /// [`SchedulerHandle`](crate::SchedulerHandle). The default is
    /// 20; a value of `0` disables history (runs are still counted).
    ///
    /// # Arguments
    ///
    /// * limit - The maximum number of run records to keep.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::TaskBuilder;
    /// let _task = TaskBuilder::new(chrono::Local)
    ///     .every("* * * * * * *")
    ///     .history_limit(100)
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn history_limit(mut self, limit: usize) -> TaskBuilder<T> {
        self.history_limit = limit;
        self
    }

    /// Set the execution schedule of the task to be generated.
    ///
    /// # Arguments
    ///
    /// * expression  - A valid cron expression.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::{TaskBuilder, Task};
    /// let _task = TaskBuilder::new(chrono::Local).every("* * * * * * *").build().unwrap();
    /// ```
    pub fn every(mut self, expression: &str) -> TaskBuilder<T> {
        self.expression = expression.to_string();
        match expression.parse() {
            Ok(schedule) => {
                self.schedule = Some(schedule);
            }
            Err(_) => {
                // We'll validate at build time
                self.schedule = None;
            }
        };
        self
    }

    /// Set the max repeats for the generated `Task`.
    ///
    /// # Arguments
    ///
    /// * repeats   - The max amount of repeats.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::TaskBuilder;
    /// let _task = TaskBuilder::new(chrono::Local).repeat(5);
    /// ```
    pub fn repeat(mut self, repeat: usize) -> TaskBuilder<T> {
        self.repeats = Some(repeat);
        self
    }

    /// Set a per-step execution timeout for the generated `Task`.
    ///
    /// A step whose future does not resolve within `timeout` is cancelled and
    /// treated as a (retryable) failure.
    ///
    /// # Arguments
    ///
    /// * timeout   - The maximum duration allowed for a single step attempt.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::time::Duration;
    /// # use tasklet::TaskBuilder;
    /// let _task = TaskBuilder::new(chrono::Local)
    ///     .every("* * * * * * *")
    ///     .timeout(Duration::from_secs(5))
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn timeout(mut self, timeout: Duration) -> TaskBuilder<T> {
        self.timeout = Some(timeout);
        self
    }

    /// Set the retry policy applied to failing steps of the generated `Task`.
    ///
    /// Only steps returning [`TaskStepStatusErr::Error`] (or timing out) are retried;
    /// [`TaskStepStatusErr::ErrorDelete`] bypasses retries.
    ///
    /// # Arguments
    ///
    /// * policy    - The [`RetryPolicy`] to apply.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::time::Duration;
    /// # use tasklet::{RetryPolicy, TaskBuilder};
    /// let _task = TaskBuilder::new(chrono::Local)
    ///     .every("* * * * * * *")
    ///     .retry(RetryPolicy::fixed(3, Duration::from_millis(100)))
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn retry(mut self, policy: RetryPolicy) -> TaskBuilder<T> {
        self.retry_policy = Some(policy);
        self
    }

    /// Set the overlap policy: what to do when a scheduled run comes due while a
    /// previous run of this task is still in progress.
    ///
    /// # Arguments
    ///
    /// * overlap   - The [`OverlapPolicy`] to apply (defaults to `Skip`).
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::{OverlapPolicy, TaskBuilder};
    /// let _task = TaskBuilder::new(chrono::Local)
    ///     .every("* * * * * * *")
    ///     .overlap(OverlapPolicy::Queue)
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn overlap(mut self, overlap: OverlapPolicy) -> TaskBuilder<T> {
        self.overlap = overlap;
        self
    }

    /// Register a callback invoked after each successful execution of the task.
    ///
    /// # Arguments
    ///
    /// * callback  - An async closure invoked when a run completes successfully.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::TaskBuilder;
    /// let _task = TaskBuilder::new(chrono::Local)
    ///     .every("* * * * * * *")
    ///     .on_success(|| async { println!("task succeeded"); })
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn on_success<F, Fut>(mut self, callback: F) -> TaskBuilder<T>
    where
        F: (FnMut() -> Fut) + 'static + Send,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_success = Some(boxed_callback(callback));
        self
    }

    /// Register a callback invoked after a failed execution of the task.
    ///
    /// # Arguments
    ///
    /// * callback  - An async closure invoked when a run fails.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::TaskBuilder;
    /// let _task = TaskBuilder::new(chrono::Local)
    ///     .every("* * * * * * *")
    ///     .on_failure(|| async { eprintln!("task failed"); })
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn on_failure<F, Fut>(mut self, callback: F) -> TaskBuilder<T>
    where
        F: (FnMut() -> Fut) + 'static + Send,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_failure = Some(boxed_callback(callback));
        self
    }

    /// Register a callback invoked once when the task reaches a terminal state
    /// (its repeat cycle is exhausted or it is force-removed).
    ///
    /// # Arguments
    ///
    /// * callback  - An async closure invoked when the task finishes.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::TaskBuilder;
    /// let _task = TaskBuilder::new(chrono::Local)
    ///     .every("* * * * * * *")
    ///     .repeat(1)
    ///     .on_finish(|| async { println!("task finished"); })
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn on_finish<F, Fut>(mut self, callback: F) -> TaskBuilder<T>
    where
        F: (FnMut() -> Fut) + 'static + Send,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_finish = Some(boxed_callback(callback));
        self
    }

    /// Add a new step for the generated task.
    ///
    /// # Arguments
    ///
    /// * description   - An optional description for the task's step.
    /// * function      - The executable body of the task's step.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::task::TaskStepStatusErr::Error;
    /// # use tasklet::TaskBuilder;
    /// let _ = TaskBuilder::new(chrono::Utc).add_step("A step that fails.", || async { Err(Error) });
    /// ```
    pub fn add_step<F, Fut>(mut self, description: &str, function: F) -> TaskBuilder<T>
    where
        F: (FnMut() -> Fut) + Send + 'static,
        Fut: std::future::Future<Output = Result<TaskStepStatusOk, TaskStepStatusErr>>
            + Send
            + 'static,
    {
        self.steps.push(TaskStep::new(description, function));
        self
    }

    /// Add a new step to the generated task (without description).
    ///
    /// # Arguments
    ///
    /// * function  - The executable body of the task's step.
    ///
    /// ```
    /// # use tasklet::task::TaskStepStatusOk::Success;
    /// use tasklet::TaskBuilder;
    /// let _ = TaskBuilder::new(chrono::Local).add_step_default(|| async { Ok(Success) });
    /// ```
    pub fn add_step_default<F, Fut>(mut self, function: F) -> TaskBuilder<T>
    where
        F: (FnMut() -> Fut) + 'static + Send,
        Fut: std::future::Future<Output = Result<TaskStepStatusOk, TaskStepStatusErr>>
            + Send
            + 'static,
    {
        self.steps.push(TaskStep::default(function));
        self
    }

    /// Build a new `Task` instance from the current configuration.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use tasklet::{TaskBuilder, Task};
    /// let mut _task = TaskBuilder::new(chrono::Utc).build().unwrap();
    /// ```
    pub fn build(self) -> TaskResult<Task<T>> {
        // Validate schedule if provided
        let schedule = match self.schedule {
            Some(s) => s,
            None => {
                // Try to parse the expression
                self.expression.parse().map_err(|e| {
                    TaskError::InvalidCronExpression(format!(
                        "Invalid cron expression '{}': {}",
                        self.expression, e
                    ))
                })?
            }
        };

        // Create the task with default expression - we'll replace the schedule after
        let mut task = Task::new(
            "* * * * * * *", // This is just a placeholder, we'll set the real schedule next
            self.description.as_deref(),
            self.repeats,
            self.timezone,
        )?;

        // Set the validated schedule
        task.set_schedule(schedule);

        // Set the steps
        task.set_steps(self.steps);

        // Transfer the identity / history configuration.
        if let Some(name) = self.name.as_deref() {
            task.set_name(name);
        }
        task.set_history_limit(self.history_limit);

        // Transfer the optional timeout / retry configuration.
        if let Some(timeout) = self.timeout {
            task.set_timeout(timeout);
        }
        if let Some(policy) = self.retry_policy {
            task.set_retry_policy(policy);
        }
        task.set_overlap(self.overlap);

        // Transfer the lifecycle callbacks.
        task.set_callbacks(self.on_success, self.on_failure, self.on_finish);

        Ok(task)
    }
}

/// Module's tests.
#[cfg(test)]
mod test {
    use super::*;
    use crate::task::TaskStepStatusOk::Success;

    /// Test helper macros.
    ///
    /// Assert a given list of `Option<>` is `None`.
    macro_rules! assert_none {
      ($x:expr) => (assert_eq!($x.is_some(), false););
      ($x:expr, $($y:expr),+) => (
            assert_none!($x);
            assert_none!($($y),+);
            );
    }

    /// Test helper macros.
    ///
    /// Assert a given list of `Option<>` is `Some`
    macro_rules! assert_some {
        ($x:expr) => (assert_eq!($x.is_some(), true););
        ($x:expr, $($y:expr),+) => (
            assert_some!($x);
            assert_some!($($y),+);
          );
    }

    /// Test the normal initialization of a `TaskBuilder`.
    #[test]
    pub fn test_task_builder_init() {
        let builder = TaskBuilder::new(chrono::Utc);
        assert_none!(builder.repeats);
        assert_eq!(builder.steps.len(), 0);
        assert_eq!(builder.timezone, chrono::Utc);
    }

    /// Test the normal functionality of the description() function of `TaskBuilder`.
    #[test]
    pub fn test_task_builder_with_description() {
        let builder = TaskBuilder::new(chrono::Utc).description("Some description");
        assert_none!(builder.repeats);
        assert_eq!(builder.steps.len(), 0);
        assert_some!(builder.description);
        assert_eq!(builder.timezone, chrono::Utc);
    }

    /// Test the normal initialization of a task with a schedule.
    #[test]
    pub fn test_task_builder_with_schedule() {
        let builder = TaskBuilder::new(chrono::Utc).every("* * * * * * *");
        assert_eq!(builder.timezone, chrono::Utc);
        assert_none!(builder.repeats, builder.description);
        assert_eq!(builder.steps.len(), 0);
        assert_some!(builder.schedule);
    }

    /// Test the normal functionality of the repeat() function of the `TaskBuilder`.
    #[test]
    pub fn test_task_builder_repeat() {
        let builder = TaskBuilder::new(chrono::Utc).repeat(5);
        assert_eq!(builder.timezone, chrono::Utc);
        assert_eq!(builder.steps.len(), 0);
        assert_some!(builder.repeats);
    }

    /// Test the normal functionality of the add_step() function of the `TaskBuilder`.
    #[test]
    pub fn test_task_builder_add_step() {
        let builder = TaskBuilder::new(chrono::Utc).add_step_default(|| async { Ok(Success) });
        assert_eq!(builder.timezone, chrono::Utc);
        assert_eq!(builder.steps.len(), 1);
    }

    /// Test the normal functionality of build() function of the `TaskBuilder`.
    #[test]
    pub fn test_task_builder_build() {
        let task = TaskBuilder::new(chrono::Utc)
            .every("* * * * * * *")
            .repeat(5)
            .description("Some description")
            .add_step("Step 1", || async { Ok(Success) })
            .build()
            .unwrap();
        assert_some!(task.repeats);
        assert_eq!(task.description, "Some description");
        assert_eq!(task.timezone, chrono::Utc);
        assert_eq!(task.steps.len(), 1);
    }

    /// Test the normal functionality of build() function of the `TaskBuilder`.
    #[test]
    pub fn test_task_builder_build_default() {
        let task = TaskBuilder::new(chrono::Utc)
            .repeat(5)
            .add_step("Step 1", || async { Ok(Success) })
            .build()
            .unwrap();
        assert_some!(task.repeats);
        assert_eq!(task.timezone, chrono::Utc);
        assert_eq!(task.steps.len(), 1);
    }

    /// Test building with an invalid cron expression
    #[test]
    pub fn test_task_builder_invalid_expression() {
        let result = TaskBuilder::new(chrono::Utc)
            .every("invalid expression")
            .build();
        assert!(result.is_err());
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

    /// `name` and `history_limit` are transferred onto the built task. (Layer 0)
    #[test]
    fn test_task_builder_name_and_history_limit() {
        let task = TaskBuilder::new(chrono::Utc)
            .every("* * * * * * *")
            .name("my-task")
            .history_limit(5)
            .build()
            .unwrap();
        assert_eq!(task.name.as_deref(), Some("my-task"));
        assert_eq!(task.history_limit, 5);
    }

    /// The default history limit is applied when not overridden. (Layer 0)
    #[test]
    fn test_task_builder_default_history_limit() {
        let task = TaskBuilder::new(chrono::Utc)
            .every("* * * * * * *")
            .build()
            .unwrap();
        assert_eq!(task.history_limit, DEFAULT_HISTORY_LIMIT);
        assert!(task.name.is_none());
    }
}

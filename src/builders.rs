use crate::errors::{TaskError, TaskResult};
use crate::task::{Task, TaskStep, TaskStepStatusErr, TaskStepStatusOk};
use chrono::TimeZone;
use cron::Schedule;

/// Task builder function.
///
/// Used to generate/build a `TaskStep` instance.
pub struct TaskBuilder<T>
where
    T: TimeZone + Send + 'static,
{
    /// An optional task description.
    description: Option<String>,
    /// The provided `TaskStep` vector.
    steps: Vec<TaskStep>,
    /// The provided `Schedule`, if not given,
    /// it will be defaulted to once every hour.
    schedule: Option<Schedule>,
    /// The original expression string, for error reporting
    expression: String,
    /// Max number of repeats.
    repeats: Option<usize>,
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
            schedule: None,
            expression: "* * * * * * *".to_string(), // Default expression
            repeats: None,
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
    /// let _ = TaskBuilder::new(chrono::Utc).add_step("A step that fails.", || Err(Error));
    /// ```
    pub fn add_step<F>(mut self, description: &str, function: F) -> TaskBuilder<T>
    where
        F: (FnMut() -> Result<TaskStepStatusOk, TaskStepStatusErr>) + Send + 'static,
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
    /// let _ = TaskBuilder::new(chrono::Local).add_step_default(|| Ok(Success));
    /// ```
    pub fn add_step_default<F>(mut self, function: F) -> TaskBuilder<T>
    where
        F: (FnMut() -> Result<TaskStepStatusOk, TaskStepStatusErr>) + 'static + Send,
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
        let builder = TaskBuilder::new(chrono::Utc).add_step_default(|| Ok(Success));
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
            .add_step("Step 1", || Ok(Success))
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
            .add_step("Step 1", || Ok(Success))
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
}

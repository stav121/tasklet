use crate::generator::TaskGenerator;
use crate::task::{Status, Task};
use chrono::prelude::*;
use chrono::Utc;
use log::{debug, error, info, warn};
use std::thread;
use std::time::Duration;

/// Task execution possible statuses.
pub(crate) enum ExecutionStatus {
    Success,
    HadError(usize),
}

/// Task scheduler and executor.
pub struct TaskScheduler<'a, T>
where
    T: TimeZone + Clone,
{
    /// The list of tasks to be executed.
    tasks: Vec<Task<'a, T>>,
    /// The (optional) task generation function.
    task_gen: Option<TaskGenerator<'a, T>>,
    /// The sleep time in ms.
    sleep: usize,
    /// The id that should be assigned to the next appended task.
    next_id: usize,
    /// The main timezone used for the scheduler.
    timezone: T,
}

/// `TaskScheduler` implementation.
impl<'a, T> TaskScheduler<'a, T>
where
    T: TimeZone + Clone,
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
    pub fn default(timezone: T) -> TaskScheduler<'a, T> {
        TaskScheduler {
            tasks: Vec::new(),
            task_gen: None,
            sleep: 1000,
            timezone,
            next_id: 0,
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
    pub fn new(sleep: usize, timezone: T) -> TaskScheduler<'a, T> {
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
    /// let mut generator = TaskGenerator::new("1 * * * * * *", chrono::Local, || None);
    /// scheduler.set_task_gen(generator);
    /// ```
    pub fn set_task_gen(&mut self, task_gen: TaskGenerator<'a, T>) -> &mut TaskScheduler<'a, T> {
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
    /// // Create a new `TaskScheduler` and attach a task to it.
    /// let mut scheduler = TaskScheduler::default(chrono::Local);
    /// // Add a task that executes every second forever.
    /// scheduler.add_task(Task::new("* * * * * * *", None, None, chrono::Local));
    /// ```
    pub fn add_task(&mut self, task: Task<'a, T>) -> &mut TaskScheduler<'a, T> {
        self.tasks.push(task);
        self
    }

    /// Execute all the tasks in the queue.
    /// After the execution the tasks are rescheduled and if needed,
    /// removed from the list.
    pub(crate) fn execute_tasks(&mut self) -> ExecutionStatus {
        // Ids of the tasks to be removed at the end of iterations.
        let mut finished_ids: Vec<usize> = Vec::new();
        let total = self.tasks.len();
        let mut err_count: usize = 0;

        for (index, task) in self.tasks.iter_mut().enumerate() {
            if task.next_exec.as_ref().unwrap() <= &Utc::now().with_timezone(&self.timezone.clone())
            {
                debug!("Executing task {} ({}/{})", task.task_id, index + 1, total);
                task.run_task();
                if task.status == Status::Failed {
                    err_count += 1;
                }
                task.reschedule();
                if task.status == Status::Finished {
                    finished_ids.push(index);
                }
            }
        }

        // Clean if needed.
        for index in finished_ids {
            self.tasks.remove(index);
            warn!("Task {} has finished and is removed.", index);
        }

        if err_count > 0 {
            ExecutionStatus::HadError(err_count)
        } else {
            ExecutionStatus::Success
        }
    }

    /// Initialize all the tasks.
    pub(crate) fn init(&mut self) {
        debug!("Initializing {} task(s).", self.tasks.len());
        for (_index, task) in self.tasks.iter_mut().enumerate() {
            task.init(self.next_id);
            self.next_id += 1;
        }
    }

    /// Execute the `TaskGenerator` instance (if set).
    ///
    /// This function will add the generated
    fn run_task_gen(&mut self) {
        match self.task_gen {
            Some(ref mut tg) => {
                // Execute only if it's time to execute it.
                if tg.next_exec <= Utc::now().with_timezone(&self.timezone) {
                    match tg.run() {
                        Some(mut t) => {
                            t.init(self.next_id);
                            self.next_id += 1;
                            self.tasks.push(t);
                        }
                        None => { /* Do nothing */ }
                    }
                }
            }
            None => { /* Do nothing */ }
        }
    }

    /// Main execution loop.
    pub fn run(&mut self) {
        info!(
            "Scheduler started. Total tasks currently in queue: {}",
            self.tasks.len()
        );
        self.init();

        loop {
            self.run_task_gen();
            match self.execute_tasks() {
                ExecutionStatus::Success => { /* Do nothing */ }
                ExecutionStatus::HadError(e) => {
                    error!("Execution round had {} errors.", e);
                }
            }

            thread::sleep(Duration::from_millis(self.sleep as u64));
        }
    }
}

/// Module tests.
#[cfg(test)]
mod test {
    use super::*;
    use crate::TaskBuilder;
    use chrono::Local;
    use std::thread;
    use std::time::Duration;

    /// Test the normal functionality of a `TaskScheduler` instance.
    #[test]
    fn test_scheduler_normal_flow() {
        // Create a new scheduler instance.
        let mut scheduler = TaskScheduler::new(500, Local);
        // Add a couple of tasks.
        scheduler
            .add_task(Task::new("* * * * * * *", None, Some(2), Local))
            .add_task(Task::new("* * * * * * *", None, None, Local));
        assert_eq!(scheduler.tasks.len(), 2);
        // Initialize the tasks.
        scheduler.init();
        thread::sleep(Duration::from_millis(1000));
        scheduler.execute_tasks();
        assert_eq!(scheduler.tasks.len(), 2);
        thread::sleep(Duration::from_millis(1000));
        scheduler.execute_tasks();
        assert_eq!(scheduler.tasks.len(), 1);
    }

    /// Test the normal functionality of a `TaskScheduler` instance,
    /// in the case of task returning error status.
    #[test]
    fn test_scheduler_normal_flow_error_case() {
        // Create a new scheduler instance.
        let mut scheduler = TaskScheduler::new(500, Local);

        // Create a task.
        let mut task = Task::new("* * * * * * *", None, Some(1), Local);
        task.add_step(None, || Ok(()));
        // Return an error in the second step.
        task.add_step(None, || Err(()));

        // Add a task.
        scheduler.add_task(task);
        assert_eq!(scheduler.tasks.len(), 1);
        // Initialize the task.
        scheduler.init();
        thread::sleep(Duration::from_millis(1000));
        scheduler.execute_tasks();
        // The task should be removed after it's execution circle.
        assert_eq!(scheduler.tasks.len(), 0);
    }

    /// Test the normal functionality of a task generation function inside
    /// a task scheduler.
    #[test]
    fn test_scheduler_with_generator() {
        // Create a new scheduler instance.
        let mut scheduler = TaskScheduler::new(500, Local);

        // Add a task generator function that does now.
        scheduler.set_task_gen(TaskGenerator::new("* * * * * * *", Local, || None));

        // Should start with zero tasks.
        assert_eq!(scheduler.tasks.len(), 0);

        // Execute the task generator.
        thread::sleep(Duration::from_millis(1000));
        scheduler.run_task_gen();

        // The number of tasks should be zero again.
        assert_eq!(scheduler.tasks.len(), 0);

        // Update the generator to actually create a new task.
        scheduler.set_task_gen(TaskGenerator::new("* * * * * * *", Local, || {
            // Run at second "1" of every minute.

            // Create the task that will execute 2 total times.
            // Return the task for the execution queue.
            Some(TaskBuilder::new(Local).every("* * * * * * *").build())
        }));

        // Execute the task generator.
        thread::sleep(Duration::from_millis(1000));
        scheduler.run_task_gen();

        // The number of tasks should be zero again.
        assert_eq!(scheduler.tasks.len(), 1);
    }
}

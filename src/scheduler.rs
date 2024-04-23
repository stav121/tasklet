use crate::generator::TaskGenerator;
use crate::task::Status::Finished;
use crate::task::{run_task, Task, TaskCmd, TaskResponse};
use chrono::prelude::*;
use chrono::Utc;
use log::{debug, error, info};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Task execution possible statuses.
#[warn(dead_code)]
pub(crate) enum ExecutionStatus {
    Success,
    HadError(usize),
}

#[doc = r#"
Handler for task threads.
Contains the join handle and sender for each task.

When a task is finished the handle must be destroyed and sender dropped, in order to totally remove the task from the execution context.

The #id must be set uppon the task initialization in order to be easier to query for later use.
"#]
#[derive(Debug)]
pub struct TaskHandle {
    id: usize,
    handle: JoinHandle<()>,
    sender: mpsc::Sender<TaskCmd>,
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
}

/// `TaskScheduler` implementation.
impl<T> TaskScheduler<T>
where
    T: TimeZone + Clone + Send + 'static,
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
            /* Original empty, no registered tasks. */
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
    /// let mut generator = TaskGenerator::new("1 * * * * * *", chrono::Local, || None);
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
    /// scheduler.add_task(Task::new("* * * * * * *", None, None, chrono::Local));
    /// # });
    /// ```
    pub fn add_task(&mut self, mut task: Task<T>) -> &mut TaskScheduler<T> {
        let (sender, receiver) = mpsc::channel(32);

        task.set_receiver(receiver);
        task.set_id(self.next_id);
        let handle = tokio::spawn(run_task(task));

        // Push the handle
        self.handles.push(TaskHandle {
            id: self.next_id,
            handle,
            sender,
        });

        // Increase the id of the next task.
        self.next_id += 1;
        self
    }

    /// Execute all the tasks in the queue.
    /// After the execution the tasks are rescheduled and if needed,
    /// removed from the list.
    pub(crate) async fn execute_tasks(&mut self) -> ExecutionStatus {
        let mut recvrs: Vec<oneshot::Receiver<TaskResponse>> = Vec::new();

        for handle in &self.handles {
            let (send, recv) = oneshot::channel();

            let msg = TaskCmd::Run { sender: send };

            let _ = handle.sender.send(msg).await;
            recvrs.push(recv);
        }

        for recv in recvrs {
            let _ = recv.await;
        }

        let mut recvrs: Vec<oneshot::Receiver<TaskResponse>> = Vec::new();

        for handle in &self.handles {
            let (send, recv) = oneshot::channel();

            let msg = TaskCmd::Reschedule { sender: send };

            let _ = handle.sender.send(msg).await;
            recvrs.push(recv);
        }

        for recv in recvrs {
            let res = recv.await;
            let res1 = res.unwrap();
            if res1.status == Finished {
                for handle in &self.handles {
                    if handle.id == res1.id {
                        debug!("Killing task {} due to end of execution circle.", res1.id);
                        handle.handle.abort();
                    }
                }
                let index = self.handles.iter().position(|x| x.id == res1.id).unwrap();
                self.handles.remove(index);
            }
        }

        ExecutionStatus::Success

        // Ids of the tasks to be removed at the end of iterations.
        // let mut finished_ids: Vec<usize> = Vec::new();
        // let total = self.tasks.len();
        // let mut err_count: usize = 0;
        //
        // for (index, task) in self.tasks.iter_mut().enumerate() {
        //     if task.next_exec.as_ref().unwrap() <= &Utc::now().with_timezone(&self.timezone.clone())
        //     {
        //         debug!("Executing task {} ({}/{})", task.task_id, index + 1, total);
        //         task.run_task();
        //         if task.status == Status::Failed {
        //             err_count += 1;
        //         }
        //         task.reschedule();
        //         if task.status == Status::Finished {
        //             finished_ids.push(index);
        //         }
        //     }
        // }
        //
        // // Clean if needed.
        // for index in finished_ids {
        //     self.tasks.remove(index);
        //     warn!("Task {} has finished and is removed.", index);
        // }
        //
        // if err_count > 0 {
        //     ExecutionStatus::HadError(err_count)
        // } else {
        //     ExecutionStatus::Success
        // }
    }

    /// Initialize all the tasks.
    pub(crate) async fn init(&mut self) {
        debug!("Initializing {} task(s).", self.handles.len());

        let mut recvrs: Vec<oneshot::Receiver<TaskResponse>> = Vec::new();

        for handle in &self.handles {
            let (send, recv) = oneshot::channel();

            let msg = TaskCmd::Init { sender: send };
            let _ = handle.sender.send(msg).await;

            recvrs.push(recv);
        }

        for recv in recvrs {
            let _ = recv.await;
        }

        debug!(
            "Initialization finished for {} tasks(s).",
            self.handles.len()
        );
    }

    /// Execute the `TaskGenerator` instance (if set).
    ///
    /// This function will spawn the task, create its handle and attach it to the scheduler.
    fn run_task_gen(&mut self) -> bool {
        return match self.task_gen {
            Some(ref mut tg) => {
                // Execute only if it's time to execute it.
                if tg.next_exec <= Utc::now().with_timezone(&self.timezone) {
                    return match tg.run() {
                        Some(t) => {
                            self.add_task(t);
                            true
                        }
                        None => false,
                    };
                }
                false
            }
            None => false,
        };
    }

    /// Main execution loop.
    pub async fn run(&mut self) {
        info!(
            "Scheduler started. Total tasks currently in queue: {}",
            self.handles.len()
        );

        self.init().await;

        loop {
            if self.run_task_gen() {
                // Re-initialize the tasks if any new is added
                self.init().await;
            }
            match self.execute_tasks().await {
                ExecutionStatus::Success => {
                    // TODO: Check if at least one was executed or add new status NonExecuted
                    // info!("Execution round had no errors");
                }
                ExecutionStatus::HadError(e) => {
                    error!("Execution round had {} errors.", e);
                }
            }
            tokio::time::sleep(Duration::from_millis(self.sleep as u64)).await;
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
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
            .add_task(Task::new("* * * * * * *", None, None, Local));
        assert_eq!(scheduler.handles.len(), 2);
        // Initialize the tasks.
        scheduler.init().await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        scheduler.execute_tasks().await;
        assert_eq!(scheduler.handles.len(), 2);
        tokio::time::sleep(Duration::from_millis(1000)).await;
        scheduler.execute_tasks().await;
        assert_eq!(scheduler.handles.len(), 1);
    }

    #[tokio::test]
    async fn test_scheduler_normal_flow_error_case() {
        // Create a new scheduler instance.
        let mut scheduler = TaskScheduler::new(500, Local);

        // Create a task.
        let mut task = Task::new("* * * * * * *", None, Some(1), Local);
        task.add_step(None, || Ok(()));
        // Return an error in the second step.
        task.add_step(None, || Err(()));

        // Add a task.
        scheduler.add_task(task);
        assert_eq!(scheduler.handles.len(), 1);
        // Initialize the task.
        scheduler.init().await;
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
        scheduler.set_task_gen(TaskGenerator::new("* * * * * * *", Local, || None));

        // Should start with zero tasks.
        assert_eq!(scheduler.handles.len(), 0);

        // Execute the task generator.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        scheduler.run_task_gen();

        // The number of tasks should be zero again.
        assert_eq!(scheduler.handles.len(), 0);

        // Update the generator to actually create a new task.
        scheduler.set_task_gen(TaskGenerator::new("* * * * * * *", Local, || {
            // Run at second "1" of every minute.

            // Create the task that will execute 2 total times.
            // Return the task for the execution queue.
            Some(TaskBuilder::new(Local).every("* * * * * * *").build())
        }));

        // Execute the task generator.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        scheduler.run_task_gen();

        // The number of tasks should be zero again.
        assert_eq!(scheduler.handles.len(), 1);
    }
}

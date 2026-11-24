//! Events published by a running scheduler.
//!
//! Subscribe with [`SchedulerHandle::subscribe`](crate::SchedulerHandle::subscribe) to
//! react to task and run changes without polling the handle. Events are delivered over a
//! [`tokio::sync::broadcast`] channel with room for [`EVENT_CAPACITY`] undelivered events;
//! a subscriber that falls further behind receives
//! [`RecvError::Lagged`](tokio::sync::broadcast::error::RecvError::Lagged) and skips
//! ahead. Publishing costs almost nothing when nobody is subscribed.

use crate::task::{FinishReason, RunReport, StepState};
use chrono::{DateTime, Utc};

/// How many undelivered events each subscriber can fall behind before it starts
/// missing them.
pub const EVENT_CAPACITY: usize = 1024;

/// A change in the scheduler or in one of its tasks.
///
/// Every task-related variant carries the task's id and, if it has one, its name.
/// With the `serde` feature enabled, events implement `Serialize` and `Deserialize`, so
/// they can be forwarded as-is to an API or a UI (for example as Server-Sent Events).
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SchedulerEvent {
    /// The scheduler loop started ([`TaskScheduler::run`](crate::TaskScheduler::run) or
    /// [`run_until`](crate::TaskScheduler::run_until) was called).
    SchedulerStarted,
    /// The scheduler stopped: every task has been stopped and `run` or `run_until` is
    /// about to return. This is the last event of a run of the scheduler.
    SchedulerStopped,
    /// A task was registered with the scheduler.
    TaskAdded {
        /// The id assigned to the task.
        task_id: usize,
        /// The task's name, if set.
        task_name: Option<String>,
    },
    /// A task produced by a [`TaskGenerator`](crate::TaskGenerator) or a
    /// [`TaskSpawner`](crate::TaskSpawner) could not be added.
    TaskRejected {
        /// The task's name, if set.
        task_name: Option<String>,
        /// Why the task was rejected.
        reason: String,
    },
    /// A task left the scheduler. Sent once per task.
    TaskRemoved {
        /// The task's id.
        task_id: usize,
        /// The task's name, if set.
        task_name: Option<String>,
        /// Why the task left.
        reason: FinishReason,
    },
    /// A task was paused through the handle.
    TaskPaused {
        /// The task's id.
        task_id: usize,
        /// The task's name, if set.
        task_name: Option<String>,
    },
    /// A paused task was resumed through the handle.
    TaskResumed {
        /// The task's id.
        task_id: usize,
        /// The task's name, if set.
        task_name: Option<String>,
    },
    /// A run was requested through [`SchedulerHandle::trigger`](crate::SchedulerHandle::trigger).
    TaskTriggered {
        /// The task's id.
        task_id: usize,
        /// The task's name, if set.
        task_name: Option<String>,
    },
    /// A run started.
    RunStarted {
        /// The task's id.
        task_id: usize,
        /// The task's name, if set.
        task_name: Option<String>,
        /// The run's id.
        run_id: usize,
    },
    /// A scheduled run was skipped because it was dispatched later than the task's
    /// [`MissedRunPolicy`](crate::MissedRunPolicy) allows.
    RunSkipped {
        /// The task's id.
        task_id: usize,
        /// The task's name, if set.
        task_name: Option<String>,
        /// The time the skipped run was scheduled for (UTC).
        scheduled_for: DateTime<Utc>,
    },
    /// A step attempt started.
    StepStarted {
        /// The task's id.
        task_id: usize,
        /// The task's name, if set.
        task_name: Option<String>,
        /// The run's id.
        run_id: usize,
        /// The step's zero-based index.
        step_index: usize,
        /// The attempt number, starting at 1.
        attempt: u32,
    },
    /// A step that started reached its final state for the run: succeeded, had errors,
    /// failed after its last attempt, or was cancelled. Steps that never start
    /// (skipped after a failure, or cancelled before starting) get no event; their
    /// final state is in [`RunFinished`](SchedulerEvent::RunFinished).
    StepFinished {
        /// The task's id.
        task_id: usize,
        /// The task's name, if set.
        task_name: Option<String>,
        /// The run's id.
        run_id: usize,
        /// The step's final state.
        step: StepState,
    },
    /// A run finished. The report holds the run record and every step's final state.
    RunFinished {
        /// The finished run.
        report: RunReport,
    },
}

impl SchedulerEvent {
    /// The id of the task the event is about, if it is about a registered task.
    /// [`TaskRejected`](SchedulerEvent::TaskRejected) and the scheduler start and stop
    /// events have none.
    pub fn task_id(&self) -> Option<usize> {
        match self {
            SchedulerEvent::TaskAdded { task_id, .. }
            | SchedulerEvent::TaskRemoved { task_id, .. }
            | SchedulerEvent::TaskPaused { task_id, .. }
            | SchedulerEvent::TaskResumed { task_id, .. }
            | SchedulerEvent::TaskTriggered { task_id, .. }
            | SchedulerEvent::RunStarted { task_id, .. }
            | SchedulerEvent::RunSkipped { task_id, .. }
            | SchedulerEvent::StepStarted { task_id, .. }
            | SchedulerEvent::StepFinished { task_id, .. } => Some(*task_id),
            SchedulerEvent::RunFinished { report } => Some(report.task_id),
            SchedulerEvent::TaskRejected { .. }
            | SchedulerEvent::SchedulerStarted
            | SchedulerEvent::SchedulerStopped => None,
        }
    }
}

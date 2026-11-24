# Observability

A `SchedulerHandle` answers two kinds of questions: what the tasks look like now
(snapshots) and what has changed (events). This page also lists the types involved and
how to serialize them.

## Snapshots

The handle reads each task's shared state directly; no call waits for the scheduler
loop. Every method also works before `run()` is called, because tasks are registered
when `add_task` returns.

| Method | Returns |
|---|---|
| `task_count()` | Number of registered tasks |
| `statuses()` | A `TaskState` for every registered task |
| `state_of(id)` | `Option<TaskState>` |
| `status_of(id)`, `status_of_name(name)` | `Option<Status>` |
| `id_of_name(name)` | `Option<usize>` |
| `history(id)` | `Vec<RunRecord>`, oldest first |
| `step_states(id)` | `Vec<StepState>`, one per step |

```rust
use tasklet::TaskScheduler;

let scheduler = TaskScheduler::default(chrono::Utc);
let handle = scheduler.handle();

// From anywhere, for example a metrics endpoint:
println!("{} task(s) live", handle.task_count());
for state in handle.statuses() {
    println!(
        "task {} ({:?}) is {:?}, running: {}, waiting: {}, paused: {}, runs: {}, next: {:?}",
        state.id,
        state.name,
        state.status,
        state.running,
        state.waiting,
        state.paused,
        state.run_count,
        state.next_exec
    );
}
```

### Task state

| `TaskState` field | Meaning |
|---|---|
| `id` | Id assigned by the scheduler |
| `name` | Name, if set |
| `status` | Lifecycle `Status` (`Scheduled` once added) |
| `next_exec` | Next execution time in UTC, or `None` |
| `running` | A run is in progress |
| `waiting` | A run waits for a concurrency permit |
| `paused` | The task is paused |
| `last_outcome` | `RunOutcome` of the most recent finished run, kept even when history is disabled |
| `run_count` | Runs started over the task's lifetime |

### Run history

Each task keeps its most recent runs as `RunRecord`s: 20 by default, set with
`TaskBuilder::history_limit(n)`. Once `run()` returns, the handle has no registered
tasks and `history(id)` is empty; keep `FinishReport::last_run` or the `RunFinished`
events if you need the last run after that. A limit of 0 disables history; `run_count` and
`last_outcome` are still updated. A run in progress appears with `finished_at` and
`outcome` set to `None`.

| `RunRecord` field | Meaning |
|---|---|
| `run_id` | Per-task run counter, from 0 |
| `started_at`, `finished_at` | UTC times; `finished_at` is `None` while running |
| `outcome` | `Success`, `HadErrors`, `Failed`, `ForceRemoved` or `Cancelled`; `None` while running |
| `attempts` | Step attempts in the run, retries included |
| `error` | Reason of the failure that ended the run |
| `failed_step` | Index of the step that failed |

### Step states

`step_states(id)` changes live while a run is in progress: a finished early step shows
`Succeeded` while a later step is still `Pending`. Between runs it holds the final state
of the most recent run. Before the first run every step is `Pending`.

| `StepState` field | Meaning |
|---|---|
| `index` | Zero-based position |
| `description` | Text passed to `add_step`, if any |
| `status` | `Pending`, `Running`, `Succeeded`, `HadErrors`, `Failed`, `Skipped` or `Cancelled` |
| `attempts` | Attempts in this run, 0 if the step has not started |
| `error` | Reason of the step's last failed attempt in this run |

`Skipped` means an earlier step failed; `Cancelled` means the task was being stopped. See
[Control and shutdown](control-and-shutdown.md#cooperative-cancellation).

## Events

`handle.subscribe()` returns a `tokio::sync::broadcast::Receiver<SchedulerEvent>`. The
receiver gets every event published after the call; subscribe before `add_task` to see
the `TaskAdded` events.

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{SchedulerEvent, TaskBuilder, TaskScheduler};
use tokio::sync::broadcast::error::RecvError;

#[tokio::main]
async fn main() {
    let mut scheduler = TaskScheduler::default(chrono::Utc);
    let mut events = scheduler.handle().subscribe();

    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(SchedulerEvent::RunFinished { report }) => {
                    println!(
                        "task {} run {}: {:?}",
                        report.task_id, report.record.run_id, report.record.outcome
                    );
                }
                Ok(SchedulerEvent::TaskRejected { task_name, reason }) => {
                    eprintln!("rejected {task_name:?}: {reason}");
                }
                // The last event of a scheduler run.
                Ok(SchedulerEvent::SchedulerStopped) => break,
                Ok(_) => {}
                Err(RecvError::Lagged(missed)) => eprintln!("missed {missed} event(s)"),
                Err(RecvError::Closed) => break,
            }
        }
    });

    scheduler
        .add_task(
            TaskBuilder::new(chrono::Utc)
                .every_seconds(2)
                .add_step("work", |_ctx| async { Ok(Success) })
                .build(),
        )
        .unwrap();
    scheduler.run().await;
}
```

| Event | Published when |
|---|---|
| `SchedulerStarted` | `run()` or `run_until()` started |
| `SchedulerStopped` | Every task has stopped and `run()` or `run_until()` is about to return; the last event of a scheduler run |
| `TaskAdded { task_id, task_name }` | A task is registered |
| `TaskRejected { task_name, reason }` | A task from a `TaskGenerator` or `TaskSpawner` could not be added |
| `TaskPaused`, `TaskResumed`, `TaskTriggered` `{ task_id, task_name }` | The handle paused, resumed or triggered the task |
| `RunStarted { task_id, task_name, run_id }` | A run started (after any wait for a permit) |
| `RunSkipped { task_id, task_name, scheduled_for }` | The missed-run policy skipped a late run |
| `StepStarted { task_id, task_name, run_id, step_index, attempt }` | A step attempt started |
| `StepFinished { task_id, task_name, run_id, step }` | A step that started reached its final state for the run, including a step cancelled during a retry backoff |
| `RunFinished { report }` | A run finished; `report` is the same `RunReport` the callbacks receive |
| `TaskRemoved { task_id, task_name, reason }` | The task left the scheduler, with its `FinishReason`; published once per task. `FinishReason::Panicked` means the task's driver stopped unexpectedly, which only an internal bug can cause |

Order within a run: `RunStarted`, then `StepStarted` for each attempt and `StepFinished`
for each step that ran, then `RunFinished`. `on_success` or `on_failure` runs after
`RunFinished` is published.
Steps that never started (`Skipped`, or `Cancelled` before starting) get no step event;
their final state is in `report.steps`. A run aborted at the end of the grace period
publishes no `RunFinished`; its `TaskRemoved` still arrives.

`SchedulerEvent::task_id()` returns the event's task id, or `None` for `TaskRejected`,
`SchedulerStarted` and `SchedulerStopped`.

### Delivery

- Each subscriber can fall up to `tasklet::events::EVENT_CAPACITY` (1024) events behind.
  A subscriber that falls further behind gets `RecvError::Lagged(n)` with the number of
  events it missed, then continues with the oldest event still buffered.
- `recv()` returns `RecvError::Closed` once the scheduler, every handle and every task
  have been dropped.
- Publishing with no subscriber costs almost nothing, so there is no switch to turn
  events off.

## Serialization

With the `serde` feature, these types implement `Serialize` and `Deserialize`:
`TaskState`, `Status`, `StepState`, `StepStatus`, `RunRecord`, `RunOutcome`,
`RunReport`, `FinishReport`, `FinishReason` and `SchedulerEvent`. A control plane can
forward them as they are, for example as JSON over Server-Sent Events.

```toml
[dependencies]
tasklet = { version = "0.6.0", features = ["serde"] }
serde_json = "1"
```

```rust
use tasklet::TaskScheduler;

let scheduler = TaskScheduler::default(chrono::Utc);
let handle = scheduler.handle();
let body = serde_json::to_string(&handle.statuses()).unwrap();
println!("{body}");

let mut events = handle.subscribe();
while let Ok(event) = events.recv().await {
    println!("{}", serde_json::to_string(&event).unwrap());
}
```

## Matching on these types

`TaskState`, `StepState`, `RunRecord`, `RunReport`, `FinishReport`, `StepStatus`,
`RunOutcome`, `FinishReason`, `MissedRunPolicy` and `SchedulerEvent` are
`#[non_exhaustive]`. Read their fields freely, but add a wildcard arm (`_ => ..`) when
matching on the enums, and use `..` when destructuring the structs. Later releases can
then add fields and variants without breaking your code.

## Logging

The crate logs through the [`log`](https://docs.rs/log) facade; install a logger such as
`simple_logger` or `env_logger` to see the output. Failed steps log at `error`, retries,
timeouts and rejected tasks at `warn`, and scheduler start and stop at `info`. Per-run
detail is at `debug`.

## Runnable examples

- [`examples/events_and_cancellation.rs`](../examples/events_and_cancellation.rs): following a scheduler through its event stream.
- [`examples/control_and_observability.rs`](../examples/control_and_observability.rs): task states and run history.
- [`examples/schedule_helpers_and_streaming.rs`](../examples/schedule_helpers_and_streaming.rs): live step states during a run.

# Control and shutdown

A `SchedulerHandle` controls a scheduler from outside its loop: pause, resume, trigger
and remove tasks, and shut the scheduler down. This page also covers how a running step
learns that it should stop, and what the scheduler does when the step does not stop.

## Getting a handle

Call `scheduler.handle()` before `run()`, because `run()` borrows the scheduler mutably
until it returns. The handle is cheap to clone and `Send`, so it can go into other Tokio
tasks, signal handlers or HTTP handlers.

`TaskScheduler` is `Send` as well. To run the scheduler in the background, take the
handle first and move the scheduler into `tokio::spawn`:

```rust
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

#[tokio::main]
async fn main() {
    let mut scheduler = TaskScheduler::default(chrono::Utc);
    scheduler
        .add_task(
            TaskBuilder::new(chrono::Utc)
                .name("heartbeat")
                .every_seconds(1)
                .add_step("beat", |_ctx| async { Ok(Success) })
                .build(),
        )
        .unwrap();
    let handle = scheduler.handle();

    let running = tokio::spawn(async move { scheduler.run().await });

    tokio::time::sleep(Duration::from_secs(3)).await;
    if let Some(id) = handle.id_of_name("heartbeat") {
        println!("heartbeat ran {} time(s)", handle.state_of(id).unwrap().run_count);
    }
    handle.shutdown();
    running.await.unwrap();
}
```

## Names

`TaskBuilder::name` gives a task a name that is unique within the scheduler. `add_task`
rejects a second task with the same name with `TaskError::DuplicateTaskName`; so does
`TaskSpawner::spawn_get_id`, while `TaskSpawner::spawn` and a `TaskGenerator` publish a
`SchedulerEvent::TaskRejected`. A task without a name is addressed by the id returned
from `add_task_get_id` or `spawn_get_id`.

## Runtime control

Every control method exists by id and by name, and returns `true` if the task was found.

| By id | By name | Effect |
|---|---|---|
| `pause(id)` | `pause_name(name)` | Stop dispatching the task's schedule. A run in progress continues |
| `resume(id)` | `resume_name(name)` | Dispatch the schedule again. If the next execution time passed during the pause, the [missed-run policy](scheduling.md#late-runs) decides whether it runs now |
| `trigger(id)` | `trigger_name(name)` | Run the task now |
| `remove(id)` | `remove_name(name)` | Stop the task and remove it; see [Removing a task](#removing-a-task) |

`trigger` runs the task regardless of its schedule, of a pause, and of its
`MissedRunPolicy`. If a run is in progress or about to start, one more run is queued
after it (at most one). A triggered run waits for concurrency permits like any other run
and counts toward `repeat`. After it, the task continues with its next future slot.

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

#[tokio::main]
async fn main() {
    let mut scheduler = TaskScheduler::default(chrono::Local);
    scheduler
        .add_task(
            TaskBuilder::new(chrono::Local)
                .every("* * * * * * *")
                .name("report")
                .add_step("Step", |_ctx| async { Ok(Success) })
                .build(),
        )
        .unwrap();

    let handle = scheduler.handle();
    tokio::spawn(async move {
        handle.pause_name("report");
        handle.trigger_name("report"); // runs once now, although paused
        handle.resume_name("report");
        handle.remove_name("report");
        handle.shutdown();
    });

    scheduler.run().await;
}
```

## Cooperative cancellation

When a task is removed or the scheduler shuts down, the task is asked to stop. Steps see
the request through their `TaskContext`:

- `ctx.is_cancelled()` returns `true`.
- `ctx.cancelled().await` resolves. Use it in `tokio::select!` to cut a long wait short.

```rust
use std::time::Duration;
use tasklet::task::TaskStepStatusErr;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .add_step("long poll", |ctx| async move {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(60)) => Ok(Success),
            _ = ctx.cancelled() => Err(TaskStepStatusErr::error("stopped")),
        }
    })
    .add_step("batch", |ctx| async move {
        for _item in 0..1000 {
            if ctx.is_cancelled() {
                return Err(TaskStepStatusErr::error("stopped mid-batch"));
            }
            // process one item
        }
        Ok(Success)
    })
    .build();
```

What the scheduler records once the stop request arrives:

| Situation | Result |
|---|---|
| A step has not started yet | It does not start; its status is `StepStatus::Cancelled` |
| The running step returns `Err(Error(..))` | The step is `Cancelled`, it is not retried, and the run's outcome is `RunOutcome::Cancelled` |
| A step is waiting between retries | The wait ends early and the step is `Cancelled` |
| The running step returns `Ok(..)` | The step keeps its status. If later steps remain, they are `Cancelled` and the run's outcome is `Cancelled`. If it was the last step, every step completed, so the run counts as completed even though a stop was requested meanwhile, and `on_success` fires |
| The running step returns `Err(ErrorDelete(..))` | Handled as usual, even though the task is being stopped: the outcome is `ForceRemoved` and `on_failure` fires |
| The run is still going when the grace period ends | The task is aborted: the step's future is dropped, the run is recorded as `Cancelled`, and `on_finish` does not run |
| The run is waiting for a concurrency permit | The run does not start and leaves no record |

A `Cancelled` run fires neither `on_success` nor `on_failure`, and does not count toward
`repeat`. A task removed during its last repeat therefore reports `FinishReason::Removed`
(or `Shutdown`), not `Completed`.

## Grace period

`TaskScheduler::grace_period(duration)` sets how long tasks get to finish their current
run after removal or shutdown. The default is 5 seconds. `Duration::ZERO` aborts runs at
once; tasks stopped this way may not get to run `on_finish`.

```rust
use std::time::Duration;
use tasklet::TaskScheduler;

let mut scheduler = TaskScheduler::default(chrono::Utc);
scheduler.grace_period(Duration::from_secs(30));
```

## Removing a task

`remove(id)` and `remove_name(name)`:

1. ask the task to stop, as described above;
2. take the task out of the handle's view (`statuses()`, `task_count()`, the control
   methods) on the scheduler's next round, which the call triggers;
3. give a run in progress the grace period, then abort it;
4. fire `on_finish` with `FinishReason::Removed` (unless the task was aborted) and
   publish `SchedulerEvent::TaskRemoved`.

An idle task stops at once.

## Shutting down

There are two ways to stop the scheduler:

- `handle.shutdown()` makes `scheduler.run()` return. It can be called at any time; a
  request made before `run()` starts is remembered, and the loop exits right after it
  starts.
- `scheduler.run_until(future)` runs until the future resolves, for example a timer or
  an OS signal.

On shutdown the scheduler stops dispatching and asks every task to stop. Idle tasks stop
at once. Runs in progress get the grace period, measured from the start of the shutdown
(a task already being removed keeps its earlier deadline if that comes first), and are
aborted after it. Each task that stops fires `on_finish` with
`FinishReason::Shutdown` unless it was aborted (a task already being removed keeps
`FinishReason::Removed`), and each task publishes `SchedulerEvent::TaskRemoved`. When
`run()` returns, the handle has no registered tasks: `statuses()`, `history(id)` and
`step_states(id)` return empty vectors. Read a task's last run from
`FinishReport::last_run` in `on_finish`, or from `SchedulerEvent::RunFinished` events.
`TaskSpawner::spawn` and `spawn_get_id` return `TaskError::ExecutionError` from then on.

```rust
use tasklet::TaskScheduler;

#[tokio::main]
async fn main() {
    let mut scheduler = TaskScheduler::default(chrono::Utc);
    let handle = scheduler.handle();

    // Stop on Ctrl-C (needs tokio's "signal" feature).
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        handle.shutdown();
    });

    scheduler.run().await; // returns after the shutdown completes
}
```

The same with `run_until`:

```rust
use tasklet::TaskScheduler;

#[tokio::main]
async fn main() {
    let mut scheduler = TaskScheduler::default(chrono::Utc);
    scheduler
        .run_until(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await;
}
```

## Runnable examples

- [`examples/spawned_scheduler.rs`](../examples/spawned_scheduler.rs): a scheduler in a background Tokio task, driven through its handle.
- [`examples/events_and_cancellation.rs`](../examples/events_and_cancellation.rs): a long step that stops cooperatively on shutdown.
- [`examples/control_and_observability.rs`](../examples/control_and_observability.rs): named tasks, pause, trigger and inspection.

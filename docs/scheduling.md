# Scheduling and concurrency

This page covers when tasks run: schedule expressions, how the scheduler loop wakes up,
what happens when runs overlap or start late, how to limit concurrent runs, and the ways
to add tasks.

## Schedules

`TaskBuilder::every` takes a cron expression in the format of the
[`cron`](https://docs.rs/cron) crate. Fields start with seconds: `sec min hour
day-of-month month day-of-week`, plus an optional year. `"1 * * * * * *"` runs at second 1
of every minute. An invalid expression makes `build()` return
`TaskError::InvalidCronExpression`.

Helpers build common expressions for you:

| Helper | Expression | Valid range |
|---|---|---|
| `every_seconds(n)` | `*/n * * * * * *` | 1 to 59 |
| `every_minutes(n)` | `0 */n * * * * *` | 1 to 59 |
| `every_hours(n)` | `0 0 */n * * * *` | 1 to 23 |
| `hourly_at(minute)` | `0 minute * * * * *` | 0 to 59 |
| `daily_at(hour, minute)` | `0 minute hour * * * *` | hour 0 to 23, minute 0 to 59 |

A value outside the range produces an invalid schedule that `build()` rejects, like an
invalid cron string.

```rust
use tasklet::TaskBuilder;

let _ = TaskBuilder::new(chrono::Local).every_seconds(5);  // "*/5 * * * * * *"
let _ = TaskBuilder::new(chrono::Local).every_minutes(15); // "0 */15 * * * * *"
let _ = TaskBuilder::new(chrono::Local).every_hours(6);    // "0 0 */6 * * * *"
let _ = TaskBuilder::new(chrono::Local).hourly_at(15);     // minute 15 of every hour
let _ = TaskBuilder::new(chrono::Local).daily_at(9, 30);   // every day at 09:30
```

Schedules are evaluated in the timezone passed to `TaskBuilder::new`. The scheduler and
its tasks share one timezone type, for example `TaskScheduler<chrono::Utc>` only accepts
`Task<chrono::Utc>`. Times reported through the handle (`TaskState::next_exec`,
`RunRecord::started_at`, ...) are in UTC.

A task whose schedule has no future slot (for example a fixed year in the past) leaves
the scheduler right away with `FinishReason::Completed`.

## How the scheduler loop runs

`TaskScheduler::run()` sleeps until the next task is due, dispatches every due task, and
sleeps again. It wakes early when a task is added, spawned, resumed or removed, or when a
run finishes, so runs start on their slot.

Dispatch does not wait for the run. Each task runs in its own Tokio task, so a slow task
never delays another task's schedule.

`TaskScheduler::default(tz)` and `TaskScheduler::new(sleep_ms, tz)` differ only in the
maximum idle wait: the longest the loop sleeps without checking the clock. It does not
delay runs; it bounds how late a run can start after the system clock jumps forward.
`default` uses 1000 ms.

```rust
use tasklet::TaskScheduler;

// Check the clock at least every 500 ms.
let _scheduler = TaskScheduler::new(500, chrono::Utc);
```

## Overlapping runs

A task never runs twice at the same time. When a slot comes due while the previous run is
still in progress, the task's `OverlapPolicy` decides:

| Policy | Effect |
|---|---|
| `OverlapPolicy::Skip` (default) | The slot is dropped; the task continues with its next future slot |
| `OverlapPolicy::Queue` | One more run starts as soon as the current one finishes, however many slots passed |

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{OverlapPolicy, TaskBuilder};

let _task = TaskBuilder::new(chrono::Local)
    .every("* * * * * * *")
    .overlap(OverlapPolicy::Queue)
    .add_step("Step", |_ctx| async { Ok(Success) })
    .build();
```

A run requested with `SchedulerHandle::trigger` while a run is in progress is queued the
same way, whatever the policy.

## Late runs

A scheduled run can start late: the task was paused when its slot came due, or the
process was busy or suspended. The task's `MissedRunPolicy` decides what happens:

| Policy | Effect |
|---|---|
| `MissedRunPolicy::RunOnce` (default) | Run once as soon as possible, however late, then continue with the next future slot. Several missed slots still produce one run |
| `MissedRunPolicy::Skip { grace }` | Skip the run if it starts more than `grace` after its slot, and continue with the next future slot. A `grace` below 1 second is treated as 1 second |

```rust
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{MissedRunPolicy, TaskBuilder};

// Drop any run that would start more than 30 seconds after its slot.
let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .missed_runs(MissedRunPolicy::Skip { grace: Duration::from_secs(30) })
    .add_step("report", |_ctx| async { Ok(Success) })
    .build();
```

Rules:

- The policy applies only to runs the scheduler dispatches for a slot. Runs requested
  with `trigger` and runs queued by `OverlapPolicy::Queue` always execute.
- A paused task's next execution time does not move. When the task is resumed after that
  time, the policy decides whether it runs straight away.
- The check happens when the run is dispatched. If the run then has to wait for a
  concurrency permit, the check is repeated once the permit is granted, so a long wait
  can make the run skip.
- A skipped run publishes `SchedulerEvent::RunSkipped` and leaves no `RunRecord`.

## Concurrency limits

By default every due task runs at once. Two limits cap that:

- `TaskScheduler::max_concurrent_runs(n)` caps the runs in progress across the whole
  scheduler.
- `TaskScheduler::add_pool(name, n)` creates a named pool, and `TaskBuilder::pool(name)`
  puts a task in it. At most `n` runs of tasks in the pool execute at the same time. A
  pool run also counts against `max_concurrent_runs`.

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

let mut scheduler = TaskScheduler::default(chrono::Utc);
scheduler.max_concurrent_runs(8).add_pool("database", 2);

for table in ["users", "orders", "invoices"] {
    scheduler
        .add_task(
            TaskBuilder::new(chrono::Utc)
                .name(&format!("vacuum-{table}"))
                .every_hours(1)
                .pool("database")
                .add_step("vacuum", |_ctx| async { Ok(Success) })
                .build(),
        )
        .unwrap();
}
```

Details:

- A run over a limit waits for a permit. While it waits, `TaskState::waiting` is `true`
  and `TaskState::running` is `false`. The `RunStarted` event and the `RunRecord` are
  created once the permit is granted.
- The pool must exist before a task that names it is added. Otherwise `add_task`,
  `add_task_get_id` and `TaskSpawner::spawn_get_id` return `TaskError::UnknownPool`;
  tasks from `TaskSpawner::spawn` and `TaskGenerator` are rejected with a
  `SchedulerEvent::TaskRejected`.
- A limit below 1 is treated as 1.
- Calling `max_concurrent_runs` or `add_pool` again replaces that limit. It applies from
  each task's next run; runs that already hold a permit from the old limit keep it until
  they finish.
- A run releases its permits when its steps finish, before `on_success` or `on_failure`
  runs.
- A run that is still waiting for a permit when the task is removed or the scheduler
  shuts down does not start.

## Adding tasks

### Before the scheduler runs

`add_task` returns `Result<&mut TaskScheduler<T>, TaskError>`; `add_task_get_id` returns
the id the scheduler assigned. Ids count from 0 in the order tasks are added. Both work
outside a Tokio runtime, in which case the tasks start when the scheduler runs. The task
is visible through every `SchedulerHandle`, with its first execution time computed, as
soon as the call returns. For a task added outside a runtime, that first execution time
is computed again when the scheduler starts, so a long gap between adding and running
does not make it late.

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

let mut scheduler = TaskScheduler::default(chrono::Local);
let id = scheduler
    .add_task_get_id(
        TaskBuilder::new(chrono::Local)
            .every_seconds(5)
            .add_step("tick", |_ctx| async { Ok(Success) })
            .build(),
    )
    .unwrap();
let handle = scheduler.handle();
assert!(handle.state_of(id).unwrap().next_exec.is_some());
```

### While the scheduler runs

`SchedulerHandle` has no timezone type, so it cannot carry a `Task<T>`. Take a
`TaskSpawner` with `scheduler.spawner()` before calling `run()`. It is cloneable and
`Send`. `spawn` queues a task and returns at once; `spawn_get_id` waits for the assigned
id or the rejection error. Spawned tasks are added when the scheduler loop wakes, which
the spawn triggers, so they are only processed while the scheduler runs.

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

#[tokio::main]
async fn main() {
    let mut scheduler = TaskScheduler::default(chrono::Local);
    let spawner = scheduler.spawner();
    let handle = scheduler.handle();

    tokio::spawn(async move {
        let task = TaskBuilder::new(chrono::Local)
            .every("* * * * * * *")
            .repeat(3)
            .add_step("Step", |_ctx| async { Ok(Success) })
            .build();
        match spawner.spawn_get_id(task).await {
            Ok(id) => println!("added task {id}"),
            Err(error) => eprintln!("rejected: {error}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        handle.shutdown();
    });

    scheduler.run().await;
}
```

### From a generator

A `TaskGenerator` runs a discovery closure on its own cron schedule. Whenever the closure
returns `Some(task)`, the scheduler adds it. The closure runs on the scheduler loop, so it
must return quickly and must be `Send`. For discovery that needs async work, use a
`TaskSpawner` from a separate Tokio task instead. A generated task that cannot be added
is logged at `warn` level and published as `SchedulerEvent::TaskRejected`.

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskGenerator, TaskScheduler};

let mut scheduler = TaskScheduler::default(chrono::Utc);
let generator = TaskGenerator::new("0 * * * * * *", chrono::Utc, || {
    Some(
        TaskBuilder::new(chrono::Utc)
            .every_seconds(10)
            .repeat(1)
            .add_step("generated", |_ctx| async { Ok(Success) })
            .build(),
    )
})
.unwrap();
scheduler.set_task_gen(generator);
```

`TaskGenerator::new` returns `TaskError::InvalidCronExpression` for an invalid
expression or one with no upcoming slot.

## Runnable examples

- [`examples/schedule_helpers_and_streaming.rs`](../examples/schedule_helpers_and_streaming.rs): schedule helpers and `add_task_get_id`.
- [`examples/overlap_and_status_example.rs`](../examples/overlap_and_status_example.rs): `OverlapPolicy::Skip` with a slow task.
- [`examples/concurrency_limits.rs`](../examples/concurrency_limits.rs): `max_concurrent_runs` and pools.
- [`examples/generator_example.rs`](../examples/generator_example.rs) and [`examples/context_and_spawner.rs`](../examples/context_and_spawner.rs): adding tasks with a generator and a spawner.

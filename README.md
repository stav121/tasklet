<p align="center">
    <img src="tasklet-logo.png">
</p>

[![CircleCI](https://img.shields.io/circleci/build/github/stav121/tasklet?style=for-the-badge&logo=circleci)](https://circleci.com/gh/stav121/tasklet)
![Crates.io](https://img.shields.io/crates/d/tasklet?style=for-the-badge&color=blue&logo=owncloud)
![Crates.io](https://img.shields.io/crates/v/tasklet?style=for-the-badge&color=orange&logo=rust)
![GitHub last commit](https://img.shields.io/github/last-commit/stav121/tasklet?style=for-the-badge&color=purple&logo=git&logoColor=white)
[![Codecov](https://img.shields.io/codecov/c/github/stav121/tasklet?style=for-the-badge&logo=codecov&logoColor=white)](https://codecov.io/gh/stav121/tasklet)
[![License](https://img.shields.io/github/license/stav121/tasklet?style=for-the-badge&color=lightgrey&logo=amazoniam&logoColor=white)](https://github.com/stav121/tasklet/blob/main/LICENSE)
[![GitHub issues](https://img.shields.io/github/issues/stav121/tasklet?style=for-the-badge&color=yellow&logo=github)](https://github.com/stav121/tasklet/issues)

An asynchronous task scheduling library written in Rust

## About

`tasklet` is a task scheduling library written in Rust. It is built over `tokio` runtime and utilizes green threads
in order to run tasks asynchronously.

## Dependencies

| library   | version |
|-----------|---------|
| cron      | 0.15.0  |
| chrono    | 0.4.42  |
| log       | 0.4.29  |
| tokio     | 1.48.0  |
| thiserror | 2.0.17  |
| fastrand  | 2.3     |

## How to use this library

In your `Cargo.toml` add:

```
[dependencies]
tasklet = "0.4.1"
```

To derive `serde` on the observable state types (`TaskState`, `Status`, `RunRecord`,
`StepState`, ...) for a control plane or API, enable the optional `serde` feature:

```
[dependencies]
tasklet = { version = "0.4.1", features = ["serde"] }
```

> **Upgrading from 0.2.x?** See the [migration notes](#migrating-from-02x-to-030) below —
> task steps are now `async`.

## Example

Find more examples in the [examples](/examples) folder.

Task steps are **asynchronous**: every step is a closure that returns a future, so it can
`.await` real work (I/O, timers, network calls) without blocking the runtime.

```rust
use log::info;
use simple_logger::SimpleLogger;
use tasklet::task::TaskStepStatusErr::Error;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

/// A simple example of a task with two steps,
/// that might work or fail sometimes.
#[tokio::main]
async fn main() {
    // Init the logger.
    SimpleLogger::new().init().unwrap();

    // A variable to be passed in the task.
    let mut exec_count = 0;

    // Task scheduler with 1000ms loop frequency.
    let mut scheduler = TaskScheduler::default(chrono::Local);

    // Create a task with 2 steps and add it to the scheduler.
    // The second step fails every second execution.
    // Append the task to the scheduler.
    let _ = scheduler.add_task(
        TaskBuilder::new(chrono::Local)
            .every("1 * * * * * *")
            .description("A simple task")
            .add_step("Step 1", || async {
                info!("Hello from step 1");
                Ok(Success) // Let the scheduler know this step was a success.
            })
            .add_step("Step 2", move || {
                // Snapshot per-run state, then move it into the async block so the
                // returned future is `'static`.
                let count = exec_count;
                exec_count += 1;
                async move {
                    if count % 2 == 0 {
                        Err(Error) // Indicate that this step was a fail.
                    } else {
                        info!("Hello from step 2");
                        Ok(Success) // Indicate that this step was a success.
                    }
                }
            })
            .build(),
    );

    // Execute the scheduler.
    scheduler.run().await;
}
```

## Graceful shutdown

`scheduler.run()` normally loops forever. To stop it cleanly, grab a
`SchedulerHandle` with `scheduler.handle()` *before* running and call `shutdown()`
from anywhere — the current round finishes, the tasks are drained and `run()` returns:

```rust,no_run
# use tasklet::TaskScheduler;
# #[tokio::main]
# async fn main() {
let mut scheduler = TaskScheduler::default(chrono::Utc);
let handle = scheduler.handle();

// Stop on Ctrl-C.
tokio::spawn(async move {
    tokio::signal::ctrl_c().await.ok();
    handle.shutdown();
});

scheduler.run().await; // returns once shutdown is requested
# }
```

You can also drive the shutdown with any future via `scheduler.run_until(future)` — for
example a timer or an OS signal.

## Timeouts, retries and lifecycle callbacks

Tasks can be made resilient with a few optional builder settings (all non-breaking,
added in 0.3.1):

```rust,no_run
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{RetryPolicy, TaskBuilder};

let _task = TaskBuilder::new(chrono::Local)
    .every("1 * * * * * *")
    // Cancel any single step attempt that runs longer than this.
    .timeout(Duration::from_secs(5))
    // Retry failing steps with exponential backoff (100ms, 200ms, 400ms), capped at 2s.
    .retry(RetryPolicy::exponential(3, Duration::from_millis(100), 2)
        .with_max_delay(Duration::from_secs(2)))
    // Async lifecycle hooks.
    .on_success(|| async { println!("run succeeded"); })
    .on_failure(|| async { eprintln!("run failed"); })
    .on_finish(|| async { println!("task finished its lifecycle"); })
    .add_step("Step", || async { Ok(Success) })
    .build();
```

- **Timeout** (`.timeout`) bounds each individual step attempt; a step that exceeds it is
  cancelled and treated as a (retryable) failure.
- **Retry** (`.retry`) re-attempts a step that returns `TaskStepStatusErr::Error` (or times
  out). Use `RetryPolicy::fixed` or `RetryPolicy::exponential`, optionally with
  `.with_jitter(Jitter::Full)` to spread retries out and avoid a thundering herd. A step
  returning `TaskStepStatusErr::ErrorDelete` bypasses retries and removes the task immediately.
- **Callbacks** (`.on_success` / `.on_failure` / `.on_finish`) are async hooks;
  `on_finish` fires once when the task reaches a terminal state.

See [`examples/retry_timeout_example.rs`](/examples/retry_timeout_example.rs) for a runnable demo.

## Overlap policy and non-blocking scheduling

Tasks run independently: a task whose step takes longer than its interval never delays
any other task's schedule. When a task's next scheduled time arrives while its previous
run is still in progress, the `OverlapPolicy` decides what happens (added in 0.3.2):

- **`Skip`** (default): drop that occurrence and resume on the next future slot.
- **`Queue`**: run the missed occurrence once the current run finishes.

```rust,no_run
# use tasklet::{OverlapPolicy, TaskBuilder};
# use tasklet::task::TaskStepStatusOk::Success;
let _task = TaskBuilder::new(chrono::Local)
    .every("* * * * * * *")
    .overlap(OverlapPolicy::Queue)
    .add_step("Step", || async { Ok(Success) })
    .build();
```

## Observing the scheduler

Grab a `SchedulerHandle` with `scheduler.handle()` and query the live task set at
runtime (added in 0.3.2):

```rust,no_run
# use tasklet::TaskScheduler;
# #[tokio::main]
# async fn main() {
let scheduler = TaskScheduler::default(chrono::Utc);
let handle = scheduler.handle();

// From anywhere, e.g. a metrics endpoint:
println!("{} task(s) live", handle.task_count());
for state in handle.statuses() {
    println!("task {} is {:?} (running: {})", state.id, state.status, state.running);
}
# }
```

See [`examples/overlap_and_status_example.rs`](/examples/overlap_and_status_example.rs) for a runnable demo.

Since 0.4.1, `handle.step_states(id)` reflects step transitions *live* during a run, not
only after it finishes: a completed early step shows as `Succeeded` while a later step is
still `Pending`.

## Schedule helpers

Since 0.4.1 you can express common cadences without writing cron by hand. Each helper is a
thin wrapper over `every(...)`:

```rust,no_run
# use tasklet::TaskBuilder;
let _ = TaskBuilder::new(chrono::Local).every_seconds(5);   // "*/5 * * * * * *"
let _ = TaskBuilder::new(chrono::Local).every_minutes(15);  // "0 */15 * * * * *"
let _ = TaskBuilder::new(chrono::Local).every_hours(6);     // "0 0 */6 * * * *"
let _ = TaskBuilder::new(chrono::Local).hourly_at(15);      // minute 15 of every hour
let _ = TaskBuilder::new(chrono::Local).daily_at(9, 30);    // every day at 09:30
```

Out-of-range values (e.g. `every_seconds(60)`) produce an invalid schedule that is
rejected by `build()`, just like an invalid cron string.

You can also capture the id the scheduler assigns to a task with `add_task_get_id`, which
is handy for addressing an unnamed task through the handle afterwards:

```rust,no_run
# use tasklet::{TaskBuilder, TaskScheduler};
# #[tokio::main]
# async fn main() {
let mut scheduler = TaskScheduler::default(chrono::Local);
let id = scheduler
    .add_task_get_id(TaskBuilder::new(chrono::Local).every_seconds(5).build())
    .unwrap();
let handle = scheduler.handle();
tokio::spawn(async move { handle.trigger(id); });
# }
```

## Naming, runtime control and history

Give a task a stable name and address it at runtime through the handle (added in 0.4.0).
Names are unique within a scheduler; `add_task` rejects a duplicate with
`TaskError::DuplicateTaskName`.

```rust,no_run
# use tasklet::{TaskBuilder, TaskScheduler};
# use tasklet::task::TaskStepStatusOk::Success;
# #[tokio::main]
# async fn main() {
let mut scheduler = TaskScheduler::default(chrono::Local);
scheduler
    .add_task(
        TaskBuilder::new(chrono::Local)
            .every("* * * * * * *")
            .name("report")
            .add_step("Step", || async { Ok(Success) })
            .build(),
    )
    .unwrap();

let handle = scheduler.handle();
tokio::spawn(async move {
    // Control a task by name (or by id) while the scheduler runs:
    handle.pause_name("report");
    handle.trigger_name("report"); // run once now, off-schedule
    handle.resume_name("report");

    // Observe what it did:
    if let Some(id) = handle.id_of_name("report") {
        let runs = handle.history(id);            // recent RunRecords
        let steps = handle.step_states(id);       // per-step outcomes
        println!("{} run(s), {} step(s)", runs.len(), steps.len());
    }
    handle.remove_name("report"); // reaped on the next round
});

scheduler.run().await;
# }
```

Every `TaskState` from `handle.statuses()` now also carries the task's `name`, whether it
is `paused`, its `run_count` and its `last_outcome`.

## Sharing data between tasks

A `Blackboard` is a cheaply-clonable, typed key/value store. Clone it into as many
task/step closures as you like; they all read and write the same storage, replacing the
ad-hoc `Arc<Mutex<HashMap<...>>>` boilerplate.

```rust
use tasklet::Blackboard;

let board = Blackboard::new();
board.set("attempts", 0u32);

let shared = board.clone();
shared.set("attempts", 3u32);

assert_eq!(board.get::<u32>("attempts"), Some(3));
```

See [`examples/control_and_observability.rs`](/examples/control_and_observability.rs) for a
runnable demo combining names, control, history and a shared blackboard.

## Migrating from 0.2.x to 0.3.0

- **Steps are now async.** A step closure must return a future. Wrap synchronous bodies
  in an `async` block:

  ```rust,ignore
  // 0.2.x
  .add_step("Step", || Ok(Success))
  // 0.3.0
  .add_step("Step", || async { Ok(Success) })
  ```

  For state that changes between runs, snapshot it in the (`FnMut`) closure and `move` the
  snapshot into the `async move` block so the future stays `'static`.
- **`TaskGenerator::new` now returns a `Result`.** It no longer panics on an invalid cron
  expression — call `?`/`.unwrap()` on the result.
- **`TaskScheduler::run` can now stop.** It returns when a shutdown is requested through a
  `SchedulerHandle`; if you never request one, behaviour is unchanged (runs forever).

## Author

Stavros Grigoriou ([stav121](github.com/stav121))

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

An asynchronous, cron-style task scheduler for Tokio.

## About

`tasklet` runs scheduled tasks inside a Tokio runtime. A task is a cron schedule plus a
list of async steps; each task runs in its own Tokio task, so a slow task never delays
the others. Tasks can retry, time out, report what happened through callbacks and
events, and be paused, triggered or removed while the scheduler runs.

> **Upgrading from 0.5.x?** 0.6.0 has breaking changes. Follow the
> [migration guide](docs/MIGRATION.md).

## Installation

In your `Cargo.toml` add:

```toml
[dependencies]
tasklet = "0.6.0"
```

To derive `serde` on the observable state types and events (`TaskState`, `RunRecord`,
`StepState`, `SchedulerEvent`, ...) for a control plane or API, enable the optional
`serde` feature:

```toml
[dependencies]
tasklet = { version = "0.6.0", features = ["serde"] }
```

`tasklet` needs a Tokio runtime but enables only the Tokio features it uses itself. The
examples below also need Tokio's `macros` and `rt-multi-thread` features
(`signal` for Ctrl-C handling), and `chrono` for the timezone:

```toml
[dependencies]
chrono = "0.4"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The minimum supported Rust version is set by `rust-version` in `Cargo.toml`.

### Dependencies

| library   | version  |
|-----------|----------|
| cron      | 0.15.0   |
| chrono    | 0.4.42   |
| log       | 0.4.29   |
| tokio     | 1.48.0   |
| thiserror | 2.0.17   |
| fastrand  | 2.3      |
| serde     | 1.0 (optional, `serde` feature) |

## Example

A task with two steps that runs at second 1 of every minute. The second step fails on
every other run. More programs are in the [examples](examples) folder.

```rust
use log::info;
use simple_logger::SimpleLogger;
use tasklet::task::TaskStepStatusErr::Error;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

#[tokio::main]
async fn main() {
    SimpleLogger::new().init().unwrap();

    let mut exec_count = 0;
    let mut scheduler = TaskScheduler::default(chrono::Local);

    scheduler
        .add_task(
            TaskBuilder::new(chrono::Local)
                .every("1 * * * * * *")
                .description("A simple task")
                .add_step("Step 1", |_ctx| async {
                    info!("Hello from step 1");
                    Ok(Success)
                })
                .add_step("Step 2", move |_ctx| {
                    // Snapshot the counter for this run, then advance it. The async block
                    // owns the snapshot, so the returned future is `'static`.
                    let count = exec_count;
                    exec_count += 1;
                    async move {
                        if count % 2 == 0 {
                            Err(Error("even run".into()))
                        } else {
                            info!("Hello from step 2");
                            Ok(Success)
                        }
                    }
                })
                .build(),
        )
        .unwrap();

    // Runs until a shutdown is requested through a `SchedulerHandle`.
    scheduler.run().await;
}
```

## Features

### Async steps with context

Every step is a closure that returns a future, so it can `.await` I/O and timers. It
receives a `TaskContext` with the task id and name, the run id, the step index, the
attempt number, a task-level `Blackboard` shared across runs and a per-run store for
passing values between steps. Details: [Tasks and steps](docs/tasks-and-steps.md).

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every_seconds(30)
    .add_step("produce", |ctx| async move {
        ctx.run_store().set("rows", 42u32);
        Ok(Success)
    })
    .add_step("consume", |ctx| async move {
        let rows = ctx.run_store().get::<u32>("rows").unwrap_or(0);
        println!("run {} attempt {}: {} rows", ctx.run_id(), ctx.attempt(), rows);
        Ok(Success)
    })
    .build();
```

### Failures, retries, timeouts and callbacks

A step fails with `TaskStepStatusErr::error(reason)` (retryable) or
`TaskStepStatusErr::error_delete(reason)` (removes the task). Timeouts apply to each step
attempt; a `RetryPolicy` retries failed attempts with fixed or exponential backoff and
optional jitter. Callbacks receive a report of the run or of why the task left.

```rust
use std::time::Duration;
use tasklet::task::TaskStepStatusErr;
use tasklet::{RetryPolicy, TaskBuilder};

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(1)
    .timeout(Duration::from_secs(5))
    .retry(RetryPolicy::exponential(3, Duration::from_millis(100), 2))
    .add_step("call API", |_ctx| async {
        Err(TaskStepStatusErr::error("upstream returned 503"))
    })
    .on_success(|report| async move { println!("run {} ok", report.record.run_id) })
    .on_failure(|report| async move { eprintln!("failed: {:?}", report.record.error) })
    .on_finish(|report| async move { println!("task left: {:?}", report.reason) })
    .build();
```

### Scheduling

Use a cron expression with `every(..)`, or a helper: `every_seconds`, `every_minutes`,
`every_hours`, `hourly_at` and `daily_at`. The scheduler sleeps until the next task is
due, so runs start on their slot. `OverlapPolicy` decides what happens when a slot comes
due while the previous run is still going, and `MissedRunPolicy` what happens to a run
that starts late. Details: [Scheduling and concurrency](docs/scheduling.md).

```rust
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{MissedRunPolicy, OverlapPolicy, TaskBuilder};

let _task = TaskBuilder::new(chrono::Local)
    .daily_at(9, 30)
    .overlap(OverlapPolicy::Queue)
    .missed_runs(MissedRunPolicy::Skip { grace: Duration::from_secs(60) })
    .add_step("report", |_ctx| async { Ok(Success) })
    .build();
```

### Concurrency limits

Cap the runs in progress across the scheduler, or per named pool.

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

let mut scheduler = TaskScheduler::default(chrono::Utc);
scheduler.max_concurrent_runs(8).add_pool("database", 2);
scheduler
    .add_task(
        TaskBuilder::new(chrono::Utc)
            .every_minutes(5)
            .pool("database")
            .add_step("vacuum", |_ctx| async { Ok(Success) })
            .build(),
    )
    .unwrap();
```

### Runtime control and graceful shutdown

A `SchedulerHandle` pauses, resumes, triggers and removes tasks by id or name, and shuts
the scheduler down. Removal and shutdown are cooperative: steps see the request through
`ctx.is_cancelled()` or `ctx.cancelled().await`, runs in progress get a grace period
(5 seconds by default, set with `TaskScheduler::grace_period`) and are aborted after it.
Details: [Control and shutdown](docs/control-and-shutdown.md).

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler};

#[tokio::main]
async fn main() {
    let mut scheduler = TaskScheduler::default(chrono::Utc);
    scheduler
        .add_task(
            TaskBuilder::new(chrono::Utc)
                .name("report")
                .every_minutes(1)
                .add_step("Step", |_ctx| async { Ok(Success) })
                .build(),
        )
        .unwrap();

    let handle = scheduler.handle();
    tokio::spawn(async move {
        handle.trigger_name("report"); // run once now, off-schedule
        tokio::signal::ctrl_c().await.ok();
        handle.shutdown();
    });

    scheduler.run().await; // returns once the shutdown has completed
}
```

`TaskScheduler` is `Send`, so it can also run in a spawned Tokio task. Use
`scheduler.run_until(future)` to stop on any future, such as a timer or an OS signal.

### Observability

The handle reports each task's state, its recent run history and the live state of its
steps. `handle.subscribe()` returns a broadcast receiver of `SchedulerEvent`s (task
added, run started, step finished, run finished, task removed, ...). Details:
[Observability](docs/observability.md).

```rust
use tasklet::{SchedulerEvent, TaskScheduler};

let scheduler = TaskScheduler::default(chrono::Utc);
let handle = scheduler.handle();

for state in handle.statuses() {
    println!("task {} is {:?} (running: {})", state.id, state.status, state.running);
}

let mut events = handle.subscribe();
tokio::spawn(async move {
    while let Ok(event) = events.recv().await {
        if let SchedulerEvent::RunFinished { report } = event {
            println!("task {} finished run {}", report.task_id, report.record.run_id);
        }
    }
});
```

### Adding tasks at runtime

`scheduler.spawner()` returns a cloneable, `Send` `TaskSpawner` that adds fully built
tasks to a running scheduler. A `TaskGenerator` adds tasks from a discovery closure on
its own schedule. See [Adding tasks](docs/scheduling.md#adding-tasks).

## Documentation

- [Guides](docs/README.md): tasks and steps, scheduling and concurrency, control and
  shutdown, observability.
- [Migration guide](docs/MIGRATION.md): code changes for 0.5.x to 0.6.0 and earlier
  breaking releases.
- [API reference](https://docs.rs/tasklet) on docs.rs.
- [Examples](examples): runnable programs, started with `cargo run --example <name>`.

## Author

Stavros Grigoriou ([stav121](https://github.com/stav121))

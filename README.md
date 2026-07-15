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

⏱️ An asynchronous task scheduling library written in Rust

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
| futures   | 0.3.31  |
| thiserror | 2.0.17  |

## How to use this library

In your `Cargo.toml` add:

```
[dependencies]
tasklet = "0.3.0"
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

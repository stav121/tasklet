<p align="center">
    <img src="tasklet-logo.png">
</p>

[![CircleCI](https://img.shields.io/circleci/build/github/stav121/tasklet?style=for-the-badge&logo=circleci)](https://circleci.com/gh/stav121/tasklet)
![Crates.io](https://img.shields.io/crates/d/tasklet?style=for-the-badge&color=blue&logo=owncloud)
![Crates.io](https://img.shields.io/crates/v/tasklet?style=for-the-badge&color=orange&logo=rust)
![GitHub last commit](https://img.shields.io/github/last-commit/stav121/tasklet?style=for-the-badge&color=purple&logo=git&logoColor=white)
[![Codecov](https://img.shields.io/codecov/c/github/stav121/tasklet?style=for-the-badge&logo=codecov&logoColor=white)](https://codecov.io/gh/stav121/tasklet)
![License](https://img.shields.io/github/license/stav121/tasklet?style=for-the-badge&color=lightgrey&logo=amazoniam&logoColor=white)
[![GitHub issues](https://img.shields.io/github/issues/stav121/tasklet?style=for-the-badge&color=yellow&logo=github)](https://github.com/stav121/tasklet/issues)

⏱️ An asynchronous task scheduling library written in Rust

## About

`tasklet` is a task scheduling library written in Rust. It is built over `tokio` runtime and utilizes green threads
in order to run tasks asynchronously.

## Dependencies

| library | version |
|---------|---------|
| cron    | 0.12.1  |
| chrono  | 0.4.38  |
| time    | 0.3.36  |
| log     | 0.4.21  |
| tokio   | 1.37.0  |

## How to use this library

In your `Cargo.toml` add:

```
[dependencies]
tasklet = "0.2.1"
```

## Example

Find more examples in the [examples](/examples) folder.

```rust
use log::{error, info};
use simple_logger::SimpleLogger;
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
    scheduler.add_task(
        TaskBuilder::new(chrono::Local)
            .every("1 * * * * * *")
            .description("A simple task")
            .add_step_default(|| {
                info!("Hello from step 1");
                Ok(()) // Let the scheduler know this step was a success.
            })
            .add_step_default(move || {
                if exec_count % 2 == 0 {
                    error!("Oh no this step failed!");
                    exec_count += 1;
                    Err(()) // Indicate that this step was a fail.
                } else {
                    info!("Hello from step 2");
                    exec_count += 1;
                    Ok(()) // Indicate that this step was a success.
                }
            })
            .build(),
    );

    // Execute the scheduler.
    scheduler.run().await;
}
```

## Author

Stavros Grigoriou ([stav121](github.com/stav121))

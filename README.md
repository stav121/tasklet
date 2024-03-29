![](tasklet-logo.png)

[![CircleCI](https://circleci.com/gh/stav121/tasklet.svg?style=shield)](https://circleci.com/gh/stav121/tasklet)
![Crates.io](https://img.shields.io/crates/d/tasklet)
![Crates.io](https://img.shields.io/crates/v/tasklet)
![GitHub last commit](https://img.shields.io/github/last-commit/stav121/tasklet)
[![codecov](https://codecov.io/gh/stav121/tasklet/branch/main/graph/badge.svg?token=HBIQJYK1EU)](https://codecov.io/gh/stav121/tasklet)
![License](https://img.shields.io/github/license/stav121/tasklet)
[![GitHub issues](https://img.shields.io/github/issues/stav121/tasklet)](https://github.com/stav121/tasklet/issues)

⏱️ A task scheduling library written in Rust

## Dependencies

* cron (0.12.0)
* chrono (0.4.31)
* time (0.3.31)
* log (0.4.20)

## Use this library

In your `Cargo.toml` add:
```
[dependencies]
tasklet = "0.1.4"
```

## Example
Find more examples in the [examples](/examples) folder.
```rust
use log::{error, info};
use simple_logger::SimpleLogger;
use tasklet::{TaskBuilder, TaskScheduler};

/// A simple example of a task with two step,
/// that might work or fail some times.
fn main() {
    // Init the logger.
    SimpleLogger::new().init().unwrap();

    // A variable to be passed in the task.
    let mut exec_count = 0;

    // Task scheduler with 2000ms loop frequency.
    let mut scheduler = TaskScheduler::new(2000, chrono::Local);

    // Create a task with 2 steps and add it to the scheduler.
    // The second step fails every second execution.
    // Append the task to the scheduler.
    scheduler.add_task(
        TaskBuilder::new(chrono::Local)
            .every("1 * * * * * *")
            .description("A simple task")
            .add_step(None, || {
                info!("Hello from step 1");
                Ok(()) // Let the scheduler know this step was a success.
            })
            .add_step(None, move || {
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
    scheduler.run();
}
```

## Author

Stavros Grigoriou ([stav121](github.com/stav121))

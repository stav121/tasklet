# Tasks and steps

A task is a schedule plus an ordered list of async steps. Each run executes the steps
one after the other; a step's result decides whether the run continues, retries the
step, fails, or removes the task. This page covers building tasks, writing steps,
handling failures and the lifecycle callbacks.

## Building a task

`TaskBuilder::new(timezone)` starts a task and `build()` returns a
`TaskResult<Task<T>>`. `TaskScheduler::add_task` accepts that `Result` directly, so an
invalid schedule surfaces when the task is added.

```rust
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{RetryPolicy, TaskBuilder, TaskScheduler};

let mut scheduler = TaskScheduler::default(chrono::Utc);
scheduler
    .add_task(
        TaskBuilder::new(chrono::Utc)
            .name("cleanup")
            .description("Delete expired sessions")
            .daily_at(3, 0)
            .timeout(Duration::from_secs(30))
            .retry(RetryPolicy::fixed(2, Duration::from_secs(5)))
            .add_step("delete", |_ctx| async { Ok(Success) })
            .build(),
    )
    .unwrap();
```

| Builder method | Purpose | Default |
|---|---|---|
| `every(cron)` and the helpers in [Scheduling](scheduling.md) | When the task runs | Every second (`* * * * * * *`) |
| `name(name)` | Unique name for runtime control | None |
| `description(text)` | Free text, used in logs | `-` |
| `repeat(n)` | Stop after `n` runs | Run forever |
| `add_step(description, step)`, `add_step_default(step)` | Append a step | No steps |
| `timeout(duration)` | Limit for each step attempt | None |
| `retry(policy)` | Retry failing steps | No retries |
| `on_success`, `on_failure`, `on_finish` | Lifecycle callbacks | None |
| `blackboard(board)` | Task-level shared store | A fresh, empty store |
| `overlap(policy)` | A slot comes due while a run is in progress | `OverlapPolicy::Skip` |
| `missed_runs(policy)` | A scheduled run starts late | `MissedRunPolicy::RunOnce` |
| `pool(name)` | Concurrency pool | None |
| `history_limit(n)` | Run records kept | 20 |

`repeat(n)` counts every run, failed ones included. After the `n`-th run the task leaves
the scheduler and `on_finish` fires with `FinishReason::Completed`. A run cancelled by
removal or shutdown does not count, so removing a task during its last run reports
`FinishReason::Removed` (or `Shutdown`), not `Completed`.

## Writing steps

A step is a closure that takes a `TaskContext` and returns a future resolving to
`Result<TaskStepStatusOk, TaskStepStatusErr>`. The closure is `FnMut + Send + 'static`
and the future must be `Send + 'static`, so the step can `.await` I/O and timers without
blocking the runtime. Write `|_ctx|` when the step does not need the context.

The closure is called once per attempt. To keep state between runs, hold it in the
closure, take a snapshot when the closure is called, and move the snapshot into the
`async move` block:

```rust
use tasklet::task::TaskStepStatusErr;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::TaskBuilder;

let mut exec_count = 0;
let _task = TaskBuilder::new(chrono::Utc)
    .every_seconds(10)
    .add_step("flaky", move |_ctx| {
        let count = exec_count;
        exec_count += 1;
        async move {
            if count % 2 == 0 {
                Err(TaskStepStatusErr::error(format!("run {count} is even")))
            } else {
                Ok(Success)
            }
        }
    })
    .build();
```

## Step results

| Step returns | Step status | Effect on the run and the task |
|---|---|---|
| `Ok(Success)` | `Succeeded` | The next step runs |
| `Ok(HadErrors)` | `HadErrors` | The next step runs; the run's outcome is `HadErrors` and `on_success` fires |
| `Err(Error(reason))` | `Failed` after the last attempt | Retried under the task's `RetryPolicy`. After the last attempt the remaining steps are `Skipped`, the run's outcome is `Failed`, `on_failure` fires and the task stays scheduled |
| `Err(ErrorDelete(reason))` | `Failed` | No retries. The run's outcome is `ForceRemoved`, `on_failure` fires, then `on_finish` with `FinishReason::ForceRemoved`, and the task is removed |
| The step panics | As for `Err(Error(..))` | The panic is caught and treated as a retryable failure with the reason `step panicked: <message>` |

Build errors with `TaskStepStatusErr::error(reason)` and
`TaskStepStatusErr::error_delete(reason)`. The reason is stored in the step's
`StepState::error` and in `RunRecord::error`, next to `RunRecord::failed_step`.
`err.reason()` returns it, `err.is_fatal()` is true for `ErrorDelete`, and `Display`
prints the reason.

When the task is being removed or the scheduler is shutting down, different rules apply;
see [Control and shutdown](control-and-shutdown.md#cooperative-cancellation).

## Step context and data flow

Every step attempt receives a `TaskContext`:

| Method | Returns |
|---|---|
| `task_id()` | The id the scheduler assigned to the task |
| `task_name()` | The task's name, if set |
| `run_id()` | The run's id, counting from 0 for each task |
| `step_index()` | The step's zero-based index |
| `attempt()` | The attempt number, starting at 1 |
| `blackboard()` | The task-level `Blackboard`, shared across runs |
| `run_store()` | A `Blackboard` created fresh for each run |
| `is_cancelled()`, `cancelled()` | Whether the task is being stopped; see [Control and shutdown](control-and-shutdown.md#cooperative-cancellation) |

The task-level blackboard keeps values across runs. Attach one with `.blackboard(...)`,
or attach the same blackboard to several tasks to pass data between them. The run store
lets a step hand values to later steps of the same run without leaking them into the
next run.

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{Blackboard, TaskBuilder};

let board = Blackboard::new();
let _task = TaskBuilder::new(chrono::Local)
    .every("* * * * * * *")
    .blackboard(board.clone())
    .add_step("produce", |ctx| async move {
        ctx.run_store().set("value", (ctx.run_id() as u32 + 1) * 10);
        Ok(Success)
    })
    .add_step("consume", |ctx| async move {
        let value = ctx.run_store().get::<u32>("value").unwrap_or(0);
        // Task-level state survives across runs.
        let total = ctx.blackboard().get_or_insert("runs", 0u32) + 1;
        ctx.blackboard().set("runs", total);
        println!("run {} produced/consumed {} (total runs: {})", ctx.run_id(), value, total);
        Ok(Success)
    })
    .build();
```

A `Blackboard` is a cheaply clonable, typed key/value store. Every clone reads and writes
the same storage. `get::<T>(key)` returns `Some` only when the value under `key` was
stored as type `T`.

```rust
use tasklet::Blackboard;

let board = Blackboard::new();
board.set("attempts", 0u32);

let shared = board.clone();
shared.set("attempts", 3u32);

assert_eq!(board.get::<u32>("attempts"), Some(3));
assert_eq!(board.get::<String>("attempts"), None);
```

## Timeouts and retries

`timeout(duration)` limits each step attempt. An attempt that runs longer is cancelled
and counts as a retryable failure with a reason such as `timed out after 5s`.

`retry(policy)` re-attempts a step that returns `Err(Error(..))` or times out.
`ErrorDelete` is never retried.

```rust
use std::time::Duration;
use tasklet::retry::Jitter;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{RetryPolicy, TaskBuilder};

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(1)
    .timeout(Duration::from_secs(5))
    // Up to 3 retries, waiting 100 ms, 200 ms and 400 ms, never more than 2 s,
    // each delay picked at random between 0 and the computed value.
    .retry(
        RetryPolicy::exponential(3, Duration::from_millis(100), 2)
            .with_max_delay(Duration::from_secs(2))
            .with_jitter(Jitter::Full),
    )
    .add_step("call API", |_ctx| async { Ok(Success) })
    .build();
```

| Policy | Delay before retry `n` (counting from 0) |
|---|---|
| `RetryPolicy::fixed(max_retries, delay)` | `delay` |
| `RetryPolicy::exponential(max_retries, base, factor)` | `base * factor^n`, capped by `with_max_delay` if set |

`max_retries` counts attempts after the first one, so `fixed(2, ..)` makes at most three
attempts. `with_jitter` spreads the delays: `Jitter::Full` picks a value between 0 and
the computed delay, `Jitter::Equal` between half the delay and the delay, and
`Jitter::None` (the default) uses the delay as computed.

## Lifecycle callbacks

Callbacks are async closures. They are `FnMut + Send + 'static` and return a future that
is `Send + 'static`.

| Callback | Argument | Fires when |
|---|---|---|
| `on_success` | `RunReport` | A run ends with outcome `Success` or `HadErrors` |
| `on_failure` | `RunReport` | A run ends with outcome `Failed` or `ForceRemoved` |
| `on_finish` | `FinishReport` | The task leaves the scheduler, once |

A `RunReport` holds `task_id`, `task_name`, `record` (the finished `RunRecord`) and
`steps` (each step's final `StepState`). A `FinishReport` holds `task_id`, `task_name`,
`reason` and `last_run`. `FinishReason` is `Completed` (repeat count used up or no
further schedule slot), `ForceRemoved` (`ErrorDelete`), `Removed` (removed through the
handle) or `Shutdown`. A fifth reason, `Panicked`, means the task's driver stopped
unexpectedly. Step and callback panics are caught, so only a bug in tasklet can cause it;
such a task is reported through `SchedulerEvent::TaskRemoved` and does not run
`on_finish`.

```rust
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{FinishReason, TaskBuilder};

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .on_success(|report| async move {
        println!("run {} took {} attempt(s)", report.record.run_id, report.record.attempts);
    })
    .on_failure(|report| async move {
        eprintln!(
            "step {:?} failed: {}",
            report.record.failed_step,
            report.record.error.as_deref().unwrap_or("unknown")
        );
    })
    .on_finish(|report| async move {
        if report.reason != FinishReason::Shutdown {
            println!("task {} left: {:?}", report.task_id, report.reason);
        }
    })
    .add_step("work", |_ctx| async { Ok(Success) })
    .build();
```

Points to keep in mind:

- `on_success` and `on_failure` run after the run's record and its `RunFinished` event
  are published and its concurrency permits are released. Until they return, the task
  counts as running, so its next run waits for them. Keep them short or hand slow work
  to `tokio::spawn`. Inside a callback, `handle.history(id)` already contains the
  finished run.
- A panic in a callback is caught and logged at `error` level; the task keeps running.
- A run interrupted by removal or shutdown has the outcome `Cancelled` and fires neither
  `on_success` nor `on_failure`.
- On removal and shutdown, `on_finish` runs within the scheduler's grace period. A task
  aborted at the end of the grace period does not run `on_finish`.
- To use a captured value in every call, clone it inside the closure before the
  `async move` block, as in the [migration example](MIGRATION.md#callbacks-receive-a-report).

## Runnable examples

- [`examples/context_and_spawner.rs`](../examples/context_and_spawner.rs): the step context, the per-run store and a shared blackboard.
- [`examples/retry_timeout_example.rs`](../examples/retry_timeout_example.rs): timeouts, retries and lifecycle callbacks.
- [`examples/force_remove_example.rs`](../examples/force_remove_example.rs): a step that removes its task with `ErrorDelete`.

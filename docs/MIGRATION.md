# Migration guide

This guide lists the code changes needed when upgrading `tasklet` across a breaking
release, newest first. Releases that are not listed here (0.3.x to 0.4.x, 0.4.0 to
0.4.1, 0.3.0 to 0.3.2) only added API and need no code changes.

| Upgrade | Code changes needed | Section |
|---|---|---|
| 0.5.x to 0.6.0 | Step errors, callbacks, exhaustive matches, generator closures | [0.5.x to 0.6.0](#05x-to-060) |
| 0.4.x to 0.5.0 | Step closures take a `TaskContext` | [0.4.x to 0.5.0](#04x-to-050) |
| 0.3.x to 0.4.x | None | |
| 0.2.x to 0.3.0 | Steps are async, `TaskGenerator::new` returns a `Result` | [0.2.x to 0.3.0](#02x-to-030) |

## 0.5.x to 0.6.0

0.6.0 changes how step failures and lifecycle callbacks report what happened, and changes
how removal and shutdown stop a task. Most projects need the first three items of the
checklist; the rest apply only if your code uses the affected types.

### Checklist

1. Give every `Err(Error)` and `Err(ErrorDelete)` returned by a step a reason, for
   example `Err(TaskStepStatusErr::error("upstream returned 503"))`. See
   [Step failures carry a reason](#step-failures-carry-a-reason).
2. Add an argument to every `on_success`, `on_failure` and `on_finish` closure:
   `|| async { .. }` becomes `|_report| async { .. }`. See
   [Callbacks receive a report](#callbacks-receive-a-report).
3. Check what your `on_finish` callback does: it now also runs when a task is removed
   through the handle or the scheduler shuts down. Read `report.reason` if the
   difference matters. See [`on_finish` runs on removal and shutdown](#on_finish-runs-on-removal-and-shutdown).
4. Add a wildcard arm to `match` expressions over `RunOutcome` and `StepStatus`, and stop
   building `StepState` or `RunRecord` values with struct literals. See
   [Observable types are `#[non_exhaustive]`](#observable-types-are-non_exhaustive).
5. Replace `TaskError::MissingComponent` in exhaustive matches; add
   `TaskError::UnknownPool`. See [`TaskError` changes](#taskerror-changes).
6. Make `TaskGenerator` closures `Send` (for example `Arc<Mutex<_>>` instead of
   `Rc<RefCell<_>>`). See [`TaskGenerator` closures must be `Send`](#taskgenerator-closures-must-be-send).
7. Write `CallbackFn<RunReport>` or `CallbackFn<FinishReport>` where you named the
   `CallbackFn` type. See [`CallbackFn` is generic](#callbackfn-is-generic).
8. Review the [behaviour changes](#behaviour-changes-that-still-compile): shutdown and
   removal now wait for the run in progress, the `sleep` argument of
   `TaskScheduler::new` has a new meaning, and `OverlapPolicy::Queue` no longer queues
   extra runs.

### Step failures carry a reason

`TaskStepStatusErr::Error` and `TaskStepStatusErr::ErrorDelete` now hold a `String`. The
reason is stored in the step's `StepState::error`, in the run's `RunRecord::error`, and
reaches the `on_failure` callback through its `RunReport`.

Before (0.5):

```rust,ignore
// tasklet 0.5
use tasklet::task::TaskStepStatusErr::{Error, ErrorDelete};
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .add_step("fetch", |_ctx| async { Err(Error) })
    .add_step("check config", |_ctx| async { Err(ErrorDelete) })
    .build();
```

After (0.6), using the constructor helpers:

```rust
use tasklet::task::TaskStepStatusErr;
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .add_step("fetch", |_ctx| async {
        Err(TaskStepStatusErr::error("upstream returned 503"))
    })
    .add_step("check config", |_ctx| async {
        Err(TaskStepStatusErr::error_delete("configuration file removed"))
    })
    .build();
```

If you import the variants directly, pass the reason as a `String`:

```rust
use tasklet::task::TaskStepStatusErr::Error;
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .add_step("fetch", |_ctx| async { Err(Error("upstream returned 503".into())) })
    .build();
```

Code that matches on the error must bind or ignore the reason. The type also has
`reason()`, `is_fatal()` (true for `ErrorDelete`) and a `Display` implementation that
prints the reason:

```rust
use tasklet::task::TaskStepStatusErr;

fn describe(err: &TaskStepStatusErr) -> String {
    match err {
        TaskStepStatusErr::Error(reason) => format!("retryable: {reason}"),
        TaskStepStatusErr::ErrorDelete(reason) => format!("fatal: {reason}"),
    }
}

let err = TaskStepStatusErr::error_delete("configuration file removed");
assert!(err.is_fatal());
assert_eq!(err.reason(), "configuration file removed");
assert_eq!(err.to_string(), "configuration file removed");
assert_eq!(describe(&err), "fatal: configuration file removed");
```

A step that times out now fails with a reason of the form `timed out after 5s`.

### Callbacks receive a report

`on_success` and `on_failure` closures take a `RunReport`; `on_finish` closures take a
`FinishReport`. A closure that ignores the argument needs a parameter all the same.

Before (0.5):

```rust,ignore
// tasklet 0.5
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .on_success(|| async { println!("run succeeded") })
    .on_failure(|| async { eprintln!("run failed") })
    .on_finish(|| async { println!("task finished") })
    .build();
```

After (0.6):

```rust
use tasklet::{FinishReason, TaskBuilder};

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .on_success(|report| async move {
        println!("run {} succeeded", report.record.run_id);
    })
    .on_failure(|report| async move {
        eprintln!(
            "run {} failed at step {:?}: {:?}",
            report.record.run_id, report.record.failed_step, report.record.error
        );
    })
    .on_finish(|report| async move {
        match report.reason {
            FinishReason::Completed => println!("task {} completed", report.task_id),
            other => println!("task {} stopped: {other:?}", report.task_id),
        }
    })
    .build();
```

`RunReport` has `task_id`, `task_name`, `record` (the finished `RunRecord`) and `steps`
(every step's final `StepState`). `FinishReport` has `task_id`, `task_name`, `reason`
(a `FinishReason`) and `last_run` (the most recent `RunRecord`, if the task ran).

When a callback captured values and cloned them per call, keep the clone and add the
parameter:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tasklet::TaskBuilder;

let failures = Arc::new(AtomicUsize::new(0));
let counter = failures.clone();
let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .on_failure(move |_report| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    })
    .build();
```

### `on_finish` runs on removal and shutdown

In 0.5, `on_finish` fired only when a task ended on its own (repeat count reached, no
further schedule slot, or `ErrorDelete`). In 0.6 it fires once whenever a task leaves the
scheduler, and `FinishReport::reason` says why:

| `FinishReason` | Cause |
|---|---|
| `Completed` | The repeat count is used up, or the schedule has no further slot |
| `ForceRemoved` | A step returned `ErrorDelete` |
| `Removed` | `SchedulerHandle::remove` or `remove_name` |
| `Shutdown` | The scheduler shut down |
| `Panicked` | The task's driver stopped unexpectedly (an internal bug; step and callback panics are caught). Reported only in `SchedulerEvent::TaskRemoved`; `on_finish` does not run |

`FinishReason` is `#[non_exhaustive]`, so a `match` on it needs a wildcard arm. A run
cancelled by removal or shutdown does not use up a repeat: removing a task during its
last repeat reports `Removed`, not `Completed`.

On removal and shutdown the callback runs inside the scheduler's grace period. If the
task's run is still going when the grace period ends, the task is aborted and its
`on_finish` does not run. A callback that should only react to normal completion must
check the reason:

```rust
use tasklet::{FinishReason, TaskBuilder};

let _task = TaskBuilder::new(chrono::Utc)
    .every_minutes(5)
    .repeat(10)
    .on_finish(|report| async move {
        if report.reason == FinishReason::Completed {
            println!("all 10 runs done");
        }
    })
    .build();
```

A run interrupted by removal or shutdown fires neither `on_success` nor `on_failure`; its
record has the outcome `RunOutcome::Cancelled`.

### Observable types are `#[non_exhaustive]`

`StepState`, `RunRecord`, `RunOutcome` and `StepStatus` are now `#[non_exhaustive]`, so
later releases can add fields and variants without breaking your build. The new types
`RunReport`, `FinishReport`, `FinishReason`, `MissedRunPolicy` and `SchedulerEvent` are
`#[non_exhaustive]` too. `TaskState` already was.

New in 0.6:

| Type | Addition |
|---|---|
| `StepState` | `attempts` (attempts made in the run), `error` (reason of the last failed attempt) |
| `RunRecord` | `error` (reason of the failure that ended the run), `failed_step` (its index) |
| `RunOutcome` | `Cancelled` |
| `StepStatus` | `Running`, `Cancelled` |
| `TaskState` | `waiting` (the run waits for a concurrency permit) |

A `match` over `RunOutcome` or `StepStatus` needs a wildcard arm.

Before (0.5):

```rust,ignore
// tasklet 0.5
use tasklet::RunOutcome;

fn label(outcome: &RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Success => "ok",
        RunOutcome::HadErrors => "ok with errors",
        RunOutcome::Failed => "failed",
        RunOutcome::ForceRemoved => "removed",
    }
}
```

After (0.6):

```rust
use tasklet::RunOutcome;

fn label(outcome: &RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Success => "ok",
        RunOutcome::HadErrors => "ok with errors",
        RunOutcome::Failed => "failed",
        RunOutcome::ForceRemoved => "removed",
        RunOutcome::Cancelled => "cancelled",
        _ => "unknown",
    }
}
```

Outside the crate you can no longer build these structs with a struct literal or
destructure them without `..`. Tests that compared a whole `StepState` against a literal
should compare fields instead.

Before (0.5):

```rust,ignore
// tasklet 0.5
use tasklet::{StepState, StepStatus};

fn is_first_success(state: &StepState) -> bool {
    *state
        == StepState {
            index: 0,
            description: Some("fetch".to_string()),
            status: StepStatus::Succeeded,
        }
}
```

After (0.6):

```rust
use tasklet::{StepState, StepStatus};

fn is_first_success(state: &StepState) -> bool {
    state.index == 0
        && state.description.as_deref() == Some("fetch")
        && state.status == StepStatus::Succeeded
}
```

### `TaskError` changes

`TaskError::MissingComponent` was removed; nothing in 0.5 returned it.
`TaskError::UnknownPool(String)` was added: `add_task`, `add_task_get_id` and
`TaskSpawner::spawn_get_id` return it for a task whose `pool(..)` names a pool that was
not created with `TaskScheduler::add_pool`. `TaskError` is not `#[non_exhaustive]`, so
an exhaustive `match` has to change.

Before (0.5):

```rust,ignore
// tasklet 0.5
use tasklet::TaskError;

fn is_config_error(err: &TaskError) -> bool {
    match err {
        TaskError::InvalidCronExpression(_)
        | TaskError::DuplicateTaskName(_)
        | TaskError::MissingComponent(_) => true,
        TaskError::NotInitialized
        | TaskError::AlreadyExecuted
        | TaskError::Failed
        | TaskError::Finished
        | TaskError::ForceRemoved
        | TaskError::ExecutionError(_) => false,
    }
}
```

After (0.6):

```rust
use tasklet::TaskError;

fn is_config_error(err: &TaskError) -> bool {
    match err {
        TaskError::InvalidCronExpression(_)
        | TaskError::DuplicateTaskName(_)
        | TaskError::UnknownPool(_) => true,
        TaskError::NotInitialized
        | TaskError::AlreadyExecuted
        | TaskError::Failed
        | TaskError::Finished
        | TaskError::ForceRemoved
        | TaskError::ExecutionError(_) => false,
    }
}
```

### `TaskGenerator` closures must be `Send`

The discovery closure passed to `TaskGenerator::new` must now be `Send`. This is what
makes `TaskScheduler` itself `Send`, so a scheduler can be moved into `tokio::spawn`.
Replace `Rc` and `RefCell` captures with `Arc` and a `Mutex` or an atomic.

Before (0.5):

```rust,ignore
// tasklet 0.5
use std::cell::RefCell;
use std::rc::Rc;
use tasklet::{TaskBuilder, TaskGenerator, TaskScheduler};

let generated = Rc::new(RefCell::new(0u32));
let counter = generated.clone();
let generator = TaskGenerator::new("0 * * * * * *", chrono::Utc, move || {
    *counter.borrow_mut() += 1;
    Some(TaskBuilder::new(chrono::Utc).every_seconds(10).repeat(1).build())
})
.unwrap();
let mut scheduler = TaskScheduler::default(chrono::Utc);
scheduler.set_task_gen(generator);
```

After (0.6):

```rust
use std::sync::{Arc, Mutex};
use tasklet::{TaskBuilder, TaskGenerator, TaskScheduler};

let generated = Arc::new(Mutex::new(0u32));
let counter = generated.clone();
let generator = TaskGenerator::new("0 * * * * * *", chrono::Utc, move || {
    *counter.lock().unwrap() += 1;
    Some(TaskBuilder::new(chrono::Utc).every_seconds(10).repeat(1).build())
})
.unwrap();
let mut scheduler = TaskScheduler::default(chrono::Utc);
scheduler.set_task_gen(generator);
```

### `CallbackFn` is generic

The boxed callback type `tasklet::task::CallbackFn` now takes the callback's argument
type. This only matters if you named the type yourself.

Before (0.5):

```rust,ignore
// tasklet 0.5
use tasklet::task::CallbackFn;

type Callback = Box<CallbackFn>;
```

After (0.6):

```rust
use tasklet::task::CallbackFn;
use tasklet::{FinishReport, RunReport};

type RunCallback = Box<CallbackFn<RunReport>>;
type FinishCallback = Box<CallbackFn<FinishReport>>;
```

### Behaviour changes that still compile

These changes need no code edits, but they change what a running program does.

**Shutdown waits for runs in progress.** In 0.5, `SchedulerHandle::shutdown` (and the
future passed to `run_until`) aborted every task at once. In 0.6 the scheduler stops
dispatching, asks every task to stop, waits up to its grace period (5 seconds by
default) for runs in progress to finish, aborts what is still running, and then returns
from `run()`. Steps see the request through `TaskContext::is_cancelled()` and
`ctx.cancelled().await`. A run that was aborted is recorded with
`RunOutcome::Cancelled`. Expect `run()` to return up to one grace period later than
before. To keep the 0.5 behaviour, set a zero grace period; tasks are then aborted at
once and their `on_finish` callbacks may not run:

```rust
use std::time::Duration;
use tasklet::TaskScheduler;

let mut scheduler = TaskScheduler::default(chrono::Utc);
scheduler.grace_period(Duration::ZERO);
```

**Removal lets the current run finish.** In 0.5, `SchedulerHandle::remove` aborted a run
in progress on the next scheduler round. In 0.6 the run is asked to stop in the same way
as on shutdown and gets the grace period before it is aborted; `on_finish` fires with
`FinishReason::Removed`.

A long step should check for the stop request so that shutdown and removal do not have
to wait for the whole grace period:

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
    .build();
```

A step that returns `Err(Error(..))` after the stop request is recorded as
`StepStatus::Cancelled`, is not retried and does not fire `on_failure`.

**The `sleep` argument of `TaskScheduler::new` is a maximum idle wait.** In 0.5 the
scheduler polled every `sleep` milliseconds, so a run could start up to one period after
its slot. In 0.6 the loop sleeps until the next task is due and wakes early when a task
is added, spawned, resumed, removed or finishes a run, so runs start on their slot.
`sleep` now only bounds how long the loop sleeps without checking the clock, which
matters after the system clock jumps forward. A small value such as 50 ms, chosen in 0.5
for precision, is no longer needed; the 1000 ms default of `TaskScheduler::default` is
enough for most programs.

**`OverlapPolicy::Queue` queues only real overlaps.** In 0.5 a task with
`OverlapPolicy::Queue` queued an extra run whenever a run lasted longer than one polling
period, even if no schedule slot came due during the run. In 0.6 a run is queued only
when a slot actually came due while the previous run was in progress (at most one).

**Late runs follow a `MissedRunPolicy`.** The default, `MissedRunPolicy::RunOnce`, keeps
the 0.5 behaviour: a run that starts late (for example after the task was paused over
its slot) runs once, then the task continues with its next future slot. Set
`MissedRunPolicy::Skip { grace }` with `TaskBuilder::missed_runs` to drop such runs
instead. See [Scheduling](scheduling.md#late-runs).

**Tasks are initialized when they are added.** `add_task` and `add_task_get_id` compute
the first execution time straight away, so `SchedulerHandle::statuses()` reports the task
as `Status::Scheduled` with its `next_exec` set before `run()` is called. In 0.5 the
status stayed `Status::Init` until the task's driver started. `add_task` also works
outside a Tokio runtime now; such tasks start when the scheduler runs.

**A panic no longer ends the task.** In 0.5 a panic in a step or a callback ended the
task's driver. In 0.6 a panicking step counts as a retryable failure with the reason
`step panicked: <message>`, and a panicking callback is logged at `error` level and
ignored.

**The handle is empty after the scheduler stops.** When `run()` or `run_until()`
returns, the handle has no registered tasks: `statuses()`, `history(id)` and
`step_states(id)` return empty vectors. Read a task's last run from
`FinishReport::last_run` in `on_finish`, or from `SchedulerEvent::RunFinished`.
`TaskSpawner::spawn` and `spawn_get_id` return `TaskError::ExecutionError` from then on.

**Rejected generated and spawned tasks are reported.** A task from a `TaskGenerator` or
a fire-and-forget `TaskSpawner::spawn` that cannot be added (duplicate name, unknown
pool) used to be dropped silently. It is now logged at `warn` level and published as
`SchedulerEvent::TaskRejected`.

### New in 0.6.0

These additions need no migration. Each links to its guide.

- Event stream: `SchedulerHandle::subscribe()` returns a broadcast receiver of
  `SchedulerEvent`s. See [Observability](observability.md#events).
- Concurrency limits: `TaskScheduler::max_concurrent_runs`, named pools with
  `TaskScheduler::add_pool` and `TaskBuilder::pool`. See
  [Scheduling](scheduling.md#concurrency-limits).
- Missed-run policy: `TaskBuilder::missed_runs`. See [Scheduling](scheduling.md#late-runs).
- Cooperative cancellation and a configurable grace period:
  `TaskContext::is_cancelled`, `TaskContext::cancelled`, `TaskScheduler::grace_period`.
  See [Control and shutdown](control-and-shutdown.md).
- `TaskScheduler` is `Send` and can run in a spawned Tokio task.

## 0.4.x to 0.5.0

**Steps receive a `TaskContext`.** The step closure signature changed from
`FnMut() -> Future` to `FnMut(TaskContext) -> Future`. Add a parameter to every step; if
you do not need it, ignore it with `|_ctx|`.

Before (0.4):

```rust,ignore
// tasklet 0.4
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every("* * * * * * *")
    .add_step("Step", || async { Ok(Success) })
    .build();
```

After (0.5):

```rust,ignore
// tasklet 0.5
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every("* * * * * * *")
    .add_step("Step", |_ctx| async { Ok(Success) })
    .build();
```

In 0.5 the lifecycle callbacks (`on_success`, `on_failure`, `on_finish`) still took
`|| async { ... }`; 0.6 changed them (see above). The context gives steps access to a
shared `Blackboard` and a per-run store; see
[Tasks and steps](tasks-and-steps.md#step-context-and-data-flow).

Additions in 0.5.0 that need no migration: `TaskBuilder::blackboard(...)` attaches a
shared blackboard, `TaskScheduler::spawner()` returns a `TaskSpawner` for adding tasks to
a running scheduler, and the handle's task registry is filled when `add_task` returns.

## 0.2.x to 0.3.0

**Steps are async.** A step closure must return a future. Wrap synchronous bodies in an
`async` block.

Before (0.2):

```rust,ignore
// tasklet 0.2
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every("* * * * * * *")
    .add_step("Step", || Ok(Success))
    .build();
```

After (0.3 and 0.4; 0.5 added the `|_ctx|` parameter):

```rust,ignore
// tasklet 0.4
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::TaskBuilder;

let _task = TaskBuilder::new(chrono::Utc)
    .every("* * * * * * *")
    .add_step("Step", || async { Ok(Success) })
    .build();
```

For state that changes between runs, snapshot it in the (`FnMut`) closure and `move` the
snapshot into the `async move` block so the future stays `'static`.

**`TaskGenerator::new` returns a `Result`.** It no longer panics on an invalid cron
expression; call `?` or `.unwrap()` on the result.

**`TaskScheduler::run` can stop.** It returns when a shutdown is requested through a
`SchedulerHandle`. If you never request one, it runs forever as before.

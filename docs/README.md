# tasklet documentation

These guides describe `tasklet` 0.6.0. The [API reference on docs.rs](https://docs.rs/tasklet)
documents every type and method; the guides explain how the parts work together.

| Guide | Covers |
|---|---|
| [Tasks and steps](tasks-and-steps.md) | Building tasks, writing async steps, step results and error reasons, the step context, blackboards, timeouts, retries, lifecycle callbacks |
| [Scheduling and concurrency](scheduling.md) | Cron expressions and helpers, timezones, the scheduler loop, overlapping and late runs, concurrency limits and pools, adding tasks before and during a run |
| [Control and shutdown](control-and-shutdown.md) | The scheduler handle, pause, resume, trigger and remove, cooperative cancellation, the grace period, graceful shutdown |
| [Observability](observability.md) | Task snapshots, run history, live step states, the event stream, serde support, logging |
| [Migration guide](MIGRATION.md) | Code changes for each breaking release, starting with 0.5.x to 0.6.0 |

Runnable programs are in the [`examples`](../examples) folder. Run one with
`cargo run --example <name>`, for example `cargo run --example simple_example`.

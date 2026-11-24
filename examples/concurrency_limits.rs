use log::info;
use simple_logger::SimpleLogger;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tasklet::task::TaskStepStatusOk::Success;
use tasklet::{TaskBuilder, TaskScheduler, TaskState};

/// Counts how many runs are inside a section right now and the most there ever were.
#[derive(Default)]
struct Gauge {
    current: AtomicUsize,
    peak: AtomicUsize,
}

impl Gauge {
    fn enter(&self) {
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }

    fn leave(&self) {
        self.current.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Five tasks come due every second and each run takes over a second, but the scheduler
/// lets at most three run at once, and at most one of the two `db-*` tasks, which share
/// the `database` pool. Runs over a limit wait for a permit and show up as `waiting`.
#[tokio::main]
async fn main() {
    SimpleLogger::new().init().unwrap();

    let mut scheduler = TaskScheduler::new(200, chrono::Utc);
    // Pools must exist before a task that names them is added.
    scheduler.max_concurrent_runs(3).add_pool("database", 1);

    let all = Arc::new(Gauge::default());
    let database = Arc::new(Gauge::default());

    let tasks = [
        ("report-a", None),
        ("report-b", None),
        ("report-c", None),
        ("db-1", Some("database")),
        ("db-2", Some("database")),
    ];
    for (name, pool) in tasks {
        let all = all.clone();
        let database = database.clone();
        let mut builder = TaskBuilder::new(chrono::Utc)
            .every_seconds(1)
            .name(name)
            .add_step("work", move |_ctx| {
                let all = all.clone();
                let database = database.clone();
                async move {
                    all.enter();
                    if pool.is_some() {
                        database.enter();
                    }
                    tokio::time::sleep(Duration::from_millis(1200)).await;
                    if pool.is_some() {
                        database.leave();
                    }
                    all.leave();
                    Ok(Success)
                }
            });
        if let Some(pool) = pool {
            builder = builder.pool(pool);
        }
        scheduler.add_task(builder.build()).unwrap();
    }

    let handle = scheduler.handle();
    let observer = tokio::spawn(async move {
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(600)).await;
            let states = handle.statuses();
            let names = |pick: fn(&TaskState) -> bool| {
                states
                    .iter()
                    .filter(|s| pick(s))
                    .filter_map(|s| s.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            info!(
                "[status] running: [{}]  waiting: [{}]",
                names(|s| s.running),
                names(|s| s.waiting)
            );
        }
    });

    scheduler
        .run_until(tokio::time::sleep(Duration::from_secs(5)))
        .await;
    observer.await.unwrap();

    let peak_all = all.peak.load(Ordering::SeqCst);
    let peak_database = database.peak.load(Ordering::SeqCst);
    info!("peak concurrent runs: {} (limit 3)", peak_all);
    info!(
        "peak runs in the database pool: {} (limit 1)",
        peak_database
    );
    assert!(peak_all <= 3 && peak_database <= 1);
}

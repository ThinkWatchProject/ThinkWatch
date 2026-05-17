//! Supervised task spawning — panic-isolated, metric-emitting.
//!
//! `tokio::spawn(future)` on its own swallows panics: a critical
//! background loop (audit worker, retention sweep, MCP catalog
//! refresh, …) can die silently while the rest of the server keeps
//! reporting healthy. The symptom is the worst kind of incident —
//! audit entries stop reaching ClickHouse hours before anyone
//! notices, GDPR purges stop running, the tool catalog ages forever.
//!
//! Two entry points:
//!
//! * [`supervise`] — fire-and-forget; on panic logs + bumps the
//!   metric, does NOT restart. Right for finite work whose death
//!   is recoverable next request (e.g. `last_used_at` update).
//! * [`supervise_restart`] — infinite-loop guardian; on panic logs +
//!   bumps the metric, sleeps with exponential backoff, then calls
//!   the factory again for a fresh future. Right for tasks that
//!   MUST stay alive.
//!
//! Every panic increments
//! `supervised_task_panics_total{task="<task_name>"}` and emits an
//! ERROR log including the panic message — alert on the metric.
//!
//! Implementation note: we inspect the panic via `JoinHandle::await`
//! plus `JoinError::into_panic()` rather than
//! `FutureExt::catch_unwind`, so this module is dependency-free
//! beyond the runtime crates the workspace already pulls in.

use std::future::Future;
use std::time::Duration;

const RESTART_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Spawn `fut` and capture any panic into a metric + ERROR log. The
/// task is NOT restarted on panic — use [`supervise_restart`] for
/// infinite loops that must stay alive.
///
/// `task_name` becomes a Prometheus label value; keep it a static
/// lower-snake-case identifier so cardinality stays bounded.
pub fn supervise<F>(task_name: &'static str, fut: F) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let handle = tokio::spawn(fut);
        match handle.await {
            Ok(()) => {}
            Err(e) if e.is_panic() => {
                let msg = panic_message(e.into_panic());
                metrics::counter!("supervised_task_panics_total", "task" => task_name).increment(1);
                tracing::error!(task = task_name, panic = %msg, "supervised task panicked");
            }
            Err(_cancelled) => {
                tracing::info!(task = task_name, "supervised task cancelled");
            }
        }
    })
}

/// Spawn an infinite-loop task with panic-restart. On panic the
/// supervisor logs + bumps `supervised_task_panics_total`, sleeps
/// for an exponential-backoff delay (1s → 60s cap), then calls
/// `factory()` for a fresh future. A clean `()` return is logged
/// once and the supervisor exits (this is how intentional shutdown
/// looks).
///
/// `factory` must be `Fn` (not `FnOnce`) because each restart needs
/// a fresh future. Captured state should be `Clone` and the
/// closure should clone what the future needs each invocation.
pub fn supervise_restart<F, Fut>(task_name: &'static str, factory: F) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut delay = RESTART_BACKOFF_INITIAL;
        loop {
            let handle = tokio::spawn(factory());
            match handle.await {
                Ok(()) => {
                    tracing::info!(task = task_name, "supervised task exited cleanly");
                    return;
                }
                Err(e) if e.is_panic() => {
                    let msg = panic_message(e.into_panic());
                    metrics::counter!("supervised_task_panics_total", "task" => task_name)
                        .increment(1);
                    tracing::error!(
                        task = task_name,
                        panic = %msg,
                        backoff_secs = delay.as_secs(),
                        "supervised task panicked, restarting after backoff"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(RESTART_BACKOFF_MAX);
                }
                Err(_cancelled) => {
                    tracing::info!(task = task_name, "supervised task cancelled");
                    return;
                }
            }
        }
    })
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn supervise_runs_to_completion() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = counter.clone();
        supervise("test_one_shot", async move {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn supervise_swallows_panic_without_aborting_runtime() {
        // The supervisor itself returns Ok even though the inner
        // task panicked — confirms the panic was captured, not
        // propagated to the parent.
        let handle = supervise("test_panic", async {
            panic!("intentional");
        });
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn supervise_restart_respawns_after_panic() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = counter.clone();
        // Stop after 3 attempts by having the factory close the loop.
        tokio::spawn(async move {
            supervise_restart("test_restart", move || {
                let c = c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        panic!("attempt {n}");
                    }
                    // Third call returns Ok — supervisor exits.
                }
            })
            .await
            .ok();
        });
        // The first two restarts are forced to wait the 1s + 2s
        // backoff floor; cap the assertion at a generous bound.
        for _ in 0..50 {
            if counter.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }
}

//! [`AuditLogger`] — the cloneable channel front-end every emit site
//! talks to — plus the background worker that drains the channel
//! into ClickHouse + the dynamic forwarder set, and the periodic
//! forwarder-registry reload.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::{Mutex, mpsc};

use super::clickhouse::flush_to_clickhouse;
use super::forwarders::{
    ForwarderRegistry, ForwarderRuntime, Registry, send_kafka, send_tcp_syslog, send_udp_syslog,
    send_webhook,
};
use super::outbox::{drain_once, webhook_outbox_drain_loop};
use super::types::AuditEntry;
use crate::models::LogForwarder;
use crate::tasks::supervise_restart;

#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// ClickHouse HTTP endpoint, e.g. "http://localhost:8123"
    pub clickhouse_url: Option<String>,
    /// ClickHouse database name
    pub clickhouse_db: String,
    /// ClickHouse user for authentication
    pub clickhouse_user: Option<String>,
    /// ClickHouse password for authentication
    pub clickhouse_password: Option<String>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            clickhouse_url: None,
            clickhouse_db: "think_watch".into(),
            clickhouse_user: None,
            clickhouse_password: None,
        }
    }
}

/// Async audit log dispatcher. Receives entries via a bounded channel,
/// writes to ClickHouse + DB-configured forwarders (syslog, kafka, webhook).
#[derive(Clone)]
pub struct AuditLogger {
    tx: mpsc::Sender<AuditEntry>,
    db: Option<PgPool>,
    registry: ForwarderRegistry,
    /// Sample rate in 1/10000ths (0 = drop everything, 10000 = keep
    /// everything). Stored as a `u32` so reads are lock-free on the
    /// hot logging path; written by the dynamic-config subscriber
    /// when `audit.sample_rate` changes.
    sample_rate_bps: Arc<std::sync::atomic::AtomicU32>,
}

/// Bounded audit channel capacity.
///
/// At ~80 bytes per entry that's about 8 MB of in-memory backlog,
/// which is fine for any reasonable host. 100k entries means a
/// 30-second ClickHouse outage at 3k req/s still survives; drops
/// are surfaced via a metric.
const AUDIT_CHANNEL_CAPACITY: usize = 100_000;

/// Throttle window for the structured "audit drop" log line. The
/// metric still increments on every drop, so dashboards see the
/// real rate; the log is just for human inspection and one line
/// per second is plenty.
fn log_audit_drop_throttled(err: &tokio::sync::mpsc::error::TrySendError<AuditEntry>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static LAST_LOG_SECS: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_LOG_SECS.load(Ordering::Relaxed);
    if now != last
        && LAST_LOG_SECS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::error!(
            "Audit log channel send failed (buffer full or closed): {err} \
             — see audit_log_dropped_total / audit_log_queue_depth metrics"
        );
    }
}

impl AuditLogger {
    /// Build an audit logger that accepts entries but never
    /// forwards them anywhere. Use in unit tests where the
    /// system-under-test calls `.log(entry)` and the test doesn't
    /// care about the side effects (sample rate is 100%, no
    /// worker, channel just fills until the test ends).
    #[cfg(test)]
    pub fn test_drain() -> Self {
        let (tx, _rx) = mpsc::channel(64);
        Self {
            tx,
            db: None,
            registry: Arc::new(Registry::new()),
            sample_rate_bps: Arc::new(std::sync::atomic::AtomicU32::new(10_000)),
        }
    }

    /// Run one pass of the webhook-outbox drain loop. Production
    /// invokes the same code path on a 10-second tick from the
    /// background task spawned in `AuditLogger::new`; tests call
    /// this method directly so they don't have to wait for the
    /// real interval.
    pub async fn drain_webhook_outbox_once(&self) -> Result<(), sqlx::Error> {
        let Some(db) = &self.db else {
            return Ok(());
        };
        // `redirect::Policy::none()` so a webhook receiver can't 302
        // us into an internal service after the admin saved a benign
        // public URL — the audit forwarder explicitly delivers to
        // admin-supplied URLs and is a prime SSRF surface.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        let mut over: Option<std::time::Instant> = None;
        let mut last: Option<std::time::Instant> = None;
        drain_once(db, &self.registry, &http, &mut over, &mut last).await
    }

    pub async fn new(
        _config: AuditConfig,
        db: Option<PgPool>,
        ch: Option<clickhouse::Client>,
        dynamic_config: Option<Arc<crate::dynamic_config::DynamicConfig>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(AUDIT_CHANNEL_CAPACITY);
        let registry: ForwarderRegistry = Arc::new(Registry::new());
        let sample_rate_bps = Arc::new(std::sync::atomic::AtomicU32::new(10_000));

        // Populate the forwarder registry BEFORE the worker starts
        // consuming audit entries. Without this, an audit log sent in
        // the first few milliseconds after AuditLogger::new() returns
        // would see an empty registry and skip forwarding — logs that
        // should have gone to syslog/Kafka/webhooks during bootstrap
        // would silently vanish.
        if let Some(pool) = &db {
            reload_forwarders(pool, &registry).await;
        }

        // Pick up the persisted sample rate once before the worker
        // sees any traffic, then poll periodically. The dynamic_config
        // subscriber on Redis already keeps the in-memory cache fresh,
        // so polling every 30s costs nothing more than an Arc clone.
        if let Some(dc) = dynamic_config.as_ref() {
            let initial = dc.audit_sample_rate().await;
            sample_rate_bps.store(
                (initial.clamp(0.0, 1.0) * 10_000.0).round() as u32,
                std::sync::atomic::Ordering::Relaxed,
            );
            let dc = dc.clone();
            let bps = sample_rate_bps.clone();
            // Sample-rate poller — survives panics so a misbehaving
            // dynamic_config call can't silently freeze the audit
            // sampling rate at its last value.
            supervise_restart("audit_sample_rate_poller", move || {
                let dc = dc.clone();
                let bps = bps.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                    loop {
                        interval.tick().await;
                        let rate = dc.audit_sample_rate().await;
                        bps.store(
                            (rate.clamp(0.0, 1.0) * 10_000.0).round() as u32,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                }
            });
        }

        // Audit worker. This is the load-bearing pipeline for every
        // compliance event in the system; if it dies silently, ALL
        // audit entries get queued into the mpsc until the channel
        // fills, then `audit_log_dropped_total` starts incrementing
        // (an indirect signal at best). Wrap in supervise_restart so
        // a panic logs, bumps the metric, and respawns — the channel
        // receiver is moved in, so respawn requires reconstructing
        // the worker; we approximate that here by spawning a fresh
        // copy of the registry/db handles. The mpsc receiver itself
        // is single-consumer, so on restart the previously-spawned
        // worker will have already dropped it. Capture the receiver
        // inside an Arc<Mutex<Option<_>>> so the factory closure can
        // take ownership exactly once and subsequent restarts panic
        // cleanly (the audit pipeline is single-instance per process).
        {
            let ch = ch.clone();
            let db_w = db.clone();
            let reg_w = registry.clone();
            let rx_cell = Arc::new(Mutex::new(Some(rx)));
            supervise_restart("audit_worker", move || {
                let ch = ch.clone();
                let db_w = db_w.clone();
                let reg_w = reg_w.clone();
                let rx_cell = rx_cell.clone();
                async move {
                    let rx_opt = {
                        let mut guard = rx_cell.lock().await;
                        guard.take()
                    };
                    let Some(rx) = rx_opt else {
                        // After the first panic the receiver is gone
                        // — we can't reattach, so log and exit. The
                        // supervisor records this as a clean exit.
                        tracing::error!(
                            "audit_worker cannot restart: mpsc receiver consumed by prior \
                             incarnation; audit pipeline is offline until process restart"
                        );
                        return;
                    };
                    audit_worker(ch, rx, db_w, reg_w).await;
                }
            });
        }

        // Spawn periodic forwarder reload (every 10s)
        if let Some(pool) = &db {
            let reload_pool = pool.clone();
            let reload_reg = registry.clone();
            supervise_restart("audit_forwarder_reload", move || {
                let reload_pool = reload_pool.clone();
                let reload_reg = reload_reg.clone();
                async move {
                    reload_forwarders_loop(reload_pool, reload_reg).await;
                }
            });

            // Durable webhook redelivery — drains rows the inline
            // retry couldn't deliver. Same registry handle so it
            // sees forwarder config edits the operator makes via
            // the admin UI without a restart.
            let drain_pool = pool.clone();
            let drain_reg = registry.clone();
            supervise_restart("audit_webhook_outbox_drain", move || {
                let drain_pool = drain_pool.clone();
                let drain_reg = drain_reg.clone();
                async move {
                    webhook_outbox_drain_loop(drain_pool, drain_reg).await;
                }
            });
        }

        // Spawn a queue-depth sampler that updates the
        // `audit_log_queue_depth` gauge every second. Without this,
        // operators only see backlog after `audit_log_dropped_total`
        // has already started incrementing — by which point data
        // is already gone. The gauge gives a leading indicator that
        // ClickHouse / forwarders are getting behind.
        {
            let tx_for_gauge = tx.clone();
            supervise_restart("audit_queue_depth_gauge", move || {
                let tx_for_gauge = tx_for_gauge.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                    loop {
                        interval.tick().await;
                        // `tokio::sync::mpsc::Sender` doesn't expose
                        // current length directly; capacity() returns
                        // the REMAINING capacity, so depth = total - capacity.
                        let depth = AUDIT_CHANNEL_CAPACITY.saturating_sub(tx_for_gauge.capacity());
                        metrics::gauge!("audit_log_queue_depth").set(depth as f64);
                    }
                }
            });
        }

        Self {
            tx,
            db,
            registry,
            sample_rate_bps,
        }
    }

    /// Update the audit sample rate (0.0..=1.0). Lower values drop a
    /// proportional fraction of entries at `log()` time before the
    /// channel send, so a high-volume deployment can spare CH without
    /// also starving the forwarder queue. Called by the dynamic-config
    /// subscriber on startup and on every `audit.sample_rate` update.
    pub fn set_sample_rate(&self, rate: f64) {
        let clamped = rate.clamp(0.0, 1.0);
        let bps = (clamped * 10_000.0).round() as u32;
        self.sample_rate_bps
            .store(bps, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn log(&self, entry: AuditEntry) {
        // Sampling: consult the atomic rate and skip the entry when a
        // uniform draw falls outside the keep window. A full keep
        // (10000) short-circuits the RNG so the hot path pays nothing
        // until an operator actually dials sampling down.
        let bps = self
            .sample_rate_bps
            .load(std::sync::atomic::Ordering::Relaxed);
        if bps < 10_000 && rand::random_range(0..10_000) >= bps {
            metrics::counter!("audit_log_sampled_out_total").increment(1);
            return;
        }
        if let Err(e) = self.tx.try_send(entry) {
            // Compliance signal: dropped audit entries are a real
            // operational incident, not a debug warning. Bump the
            // metric on every drop so dashboards / alerts see the
            // true rate, but throttle the structured log to once
            // per second — at 10k req/s a sustained drop would
            // otherwise produce 10k error lines/sec and saturate
            // the log pipeline along with the audit pipeline.
            metrics::counter!("audit_log_dropped_total").increment(1);
            log_audit_drop_throttled(&e);
        }
    }

    /// Replace the URL check every webhook / Kafka delivery goes through.
    /// The server hands it the same validator it uses everywhere else.
    pub fn set_url_validator(&self, v: crate::validation::UrlValidator) {
        self.registry.set_url_check(v);
    }

    /// Force-reload forwarder configs from DB (called after CRUD ops).
    pub async fn reload_forwarders(&self) {
        if let Some(ref db) = self.db {
            reload_forwarders(db, &self.registry).await;
        }
    }
}

/// Periodically reload forwarder configs from the database.
async fn reload_forwarders_loop(db: PgPool, registry: ForwarderRegistry) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        reload_forwarders(&db, &registry).await;
    }
}

async fn reload_forwarders(db: &PgPool, registry: &ForwarderRegistry) {
    let rows = match sqlx::query_as::<_, LogForwarder>("SELECT * FROM log_forwarders")
        .fetch_all(db)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Failed to load log forwarders from DB: {e}");
            return;
        }
    };

    let mut map = HashMap::new();
    for row in rows {
        let udp_socket = if row.forwarder_type == "udp_syslog" && row.enabled {
            UdpSocket::bind("0.0.0.0:0").ok()
        } else {
            None
        };
        map.insert(
            row.id,
            ForwarderRuntime {
                config: row,
                udp_socket,
                tcp_stream: Arc::new(Mutex::new(None)),
            },
        );
    }

    let mut guard = registry.forwarders.write().await;
    *guard = map;
}

// ---------------------------------------------------------------------------
// Background worker
// ---------------------------------------------------------------------------

async fn audit_worker(
    ch: Option<clickhouse::Client>,
    mut rx: mpsc::Receiver<AuditEntry>,
    db: Option<PgPool>,
    registry: ForwarderRegistry,
) {
    // `redirect::Policy::none()` SSRF defense — same reasoning as
    // `drain_webhook_outbox_once`: webhook deliveries can be 302'd
    // into internal services if redirects are followed automatically.
    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default();

    // Separate batches per log type for routing to correct ClickHouse table
    let mut batches: HashMap<&'static str, Vec<AuditEntry>> = HashMap::new();
    let mut flush_interval = tokio::time::interval(std::time::Duration::from_secs(2));

    loop {
        tokio::select! {
            Some(entry) = rx.recv() => {
                // Forward to all enabled forwarders immediately
                forward_to_all(&http_client, &registry, &db, &entry).await;

                let table = entry.log_type.index_id();
                let batch = batches.entry(table).or_insert_with(|| Vec::with_capacity(64));
                batch.push(entry);
                if batch.len() >= 50 {
                    // `flush_to_clickhouse` now retains entries on
                    // error (capped at CH_RETAIN_CAP). Take the batch
                    // out, flush, and put back any entries the flush
                    // failed to deliver so the next tick retries
                    // them. Without the put-back, a CH outage at the
                    // size-trigger path would drop the entries even
                    // though the flush function preserved them.
                    let mut b = std::mem::take(batch);
                    flush_to_clickhouse(&ch, table, &mut b).await;
                    if !b.is_empty() {
                        // Prepend retained entries so arrival order
                        // is preserved against any new entries that
                        // accumulate before the next flush.
                        b.append(batch);
                        *batch = b;
                    }
                }
            }
            _ = flush_interval.tick() => {
                for (table, batch) in batches.iter_mut() {
                    if !batch.is_empty() {
                        flush_to_clickhouse(&ch, table, batch).await;
                    }
                }
            }
            else => break,
        }
    }
}

/// Forward a single entry to all enabled forwarders that match the log type.
///
/// `pub(super)` so the outbox alert path can self-emit a synthetic
/// audit entry through the same forwarder set without going via
/// `AuditLogger::log` (the drain loop has no channel handle).
pub(super) async fn forward_to_all(
    http_client: &reqwest::Client,
    registry: &ForwarderRegistry,
    db: &Option<PgPool>,
    entry: &AuditEntry,
) {
    let log_type_str = entry.log_type.as_str();
    let check = registry.url_check();
    let guard = registry.forwarders.read().await;
    for (id, runtime) in guard.iter() {
        if !runtime.config.enabled {
            continue;
        }
        // Only forward if the forwarder subscribes to this log type
        if !runtime.config.log_types.iter().any(|t| t == log_type_str) {
            continue;
        }
        let result = match runtime.config.forwarder_type.as_str() {
            "udp_syslog" => send_udp_syslog(runtime, entry),
            "tcp_syslog" => send_tcp_syslog(runtime, entry).await,
            "kafka" => send_kafka(http_client, &check, &runtime.config, entry).await,
            "webhook" => send_webhook(http_client, &check, &runtime.config, entry).await,
            other => {
                tracing::warn!("Unknown forwarder type: {other}");
                Err(format!("Unknown forwarder type: {other}"))
            }
        };

        // Update stats in DB (fire-and-forget)
        if let Some(pool) = &db {
            match result {
                Ok(()) => {
                    let _ = sqlx::query(
                        "UPDATE log_forwarders SET sent_count = sent_count + 1, last_sent_at = now(), updated_at = now() WHERE id = $1"
                    )
                    .bind(id)
                    .execute(pool)
                    .await;
                }
                Err(ref err_msg) => {
                    // Counter semantics:
                    //   - `sent_count`  = successful deliveries (inline or
                    //                     via outbox replay)
                    //   - `error_count` = PERMANENT failures (outbox replay
                    //                     exhausted retry budget)
                    // A transient inline failure that successfully enqueues
                    // for retry no longer bumps `error_count` here — that
                    // happens only when the drain loop gives up after
                    // MAX_OUTBOX_ATTEMPTS. Without this split, the same
                    // entry that fails inline AND then succeeds via outbox
                    // contributed +1 to both totals, so
                    // `sent_count + error_count > attempts` on any
                    // transient failure. `last_error` still updates so
                    // operators see the most recent failure message.
                    let _ = sqlx::query(
                        "UPDATE log_forwarders SET last_error = $2, updated_at = now() \
                         WHERE id = $1",
                    )
                    .bind(id)
                    .bind(err_msg)
                    .execute(pool)
                    .await;
                    // Park in outbox for the drain worker to retry through
                    // the right transport (the drain now dispatches by
                    // forwarder_type, matching the inline path). If the
                    // insert itself fails, the audit row is genuinely
                    // lost — bump `error_count` as the permanent-failure
                    // path so operators see SOMETHING.
                    if let Ok(payload_json) = serde_json::to_value(entry) {
                        let insert_result = sqlx::query(
                            "INSERT INTO webhook_outbox (forwarder_id, payload, last_error) \
                             VALUES ($1, $2, $3)",
                        )
                        .bind(id)
                        .bind(&payload_json)
                        .bind(err_msg)
                        .execute(pool)
                        .await;
                        if insert_result.is_ok() {
                            metrics::counter!(
                                "forwarder_deadletter_total",
                                "transport" => runtime.config.forwarder_type.clone(),
                            )
                            .increment(1);
                        } else {
                            // Outbox insert failed — this is the only path
                            // where the inline failure becomes permanent
                            // without the drain getting a chance.
                            let _ = sqlx::query(
                                "UPDATE log_forwarders SET error_count = error_count + 1 \
                                 WHERE id = $1",
                            )
                            .bind(id)
                            .execute(pool)
                            .await;
                        }
                    }
                }
            }
        }
    }
}

//! Resource admission shared by AI, MCP, authentication and audit dispatch.
//! Limits describe this process (including cgroups), never the host alone.
//! Byte reservations survive cancellation, shared buffers and audit retries.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[cfg(any(target_os = "linux", test))]
mod cgroup;
pub mod http;

const MIB: usize = 1024 * 1024;
// Compact JSON arrays can use many times their wire size as Value nodes. This
// also covers conversion/capture copies rather than budgeting raw bytes alone.
pub const BODY_HEADROOM: usize = 32;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct Policy {
    pub io_workers: usize,
    pub cpu_jobs: usize,
    pub argon_jobs: usize,
    pub body_bytes: usize,
    pub audit_bytes: usize,
    pub max_body_bytes: usize,
}

impl Policy {
    fn for_machine(cpus: usize, memory: u64) -> Self {
        let memory = usize::try_from(memory).unwrap_or(usize::MAX);
        let cpus = cpus.max(1);
        let body_bytes = (memory / 4).clamp(8 * MIB, 8 * 1024 * MIB);
        Self {
            io_workers: cpus.min(8),
            cpu_jobs: cpus.min((memory / (128 * MIB)).max(1)),
            // Argon2's 19 MiB workspace, allocator headroom, and at most 1/8
            // of the process allowance. CPU admission is shared with scanning.
            argon_jobs: cpus.min((memory / 8 / (32 * MIB)).max(1)),
            body_bytes,
            audit_bytes: (memory / 16).clamp(2 * MIB, 128 * MIB),
            max_body_bytes: (body_bytes / BODY_HEADROOM).min(64 * MIB),
        }
    }
}

pub fn policy() -> &'static Policy {
    static POLICY: OnceLock<Policy> = OnceLock::new();
    POLICY.get_or_init(|| {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let physical = match system.total_memory() {
            0 => 1 << 30,
            memory => memory,
        };
        let limit = system.cgroup_limits().map(|l| l.total_memory);
        #[cfg(target_os = "linux")]
        let limit = cgroup::memory_limit().or(limit);
        let memory = limit
            .filter(|&n| n > 0)
            .map_or(physical, |n| n.min(physical));
        Policy::for_machine(
            std::thread::available_parallelism().map_or(1, usize::from),
            memory,
        )
    })
}

struct Scheduler {
    slots: Arc<Semaphore>,
    inline: Semaphore,
}

impl Scheduler {
    fn new(policy: &Policy) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(policy.cpu_jobs)),
            inline: Semaphore::new(policy.io_workers.div_ceil(2)),
        }
    }

    async fn run<T>(&self, bytes: usize, f: impl FnOnce() -> T) -> T {
        let _slot = self.slots.clone().acquire_owned().await.unwrap();
        let inline = (bytes < MIB)
            .then(|| self.inline.try_acquire().ok())
            .flatten();
        let multi = tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread;
        if inline.is_some() || !multi {
            f()
        } else {
            tokio::task::block_in_place(f)
        }
    }
}

fn scheduler() -> &'static Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(|| Scheduler::new(policy()))
}

pub async fn cpu<T>(bytes: usize, f: impl FnOnce() -> T) -> T {
    scheduler().run(bytes, f).await
}

/// Acquire before spawning; cancellation of the caller must not release a
/// running password operation's CPU or memory admission.
pub async fn password_slot() -> (OwnedSemaphorePermit, OwnedSemaphorePermit) {
    static PASSWORDS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let memory = PASSWORDS.get_or_init(|| Arc::new(Semaphore::new(policy().argon_jobs)));
    let memory = memory.clone().acquire_owned().await.unwrap();
    let cpu = scheduler().slots.clone().acquire_owned().await.unwrap();
    (memory, cpu)
}

#[derive(Debug)]
pub struct ByteBudget {
    limit: usize,
    used: Mutex<usize>,
}

impl ByteBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: Mutex::new(0),
        })
    }

    pub fn reserve(self: &Arc<Self>) -> Reservation {
        Reservation {
            budget: self.clone(),
            charged: 0,
        }
    }

    pub fn used(&self) -> usize {
        *self.used.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug)]
pub struct Reservation {
    budget: Arc<ByteBudget>,
    charged: usize,
}

impl Reservation {
    pub fn grow(&mut self, bytes: usize) -> bool {
        let extra = bytes.saturating_sub(self.charged);
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        if extra > self.budget.limit.saturating_sub(*used) {
            return false;
        }
        *used += extra;
        self.charged += extra;
        true
    }

    pub fn wrap(self, data: Bytes) -> Bytes {
        Bytes::from_owner(Buffered {
            data,
            reservation: Arc::new(self),
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        *self.budget.used.lock().unwrap_or_else(|e| e.into_inner()) -= self.charged;
    }
}

struct Buffered {
    data: Bytes,
    reservation: Arc<Reservation>,
}

impl AsRef<[u8]> for Buffered {
    fn as_ref(&self) -> &[u8] {
        let _ = &self.reservation;
        &self.data
    }
}

pub fn body_budget() -> &'static Arc<ByteBudget> {
    static BUDGET: OnceLock<Arc<ByteBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| ByteBudget::new(policy().body_bytes))
}

pub fn audit_budget() -> &'static Arc<ByteBudget> {
    static BUDGET: OnceLock<Arc<ByteBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| ByteBudget::new(policy().audit_bytes))
}

/// A stream's retained timeline, shared with its detached post-invoke tail.
pub struct StreamBudget {
    reservation: Reservation,
    received: usize,
    max: usize,
}

impl StreamBudget {
    pub fn new(budget: &Arc<ByteBudget>, max: usize) -> Self {
        Self {
            reservation: budget.reserve(),
            received: 0,
            max,
        }
    }

    pub fn append(&mut self, bytes: usize) -> bool {
        let received = self.received.saturating_add(bytes);
        if received > self.max
            || !self
                .reservation
                .grow(received.saturating_mul(BODY_HEADROOM))
        {
            return false;
        }
        self.received = received;
        true
    }
}

pub type SharedStreamBudget = Arc<Mutex<StreamBudget>>;

pub fn stream_budget() -> SharedStreamBudget {
    Arc::new(Mutex::new(StreamBudget::new(
        body_budget(),
        policy().max_body_bytes,
    )))
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("The response exceeds the gateway buffering limit")]
    TooLarge,
    #[error("The gateway buffering budget is occupied; retry shortly")]
    Busy,
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
}

/// Charge before appending, including chunked and decompressed responses. Keep
/// room for JSON, conversion and allocator capacity; never wait holding a partial
/// body, since concurrent readers could otherwise deadlock each other.
pub async fn read_response(response: reqwest::Response, max: usize) -> Result<Bytes, ReadError> {
    let max = max.min(policy().max_body_bytes);
    read_response_with_budget(response, max, body_budget()).await
}

async fn read_response_with_budget(
    mut response: reqwest::Response,
    max: usize,
    budget: &Arc<ByteBudget>,
) -> Result<Bytes, ReadError> {
    if response.content_length().is_some_and(|n| n > max as u64) {
        return Err(ReadError::TooLarge);
    }
    let mut reservation = budget.reserve();
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let size = data
            .len()
            .checked_add(chunk.len())
            .ok_or(ReadError::TooLarge)?;
        if size > max {
            return Err(ReadError::TooLarge);
        }
        if !reservation.grow(size.saturating_mul(BODY_HEADROOM)) {
            metrics::counter!("gateway_memory_busy_total").increment(1);
            return Err(ReadError::Busy);
        }
        data.extend_from_slice(&chunk);
    }
    Ok(reservation.wrap(data.into()))
}

/// A transformed/captured body still owns the original read reservation.
pub fn retain(data: Bytes, source: Bytes) -> Bytes {
    struct Retained {
        data: Bytes,
        _source: Bytes,
    }
    impl AsRef<[u8]> for Retained {
        fn as_ref(&self) -> &[u8] {
            &self.data
        }
    }
    Bytes::from_owner(Retained {
        data,
        _source: source,
    })
}

/// Reserve before fetching a Redis cache entry. STRLEN followed by bounded
/// GETRANGE also stays bounded if another writer replaces the key in between.
pub async fn cached_body(redis: &fred::clients::Client, key: &str) -> Option<Bytes> {
    use fred::interfaces::KeysInterface;
    let length: usize = redis.strlen(key).await.ok()?;
    if length == 0 || length > policy().max_body_bytes {
        return None;
    }
    let mut lease = body_budget().reserve();
    if !lease.grow(length.saturating_mul(BODY_HEADROOM)) {
        return None;
    }
    let data: Vec<u8> = redis.getrange(key, 0, length).await.ok()?;
    if data.len() > length {
        return None;
    }
    Some(lease.wrap(data.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_limits_allocation_without_capping_a_well_provisioned_eighty_core_server() {
        let large = Policy::for_machine(80, 128 << 30);
        assert_eq!(
            (large.io_workers, large.cpu_jobs, large.argon_jobs),
            (8, 80, 80)
        );
        let small = Policy::for_machine(80, 256 << 20);
        assert_eq!((small.cpu_jobs, small.argon_jobs), (2, 1));
        assert!(small.body_bytes + small.audit_bytes <= 256 * MIB / 2);
    }

    #[test]
    fn reservations_follow_shared_slices_and_failed_growth_is_atomic() {
        let budget = ByteBudget::new(10);
        let mut lease = budget.reserve();
        assert!(lease.grow(8));
        assert!(!lease.grow(11));
        let data = lease.wrap(Bytes::from_static(b"hello"));
        let slice = data.slice(1..3);
        drop(data);
        assert_eq!(budget.used(), 8);
        drop(slice);
        assert_eq!(budget.used(), 0);
    }

    #[tokio::test]
    async fn chunked_responses_obey_size_and_shared_memory_limits() {
        async fn response() -> (reqwest::Response, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let router = axum::Router::new().route(
                "/",
                axum::routing::get(|| async {
                    axum::body::Body::from_stream(futures::stream::iter([
                        Ok::<_, std::io::Error>(Bytes::from_static(b"12345678")),
                        Ok(Bytes::from_static(b"abcdefgh")),
                    ]))
                }),
            );
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            (reqwest::get(url).await.unwrap(), server)
        }
        for (limit, max, expected) in [
            (10 * BODY_HEADROOM, 32, "busy"),
            (32 * BODY_HEADROOM, 12, "large"),
            (32 * BODY_HEADROOM, 32, "ok"),
        ] {
            let budget = ByteBudget::new(limit);
            let (response, server) = response().await;
            assert!(response.content_length().is_none());
            match read_response_with_budget(response, max, &budget).await {
                Err(ReadError::Busy) => assert_eq!(expected, "busy"),
                Err(ReadError::TooLarge) => assert_eq!(expected, "large"),
                Ok(bytes) => {
                    assert_eq!(expected, "ok");
                    assert_eq!(&bytes[..], b"12345678abcdefgh");
                    assert_eq!(budget.used(), 16 * BODY_HEADROOM);
                    let copy = bytes.clone();
                    drop(bytes);
                    assert_eq!(budget.used(), 16 * BODY_HEADROOM);
                    drop(copy);
                }
                Err(error) => panic!("{error}"),
            }
            assert_eq!(budget.used(), 0);
            server.abort();
        }
    }

    #[test]
    fn stream_timeline_reservations_cover_retained_history_and_reject_growth() {
        let budget = ByteBudget::new(25 * BODY_HEADROOM);
        let mut stream = StreamBudget::new(&budget, 64);
        assert!(stream.append(20));
        assert!(!stream.append(10));
        assert_eq!(budget.used(), 20 * BODY_HEADROOM);
        drop(stream);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn eighty_cpu_jobs_expand_beyond_io_workers_then_threads_retire() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;
        let alive = Arc::new(AtomicUsize::new(0));
        let (started, stopped) = (alive.clone(), alive.clone());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_keep_alive(Duration::from_millis(50))
            .on_thread_start(move || {
                started.fetch_add(1, Ordering::SeqCst);
            })
            .on_thread_stop(move || {
                stopped.fetch_sub(1, Ordering::SeqCst);
            })
            .enable_all()
            .build()
            .unwrap();
        let scheduler = Arc::new(Scheduler::new(&Policy {
            io_workers: 2,
            ..Policy::for_machine(80, 128 << 30)
        }));
        runtime.block_on(async {
            let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            let (began, mut beginnings) = tokio::sync::mpsc::unbounded_channel();
            let mut jobs = Vec::new();
            for _ in 0..80 {
                let (scheduler, release, began) =
                    (scheduler.clone(), release.clone(), began.clone());
                jobs.push(tokio::spawn(async move {
                    scheduler
                        .run(4096, || {
                            began.send(()).unwrap();
                            let guard = release.0.lock().unwrap();
                            let _guard = release
                                .1
                                .wait_timeout_while(guard, Duration::from_secs(10), |done| !*done)
                                .unwrap();
                        })
                        .await;
                }));
            }
            for _ in 0..80 {
                tokio::time::timeout(Duration::from_secs(5), beginnings.recv())
                    .await
                    .expect("all 80 calls must start")
                    .unwrap();
            }
            assert_eq!(scheduler.slots.available_permits(), 0);
            tokio::time::sleep(Duration::from_millis(1)).await;
            *release.0.lock().unwrap() = true;
            release.1.notify_all();
            for job in jobs {
                job.await.unwrap();
            }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive.load(Ordering::SeqCst) != 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(alive.load(Ordering::SeqCst), 2);
    }
}

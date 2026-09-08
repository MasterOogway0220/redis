#![doc = include_str!("../README.md")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use kvlite_api::{KvClock, KvOptions};
use kvlite_core::KvStore;
use kvlite_server::ServerConfig;
use tokio::runtime::{Builder, Runtime};

/// 2023-11-14T22:13:20Z. An arbitrary but plausible starting point, so anything
/// that formats a timestamp from the store looks like a date rather than 1970.
const DEFAULT_EPOCH_MILLIS: u64 = 1_700_000_000_000;

/// A clock that only moves when a test moves it.
///
/// This is the capability the whole test-double pitch rests on: asserting that a
/// 30-minute session has expired should take a microsecond, not 30 minutes, and it
/// should not depend on the machine being fast enough for a `sleep` to be accurate.
///
/// ```
/// use kvlite_testing::ManualClock;
/// use std::time::Duration;
///
/// let clock = ManualClock::new();
/// let start = clock.now_millis();
///
/// clock.advance(Duration::from_secs(90));
/// assert_eq!(clock.now_millis(), start + 90_000);
/// ```
#[derive(Debug)]
pub struct ManualClock {
    millis: AtomicU64,
}

impl ManualClock {
    /// A clock stopped at a fixed, plausible instant.
    #[must_use]
    pub fn new() -> Self {
        Self { millis: AtomicU64::new(DEFAULT_EPOCH_MILLIS) }
    }

    /// A clock stopped at a specific instant, in milliseconds since the epoch.
    #[must_use]
    pub fn starting_at(millis: u64) -> Self {
        Self { millis: AtomicU64::new(millis) }
    }

    /// The current instant, in milliseconds since the epoch.
    ///
    /// Inherent as well as on [`KvClock`], so reading the clock in a test does not
    /// mean importing a trait.
    #[must_use]
    pub fn now_millis(&self) -> u64 {
        self.millis.load(Ordering::SeqCst)
    }

    /// Moves time forward.
    pub fn advance(&self, by: Duration) {
        let millis = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.millis.fetch_add(millis, Ordering::SeqCst);
    }

    /// Moves time to an absolute instant, in milliseconds since the epoch.
    ///
    /// Time may be moved backwards. Deadlines are absolute, so keys that had
    /// expired become live again — useful for a test, and a thing the system clock
    /// can also do to you.
    pub fn set(&self, millis: u64) {
        self.millis.store(millis, Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl KvClock for ManualClock {
    fn now_millis(&self) -> u64 {
        Self::now_millis(self)
    }
}

/// A Kvlite server for one test, on an ephemeral port, with a clock you control.
///
/// Dropping it stops the listener and every connection it accepted — including when
/// a test panics mid-assertion, because `Drop` still runs while unwinding.
///
/// **Time does not pass on its own.** The clock starts stopped, so a key with a TTL
/// stays alive until [`Server::clock`] is advanced. That is deliberate: a test that
/// depends on wall-clock time is a test that fails on a loaded CI machine.
///
/// The server runs on a runtime of its own, so it does not matter whether your test
/// is async, what runtime flavour it uses, or whether your Redis client is blocking.
/// Starting one needs no runtime at all.
pub struct Server {
    inner: Option<kvlite_server::Server>,
    runtime: Option<Runtime>,
    clock: Arc<ManualClock>,
}

impl Server {
    /// Starts a server on an ephemeral loopback port.
    ///
    /// Synchronous, and usable from a plain `#[test]`, from any flavour of async
    /// test, and with a blocking or an async client.
    ///
    /// # Panics
    /// If no loopback port can be bound at all, which means something is wrong with
    /// the machine rather than with the test. Use [`Server::try_start`] to handle it.
    #[must_use]
    pub fn start() -> Self {
        Self::try_start().expect("failed to start an in-process Kvlite server")
    }

    /// Starts a server on an ephemeral loopback port, reporting failures.
    ///
    /// # Errors
    /// Whatever building the runtime or binding the listener reports.
    pub fn try_start() -> std::io::Result<Self> {
        // The fixture owns its runtime rather than borrowing the caller's, and that
        // is the whole point. A blocking Redis client called from `#[tokio::test]` —
        // which is single-threaded by default — would otherwise starve the thread
        // the accept loop lives on, and the test would hang with no error at all.
        // One worker is plenty for a test double.
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("kvlite-testing")
            .enable_all()
            .build()?;

        let clock = Arc::new(ManualClock::new());

        let mut options = KvOptions::default();
        options.clock = Some(Arc::clone(&clock) as Arc<dyn KvClock>);
        let store = Arc::new(KvStore::with_options(options));

        let mut config = ServerConfig::default();
        config.bind = SocketAddr::from(([127, 0, 0, 1], 0));
        // Nothing here lives long enough to need reclaiming, and a timer would only
        // add a task that has to be torn down.
        config.expiry_sweep_interval = None;

        // Bind on our own runtime and wait for the answer. This blocks the caller for
        // about a millisecond, and works whether or not the caller has a runtime.
        let (sender, receiver) = mpsc::sync_channel(1);
        runtime.spawn(async move {
            let _ = sender.send(kvlite_server::Server::bind_with_store(config, store).await);
        });
        let inner = receiver.recv().expect("the bind task panicked")?;

        Ok(Self { inner: Some(inner), runtime: Some(runtime), clock })
    }

    fn inner(&self) -> &kvlite_server::Server {
        self.inner.as_ref().expect("the server is only taken during drop")
    }

    /// The address the server actually bound.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.inner().local_addr()
    }

    /// The ephemeral port the server bound.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr().port()
    }

    /// A `redis://` URL to hand to a client library.
    #[must_use]
    pub fn url(&self) -> String {
        self.inner().url()
    }

    /// The clock driving expiry. Advance it to make TTLs fire.
    #[must_use]
    pub fn clock(&self) -> &Arc<ManualClock> {
        &self.clock
    }

    /// The store behind the server, for seeding or asserting without going over
    /// the wire.
    #[must_use]
    pub fn store(&self) -> &Arc<KvStore> {
        self.inner().store()
    }

    /// Empties every keyspace **without restarting**, which is what makes
    /// per-test isolation cheap enough to do in every test.
    ///
    /// Open connections stay open. Their selected keyspace and subscriptions are
    /// untouched, exactly as `FLUSHALL` would leave them.
    pub fn reset(&self) {
        self.store().clear();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Drop the server first, so its tasks are signalled and aborted while the
        // runtime they live on is still alive.
        self.inner.take();

        if let Some(runtime) = self.runtime.take() {
            // Not a plain drop: dropping a runtime waits for its threads, which
            // panics when it happens inside another runtime — which is exactly where
            // an async test drops us.
            runtime.shutdown_background();
        }
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("addr", &self.addr())
            .field("now_millis", &self.clock.now_millis())
            .finish_non_exhaustive()
    }
}

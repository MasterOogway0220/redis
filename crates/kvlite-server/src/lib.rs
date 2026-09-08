#![doc = include_str!("../README.md")]

mod command;
mod connection;
mod glob;
mod pubsub;
mod state;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kvlite_core::KvStore;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::state::ServerState;

/// How to start a server.
///
/// Fields are public but the struct is `#[non_exhaustive]`, so start from
/// [`ServerConfig::default`] and adjust what you need.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ServerConfig {
    /// Address to listen on. Port 0 binds an ephemeral port, which is what a test
    /// wants: [`Server::local_addr`] then reports the one that was chosen.
    pub bind: SocketAddr,

    /// How often to sweep expired keys, or `None` to leave expiry entirely lazy.
    ///
    /// Correctness never depends on this. It exists so a long-running server gets
    /// memory back from keys nobody reads.
    pub expiry_sweep_interval: Option<Duration>,

    /// Keys sampled per keyspace per sweep. Bounds the pause.
    pub expiry_sample_size: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // Loopback, and not 6379: a server that grabs the standard Redis port by
            // default will one day be started next to a real Redis and win the race.
            bind: SocketAddr::from(([127, 0, 0, 1], 6380)),
            expiry_sweep_interval: Some(Duration::from_millis(100)),
            expiry_sample_size: 20,
        }
    }
}

/// A running Kvlite server.
///
/// Dropping it stops the listener and every connection it accepted. Nothing is
/// left behind: no leaked task, no held port. Use [`Server::shutdown`] instead when
/// you want to wait for that to finish.
///
/// ```
/// # use kvlite_server::{Server, ServerConfig};
/// # use std::net::SocketAddr;
/// # async fn example() -> std::io::Result<()> {
/// let mut config = ServerConfig::default();
/// config.bind = SocketAddr::from(([127, 0, 0, 1], 0));   // ephemeral port
///
/// let server = Server::bind(config).await?;
/// println!("listening on {}", server.local_addr());
///
/// server.shutdown().await;
/// # Ok(())
/// # }
/// ```
pub struct Server {
    local_addr: SocketAddr,
    store: Arc<KvStore>,
    shutdown: watch::Sender<bool>,
    accept: Option<JoinHandle<()>>,
    sweeper: Option<JoinHandle<()>>,
}

impl Server {
    /// Binds and starts accepting, on a store of its own.
    ///
    /// # Errors
    /// Whatever [`TcpListener::bind`] reports — the address is in use, or not one
    /// this process may bind.
    pub async fn bind(config: ServerConfig) -> io::Result<Self> {
        Self::bind_with_store(config, Arc::new(KvStore::new())).await
    }

    /// Binds and starts accepting on an existing store.
    ///
    /// Pass a store built with your own clock to make expiry testable, or share one
    /// store between a server and the embedded API in the same process.
    ///
    /// # Errors
    /// Whatever [`TcpListener::bind`] reports.
    pub async fn bind_with_store(config: ServerConfig, store: Arc<KvStore>) -> io::Result<Self> {
        let listener = TcpListener::bind(config.bind).await?;
        let local_addr = listener.local_addr()?;

        let state = Arc::new(ServerState::new(Arc::clone(&store)));
        let (shutdown, receiver) = watch::channel(false);

        let accept = tokio::spawn(accept_loop(listener, Arc::clone(&state), receiver.clone()));

        let sweeper = config.expiry_sweep_interval.map(|interval| {
            tokio::spawn(sweep_loop(
                Arc::clone(&store),
                interval,
                config.expiry_sample_size,
                receiver,
            ))
        });

        Ok(Self { local_addr, store, shutdown, accept: Some(accept), sweeper })
    }

    /// The address actually bound, which is how you learn an ephemeral port.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// A `redis://` URL for this server, for handing to a client library.
    #[must_use]
    pub fn url(&self) -> String {
        format!("redis://{}", self.local_addr)
    }

    /// The store behind the server. Reach through it to inspect or seed state
    /// without going over the wire.
    #[must_use]
    pub fn store(&self) -> &Arc<KvStore> {
        &self.store
    }

    /// Stops the server and waits for its tasks to finish.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        for handle in [self.accept.take(), self.sweeper.take()].into_iter().flatten() {
            let _ = handle.await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Signal first so connection tasks unwind on their own, then abort the two
        // tasks we own outright. A test that panics mid-assertion still leaves a
        // clean process behind.
        let _ = self.shutdown.send(true);
        for handle in [self.accept.as_ref(), self.sweeper.as_ref()].into_iter().flatten() {
            handle.abort();
        }
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("local_addr", &self.local_addr).finish_non_exhaustive()
    }
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<ServerState>,
    shutdown: watch::Receiver<bool>,
) {
    let mut signal = shutdown.clone();
    loop {
        tokio::select! {
            biased;

            _ = signal.changed() => return,

            accepted = listener.accept() => match accepted {
                Ok((socket, peer)) => {
                    tokio::spawn(connection::serve(
                        socket,
                        peer,
                        Arc::clone(&state),
                        shutdown.clone(),
                    ));
                }
                Err(_) => {
                    // Per-connection accept failures (a peer that reset between the
                    // SYN and our accept, or a momentary fd shortage) are not fatal.
                    // Yield so a persistent failure cannot become a busy loop.
                    tokio::task::yield_now().await;
                }
            },
        }
    }
}

async fn sweep_loop(
    store: Arc<KvStore>,
    interval: Duration,
    sample_size: usize,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            _ = ticker.tick() => {
                store.sweep_expired(sample_size);
            }
        }
    }
}

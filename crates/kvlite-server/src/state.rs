use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use kvlite_core::KvStore;
use kvlite_resp::{Frame, RespProtocol};
use tokio::sync::mpsc::UnboundedSender;

use crate::pubsub::{ConnectionId, PubSub};

/// Everything shared by every connection.
pub(crate) struct ServerState {
    pub store: Arc<KvStore>,
    pub pubsub: PubSub,
    pub started: Instant,
    next_connection_id: AtomicU64,
}

impl ServerState {
    pub(crate) fn new(store: Arc<KvStore>) -> Self {
        Self {
            store,
            pubsub: PubSub::default(),
            started: Instant::now(),
            next_connection_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn next_connection_id(&self) -> ConnectionId {
        self.next_connection_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Per-connection state. One of these lives in each connection task.
pub(crate) struct Session {
    pub id: ConnectionId,
    pub peer: SocketAddr,
    pub keyspace: usize,
    pub protocol: RespProtocol,
    pub name: Vec<u8>,
    pub library: Vec<u8>,
    /// Where pub/sub deliveries for this connection are queued.
    pub outbox: UnboundedSender<Frame>,
    // BTreeSet rather than HashSet so UNSUBSCRIBE with no arguments produces a
    // stable, testable order.
    pub channels: BTreeSet<Vec<u8>>,
    pub patterns: BTreeSet<Vec<u8>>,
}

impl Session {
    pub(crate) fn new(id: ConnectionId, peer: SocketAddr, outbox: UnboundedSender<Frame>) -> Self {
        Self {
            id,
            peer,
            keyspace: 0,
            protocol: RespProtocol::Resp2,
            name: Vec::new(),
            library: Vec::new(),
            outbox,
            channels: BTreeSet::new(),
            patterns: BTreeSet::new(),
        }
    }

    pub(crate) fn subscription_count(&self) -> usize {
        self.channels.len() + self.patterns.len()
    }

    pub(crate) fn is_subscribed(&self) -> bool {
        self.subscription_count() > 0
    }
}

//! Channel and pattern subscriptions.
//!
//! Every subscriber owns an unbounded queue that the publishing task pushes into,
//! so a slow consumer never blocks a `PUBLISH`. That is the same trade Redis makes,
//! and it has the same consequence: a subscriber that never reads grows its queue.
//! Bounding it is a Phase 2 concern, alongside `client-output-buffer-limit`.

use std::collections::HashMap;
use std::sync::Mutex;

use kvlite_resp::Frame;
use tokio::sync::mpsc::UnboundedSender;

use crate::glob;

/// Identifies one connection for the lifetime of that connection.
pub(crate) type ConnectionId = u64;

#[derive(Default)]
struct Registry {
    /// channel -> subscribers
    channels: HashMap<Vec<u8>, HashMap<ConnectionId, UnboundedSender<Frame>>>,
    /// pattern -> subscribers
    patterns: HashMap<Vec<u8>, HashMap<ConnectionId, UnboundedSender<Frame>>>,
}

/// The server's subscription registry.
#[derive(Default)]
pub(crate) struct PubSub {
    registry: Mutex<Registry>,
}

impl PubSub {
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn subscribe(
        &self,
        id: ConnectionId,
        channel: &[u8],
        outbox: UnboundedSender<Frame>,
    ) {
        self.lock().channels.entry(channel.to_vec()).or_default().insert(id, outbox);
    }

    pub(crate) fn subscribe_pattern(
        &self,
        id: ConnectionId,
        pattern: &[u8],
        outbox: UnboundedSender<Frame>,
    ) {
        self.lock().patterns.entry(pattern.to_vec()).or_default().insert(id, outbox);
    }

    pub(crate) fn unsubscribe(&self, id: ConnectionId, channel: &[u8]) {
        let mut registry = self.lock();
        if let Some(subscribers) = registry.channels.get_mut(channel) {
            subscribers.remove(&id);
            if subscribers.is_empty() {
                registry.channels.remove(channel);
            }
        }
    }

    pub(crate) fn unsubscribe_pattern(&self, id: ConnectionId, pattern: &[u8]) {
        let mut registry = self.lock();
        if let Some(subscribers) = registry.patterns.get_mut(pattern) {
            subscribers.remove(&id);
            if subscribers.is_empty() {
                registry.patterns.remove(pattern);
            }
        }
    }

    /// Drops every subscription held by a connection that is going away.
    pub(crate) fn disconnect(&self, id: ConnectionId) {
        let mut registry = self.lock();
        registry.channels.retain(|_, subscribers| {
            subscribers.remove(&id);
            !subscribers.is_empty()
        });
        registry.patterns.retain(|_, subscribers| {
            subscribers.remove(&id);
            !subscribers.is_empty()
        });
    }

    /// Delivers a message and returns how many subscribers received it.
    ///
    /// A connection subscribed both directly and by a matching pattern receives the
    /// message twice and counts twice, which is what Redis does.
    pub(crate) fn publish(&self, channel: &[u8], payload: &[u8]) -> u64 {
        let registry = self.lock();
        let mut delivered = 0;

        if let Some(subscribers) = registry.channels.get(channel) {
            let message = Frame::Push(vec![
                Frame::bulk("message"),
                Frame::bulk(channel),
                Frame::bulk(payload),
            ]);
            for outbox in subscribers.values() {
                if outbox.send(message.clone()).is_ok() {
                    delivered += 1;
                }
            }
        }

        for (pattern, subscribers) in &registry.patterns {
            if !glob::matches(pattern, channel) {
                continue;
            }
            let message = Frame::Push(vec![
                Frame::bulk("pmessage"),
                Frame::bulk(pattern),
                Frame::bulk(channel),
                Frame::bulk(payload),
            ]);
            for outbox in subscribers.values() {
                if outbox.send(message.clone()).is_ok() {
                    delivered += 1;
                }
            }
        }

        delivered
    }

    /// Channels with at least one subscriber, for `PUBSUB CHANNELS`.
    pub(crate) fn active_channels(&self, pattern: Option<&[u8]>) -> Vec<Vec<u8>> {
        self.lock()
            .channels
            .keys()
            .filter(|channel| pattern.is_none_or(|p| glob::matches(p, channel)))
            .cloned()
            .collect()
    }
}

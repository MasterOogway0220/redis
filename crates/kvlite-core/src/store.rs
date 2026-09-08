use std::sync::Arc;
use std::time::Duration;

use kvlite_api::{KvClock, KvOptions, KvResult};

use crate::{KvKeyspace, SystemClock};

/// A Kvlite instance: a fixed set of numbered keyspaces sharing one clock.
///
/// Instances are fully independent — there is no static mutable state — so a test
/// suite can create as many as it likes. `KvStore` is `Send + Sync`; share it with an
/// `Arc` rather than cloning it, because a clone would be a second, separate store.
///
/// The bare methods ([`KvStore::set`], [`KvStore::get`] and friends) act on keyspace
/// 0, which is what an embedded user almost always wants. Reach for
/// [`KvStore::keyspace`] when you want the rest.
///
/// ```
/// use kvlite_core::KvStore;
/// use std::time::Duration;
///
/// let store = KvStore::new();
/// store.set("greeting", "hello", Some(Duration::from_secs(60)));
///
/// assert_eq!(store.get_str("greeting")?.as_deref(), Some("hello"));
/// # Ok::<(), kvlite_api::KvError>(())
/// ```
pub struct KvStore {
    keyspaces: Vec<KvKeyspace>,
    clock: Arc<dyn KvClock>,
}

impl KvStore {
    /// A store with the default options: 16 keyspaces on the system clock.
    ///
    /// Nothing is allocated eagerly and no thread is spawned, so construction is
    /// cheap enough for a short-lived process.
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(KvOptions::default())
    }

    /// A store with custom options.
    ///
    /// # Panics
    /// When `options.keyspace_count` is zero. A store with no keyspaces cannot
    /// answer any command, so this is a programming error rather than a runtime one.
    #[must_use]
    pub fn with_options(options: KvOptions) -> Self {
        assert!(options.keyspace_count > 0, "a store needs at least one keyspace");

        let clock: Arc<dyn KvClock> = options.clock.unwrap_or_else(|| Arc::new(SystemClock));
        let keyspaces = (0..options.keyspace_count)
            .map(|index| KvKeyspace::new(index, Arc::clone(&clock)))
            .collect();

        Self { keyspaces, clock }
    }

    /// Number of keyspaces, fixed at construction.
    #[must_use]
    pub fn keyspace_count(&self) -> usize {
        self.keyspaces.len()
    }

    /// The keyspace at `index`, or `None` when the index is outside the store.
    #[must_use]
    pub fn keyspace(&self, index: usize) -> Option<&KvKeyspace> {
        self.keyspaces.get(index)
    }

    /// Keyspace 0, which every bare method on this type uses.
    #[must_use]
    pub fn default_keyspace(&self) -> &KvKeyspace {
        &self.keyspaces[0]
    }

    /// The clock driving expiry.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn KvClock> {
        &self.clock
    }

    /// Removes every key in every keyspace, as Redis `FLUSHALL` does.
    pub fn clear(&self) {
        for keyspace in &self.keyspaces {
            keyspace.clear();
        }
    }

    /// Sweeps expired keys from every keyspace and returns how many were dropped.
    ///
    /// Correctness never depends on this — expiry is lazy, so an expired key is
    /// already invisible. Call it on a timer if you want the memory back from keys
    /// nobody reads.
    pub fn sweep_expired(&self, sample_size: usize) -> usize {
        self.keyspaces.iter().map(|keyspace| keyspace.sweep_expired(sample_size)).sum()
    }

    // ---- keyspace 0 conveniences -----------------------------------------

    /// Stores a string in keyspace 0, replacing whatever was there.
    pub fn set(
        &self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
        time_to_live: Option<Duration>,
    ) -> bool {
        self.default_keyspace().set(
            key.as_ref(),
            value.as_ref(),
            time_to_live,
            kvlite_api::KvWriteCondition::Always,
            false,
        )
    }

    /// Reads a string from keyspace 0.
    ///
    /// # Errors
    /// [`kvlite_api::KvError::WrongType`] when the key holds something else.
    pub fn get(&self, key: impl AsRef<[u8]>) -> KvResult<Option<Vec<u8>>> {
        self.default_keyspace().get(key.as_ref())
    }

    /// Reads a string from keyspace 0 as text, replacing invalid UTF-8.
    ///
    /// # Errors
    /// [`kvlite_api::KvError::WrongType`] when the key holds something else.
    pub fn get_str(&self, key: impl AsRef<[u8]>) -> KvResult<Option<String>> {
        Ok(self.get(key)?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Adds `delta` to a counter in keyspace 0 and returns the result.
    ///
    /// # Errors
    /// [`kvlite_api::KvError::NotAnInteger`], [`kvlite_api::KvError::OutOfRange`] or
    /// [`kvlite_api::KvError::WrongType`].
    pub fn incr_by(&self, key: impl AsRef<[u8]>, delta: i64) -> KvResult<i64> {
        self.default_keyspace().incr_by(key.as_ref(), delta)
    }

    /// Removes a key from keyspace 0. Returns whether it existed.
    pub fn remove(&self, key: impl AsRef<[u8]>) -> bool {
        self.default_keyspace().remove(key.as_ref())
    }

    /// Whether a key exists in keyspace 0 and has not expired.
    #[must_use]
    pub fn contains(&self, key: impl AsRef<[u8]>) -> bool {
        self.default_keyspace().contains(key.as_ref())
    }

    /// Sets or clears a key's lifetime in keyspace 0. Returns whether it existed.
    pub fn expire(&self, key: impl AsRef<[u8]>, time_to_live: Option<Duration>) -> bool {
        self.default_keyspace().expire(key.as_ref(), time_to_live)
    }

    /// Remaining lifetime of a key in keyspace 0.
    #[must_use]
    pub fn time_to_live(&self, key: impl AsRef<[u8]>) -> Option<Duration> {
        self.default_keyspace().time_to_live(key.as_ref())
    }
}

impl Default for KvStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for KvStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvStore")
            .field("keyspace_count", &self.keyspaces.len())
            .finish_non_exhaustive()
    }
}

impl kvlite_api::Store for KvStore {
    fn keyspace_count(&self) -> usize {
        Self::keyspace_count(self)
    }

    fn keyspace(&self, index: usize) -> Option<&dyn kvlite_api::Keyspace> {
        Self::keyspace(self, index).map(|keyspace| keyspace as &dyn kvlite_api::Keyspace)
    }
}

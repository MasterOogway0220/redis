use alloc::vec::Vec;
use core::time::Duration;

use crate::{KvResult, KvValueType, KvWriteCondition};

/// One logical database: key lifetime management plus string and counter operations.
///
/// This contract is deliberately narrow. It carries what a library built *on top of*
/// Kvlite needs — key lifetime, binary strings, counters — so that library can depend
/// on `kvlite-api` alone and leave the choice of implementation to its consumers.
///
/// Lists and hashes are on the concrete engine type in `kvlite-core` and over the wire
/// protocol. They are not here because nothing has yet needed them through an
/// abstraction, and every public symbol is a permanent commitment.
///
/// Keys and values are binary-safe: they are byte strings, not UTF-8.
///
/// Methods take `&self`. Implementations are expected to be `Sync` and to handle their
/// own interior mutability, because a store is shared, not owned by one caller.
pub trait Keyspace {
    /// Zero-based index of this keyspace within its store.
    fn index(&self) -> usize;

    /// Number of live keys, excluding keys that have already expired.
    fn len(&self) -> u64;

    /// Whether the keyspace holds no live keys.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the key exists and has not expired.
    fn contains(&self, key: &[u8]) -> bool;

    /// Removes the key. Returns whether it existed.
    fn remove(&self, key: &[u8]) -> bool;

    /// Sets or clears the key's time to live. Returns whether the key existed.
    ///
    /// `None` makes an existing key persistent.
    fn expire(&self, key: &[u8], time_to_live: Option<Duration>) -> bool;

    /// Remaining lifetime of the key.
    ///
    /// `None` covers both "no such key" and "key has no expiry"; use [`Keyspace::contains`]
    /// to tell them apart, exactly as Redis makes you distinguish `-2` from `-1`.
    fn time_to_live(&self, key: &[u8]) -> Option<Duration>;

    /// Type of the value stored at the key.
    fn kind(&self, key: &[u8]) -> KvValueType;

    /// Removes every key in this keyspace.
    fn clear(&self);

    /// Stores a binary-safe string. Returns whether the write was applied.
    ///
    /// A write is skipped, and `false` returned, only when `condition` is not met.
    /// Setting a key that holds another type replaces it, as Redis `SET` does.
    fn set(
        &self,
        key: &[u8],
        value: &[u8],
        time_to_live: Option<Duration>,
        condition: KvWriteCondition,
        keep_time_to_live: bool,
    ) -> bool;

    /// Reads a binary-safe string, or `None` when the key is missing.
    ///
    /// # Errors
    /// [`crate::KvError::WrongType`] when the key holds a value that is not a string.
    fn get(&self, key: &[u8]) -> KvResult<Option<Vec<u8>>>;

    /// Adds `delta` to the integer at the key and returns the result.
    ///
    /// A missing key is treated as zero, as Redis `INCRBY` does.
    ///
    /// # Errors
    /// [`crate::KvError::NotAnInteger`] when the stored value is not a 64-bit integer,
    /// [`crate::KvError::OutOfRange`] when the result would overflow,
    /// [`crate::KvError::WrongType`] when the key holds a value that is not a string.
    fn incr_by(&self, key: &[u8], delta: i64) -> KvResult<i64>;

    /// Length in bytes of the string at the key, or 0 when it is missing.
    ///
    /// # Errors
    /// [`crate::KvError::WrongType`] when the key holds a value that is not a string.
    fn strlen(&self, key: &[u8]) -> KvResult<u64>;
}

/// A Kvlite instance: a fixed set of numbered keyspaces sharing one engine.
///
/// Instances are fully independent — there is no static mutable state — so a test
/// suite can create as many as it likes.
pub trait Store {
    /// Number of keyspaces, fixed at construction.
    fn keyspace_count(&self) -> usize;

    /// The keyspace at `index`, or `None` when the index is outside the store.
    fn keyspace(&self, index: usize) -> Option<&dyn Keyspace>;
}

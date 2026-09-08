use alloc::sync::Arc;

use crate::KvClock;

/// Construction-time options for a Kvlite engine.
///
/// Fields are public but the struct is `#[non_exhaustive]`, so start from
/// [`KvOptions::default`] and adjust what you need. New options can then be added
/// without a breaking change.
///
/// ```
/// # use kvlite_api::KvOptions;
/// let mut options = KvOptions::default();
/// options.keyspace_count = 4;
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct KvOptions {
    /// Number of numbered keyspaces, as Redis `SELECT` databases. Defaults to 16.
    ///
    /// Must be at least 1; the engine panics at construction otherwise.
    pub keyspace_count: usize,

    /// Clock used for expiry. `None` means the system clock.
    ///
    /// Supply your own to make TTL behaviour testable without sleeping.
    pub clock: Option<Arc<dyn KvClock>>,
}

impl Default for KvOptions {
    fn default() -> Self {
        Self { keyspace_count: 16, clock: None }
    }
}

impl core::fmt::Debug for KvOptions {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `dyn KvClock` is not Debug, and requiring it of implementors would be a
        // tax on every consumer for the sake of one line here.
        f.debug_struct("KvOptions")
            .field("keyspace_count", &self.keyspace_count)
            .field("clock", &self.clock.as_ref().map(|_| "<custom>"))
            .finish()
    }
}

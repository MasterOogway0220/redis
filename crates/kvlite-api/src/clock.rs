/// Source of wall-clock time for key expiry.
///
/// Injecting the clock is what lets a test advance time and assert TTL expiry
/// without sleeping. Redis cannot do this, and it is a capability advantage
/// worth advertising.
///
/// Milliseconds since the Unix epoch, rather than a `std::time` type, so this
/// crate stays `no_std`.
pub trait KvClock: Send + Sync + 'static {
    /// Milliseconds elapsed since 1970-01-01T00:00:00Z.
    fn now_millis(&self) -> u64;
}

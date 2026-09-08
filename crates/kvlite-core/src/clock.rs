use std::time::{SystemTime, UNIX_EPOCH};

use kvlite_api::KvClock;

/// The wall clock, which is what a store uses unless you give it something else.
///
/// A clock that runs backwards, or a system clock set before 1970, reports 0 rather
/// than panicking. Expiry deadlines are absolute, so a backwards jump makes keys
/// live longer than asked — the same thing that happens to Redis, and better than
/// taking the process down.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl KvClock for SystemClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
            .unwrap_or(0)
    }
}

#![doc = include_str!("../README.md")]

mod clock;
mod int;
mod keyspace;
mod score;
mod sorted_set;
mod store;

pub use clock::SystemClock;
pub use int::{format_int, parse_int};
pub use keyspace::KvKeyspace;
pub use score::{format_score, parse_score};
pub use store::KvStore;

// Deliberate re-export, and a documented one. This crate implements `kvlite-api`'s
// traits, so a consumer needs its types to name ours. `kvlite-api` is dependency-free
// and versioned in lockstep with this crate, which is what makes the re-export safe
// — see kvlite-packaging.md section 5 on not leaking dependency types.
pub use kvlite_api::{Keyspace, Store};
pub use kvlite_api::{KvClock, KvError, KvOptions, KvResult, KvValueType, KvWriteCondition};

#![no_std]
#![doc = include_str!("../README.md")]

extern crate alloc;

// The test harness needs std even though the crate itself does not.
#[cfg(test)]
extern crate std;

mod clock;
mod error;
mod options;
mod store;
mod value;

pub use clock::KvClock;
pub use error::{KvError, KvResult};
pub use options::KvOptions;
pub use store::{Keyspace, Store};
pub use value::{KvValueType, KvWriteCondition};

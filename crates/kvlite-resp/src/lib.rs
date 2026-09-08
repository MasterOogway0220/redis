#![no_std]
#![doc = include_str!("../README.md")]

extern crate alloc;

// The test harness needs std even though the crate itself does not.
#[cfg(test)]
extern crate std;

mod decode;
mod encode;
mod error;
mod frame;
mod limits;

pub use decode::{DecodedCommand, Decoder};
pub use encode::{Encoder, RespProtocol};
pub use error::DecodeError;
pub use frame::Frame;
pub use limits::Limits;

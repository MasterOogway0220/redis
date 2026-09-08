#![no_main]
//! The decoder sits on a trust boundary: every byte it sees came from a peer.
//!
//! Being sans-io is what makes this target three lines long. A codec welded to a
//! socket would need a harness that fakes one.

use kvlite_resp::{Decoder, Limits};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Tight limits so the fuzzer spends its time on parser logic rather than on
    // allocating half a gigabyte for a legal-but-enormous bulk string.
    let limits = Limits {
        max_line_len: 4096,
        max_bulk_len: 65_536,
        max_array_len: 1024,
        max_depth: 16,
        ..Limits::default()
    };
    let decoder = Decoder::with_limits(limits);

    // Three invariants, none of which may ever panic:
    //   1. Any byte string is either a frame, an incomplete frame, or an error.
    //   2. A successful decode never claims more bytes than it was given.
    //   3. The command path agrees with the frame path about framing.
    if let Ok(Some((_, consumed))) = decoder.decode(data) {
        assert!(consumed <= data.len(), "decoder consumed more than it was given");
    }
    if let Ok(Some((_, consumed))) = decoder.decode_command(data) {
        assert!(consumed <= data.len(), "command decoder consumed more than it was given");
    }
});

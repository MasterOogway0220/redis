#![no_main]
//! Anything the decoder accepts must survive re-encoding unchanged.
//!
//! This is the property that keeps a proxy honest: decode a frame from upstream,
//! encode it downstream, and the peer must see the same value. A round trip that
//! loses information is a bug even when neither half panics.

use kvlite_resp::{Decoder, Encoder, RespProtocol};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let decoder = Decoder::new();
    let Ok(Some((frame, consumed))) = decoder.decode(data) else {
        return;
    };

    // RESP3 is the lossless direction. RESP2 deliberately downgrades typed frames,
    // so it is not a round trip and is not asserted here.
    let mut encoded = Vec::new();
    Encoder::new(RespProtocol::Resp3).encode(&frame, &mut encoded);

    let (reparsed, reconsumed) = decoder
        .decode(&encoded)
        .expect("our own output must be decodable")
        .expect("our own output must be complete");

    assert_eq!(reconsumed, encoded.len(), "re-encoding left trailing bytes");

    // Doubles are the one exception: NaN is not equal to itself, and encoding
    // normalises the spelling. Everything else must compare equal.
    if !matches!(frame, kvlite_resp::Frame::Double(_)) {
        assert_eq!(frame, reparsed, "round trip changed the value");
    }

    let _ = consumed;
});

# kvlite-resp

A **sans-io** RESP2/RESP3 decoder and encoder for the Redis wire protocol. It takes bytes and gives you frames; it takes frames and gives you bytes. It pulls in **no async runtime, no socket, and nothing outside `core` and `alloc`** — so it works under tokio, under async-std, under smol, in blocking code, in a fuzz harness, and on `wasm32`.

```rust
use kvlite_resp::{Decoder, Encoder, Frame, RespProtocol};

let decoder = Decoder::new();
let (command, consumed) = decoder
    .decode_command(b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n")?
    .expect("a complete command");

assert_eq!(command, vec![b"GET".to_vec(), b"key".to_vec()]);
assert_eq!(consumed, 22);

let mut out = Vec::new();
Encoder::new(RespProtocol::Resp2).encode(&Frame::bulk("value"), &mut out);
assert_eq!(out, b"$5\r\nvalue\r\n");
# Ok::<(), kvlite_resp::DecodeError>(())
```

Decoding is incremental: `Ok(None)` means "this is a valid prefix, give me more bytes", and a successful decode reports how many bytes it consumed so you can advance your own buffer. The decoder never holds a reference to your buffer and never blocks.

## When to use this crate instead of the others

Use `kvlite-resp` when you need to **speak the Redis protocol** but do not want a Redis: a proxy, a sniffer, a protocol-level test double, a custom client, or an implementation of the server side.

- Want a key-value store in your process? Use [`kvlite-core`](https://crates.io/crates/kvlite-core).
- Want a Redis-compatible server? Use [`kvlite-server`](https://crates.io/crates/kvlite-server).
- Want a Redis substitute in your tests? Use [`kvlite-testing`](https://crates.io/crates/kvlite-testing).

Every limit that protects you from a hostile peer — line length, bulk length, array length, nesting depth — is configurable through [`Limits`], and all of them are enforced by default.

## Dependencies

**None.** Not one crate outside `core` and `alloc`.

## MSRV and `no_std`

MSRV is 1.85. This crate is `#![no_std]` and needs only `alloc`.

## Full docs

<https://docs.rs/kvlite-resp>

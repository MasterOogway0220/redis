# kvlite-api

Traits and option types for [Kvlite](https://github.com/kvlite/kvlite). Contracts only — this crate pulls in **no engine, no server, and nothing outside `core` and `alloc`**.

```rust
use kvlite_api::{Keyspace, KvWriteCondition};
use core::time::Duration;

/// Works against any Kvlite implementation — embedded, or a remote server.
/// This function's crate does not depend on either one.
pub fn claim(ks: &dyn Keyspace, key: &[u8], holder: &[u8]) -> bool {
    ks.set(key, holder, Some(Duration::from_secs(30)), KvWriteCondition::NotExists, false)
}
```

## When to use this crate instead of the others

Use `kvlite-api` when you are writing a **library** on top of Kvlite — a rate limiter, a session store, a lock — and you want your own consumers to choose the implementation. Depending on this crate rather than the engine means your users are not forced to take the engine with you.

- Building an application, not a library? Use [`kvlite-core`](https://crates.io/crates/kvlite-core) directly.
- Talking to a Kvlite or Redis server? Use a RESP client.
- Need a Redis substitute in your tests? Use [`kvlite-testing`](https://crates.io/crates/kvlite-testing).

The surface here is deliberately narrow: key lifetime, binary-safe strings, and counters. Lists, hashes, sets and sorted sets live on the concrete engine type and over the wire protocol, because nothing has yet needed them through an abstraction and every public symbol is a permanent commitment.

## Dependencies

**None.** Not one crate outside `core` and `alloc`.

## MSRV and `no_std`

MSRV is 1.85. This crate is `#![no_std]` and needs only `alloc`.

## Full docs

<https://docs.rs/kvlite-api>

# kvlite-core

The [Kvlite](https://github.com/kvlite/kvlite) engine: Redis data-type and expiry semantics, running inside your process. It pulls in **no TCP listener, no persistence, no async runtime, and no crates outside `std`**.

```rust
use kvlite_core::KvStore;
use std::time::Duration;

let store = KvStore::new();

store.set("session:abc", "user-42", Some(Duration::from_secs(30 * 60)));
let user = store.get_str("session:abc")?;          // Option<String>
let views = store.incr_by("page:views", 1)?;       // 1

assert_eq!(user.as_deref(), Some("user-42"));
assert_eq!(views, 1);
# Ok::<(), kvlite_api::KvError>(())
```

No server, no port, no configuration file, no background thread. A `KvStore` is `Send + Sync`; share it with an `Arc` and call it from anywhere.

## When to use this crate instead of the others

Use `kvlite-core` when you want a key-value store **in your process** and you want Redis's semantics — binary-safe keys, per-key TTL, the same data types — so the same code and the same mental model work embedded and against a server later.

- Want a cache and nothing more? `moka` is excellent and you probably want that instead. This crate earns its place when you want Redis *semantics*, not just a cache.
- Writing a library others will depend on? Depend on [`kvlite-api`](https://crates.io/crates/kvlite-api) instead, so your consumers pick the implementation.
- Want a Redis-compatible server? Use [`kvlite-server`](https://crates.io/crates/kvlite-server).
- Want a Redis substitute in your tests? Use [`kvlite-testing`](https://crates.io/crates/kvlite-testing).

Expiry is lazy: a key that has passed its deadline is invisible to every read and is dropped when next touched. Nothing here spawns a thread. A long-lived process that wants memory back from keys nobody reads calls [`KvStore::sweep_expired`] on its own schedule — which is exactly what `kvlite-server` does.

## Dependencies

**One**, and it is ours: [`kvlite-api`](https://crates.io/crates/kvlite-api), which is itself dependency-free. Nothing outside `std` reaches this crate, and CI fails if that ever changes.

## MSRV

MSRV is 1.85. This crate needs `std`.

## Full docs

<https://docs.rs/kvlite-core>

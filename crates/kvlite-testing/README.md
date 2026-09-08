# kvlite-testing

A Redis substitute for your integration tests. It starts **in your process, on an ephemeral port, in under 50 ms**, needs **no Docker daemon**, and depends on **no test framework** — so it works under `cargo test`, under `rstest`, under `#[tokio::test]`, and under anything else.

```rust
use std::time::Duration;

let kv = kvlite_testing::Server::start();

// Point any Redis client at it. The wire protocol is the whole compatibility story.
let url = kv.url();                     // redis://127.0.0.1:<ephemeral>

// Time only moves when you move it, so TTL behaviour is deterministic.
kv.store().set("cart:abc", "2 items", Some(Duration::from_secs(30)));
kv.clock().advance(Duration::from_secs(31));
assert!(!kv.store().contains("cart:abc"));

kv.reset();                             // clean keyspace, no restart
# let _ = url;
```

Starting one is **synchronous and needs no runtime of your own**, because the server runs on a runtime it owns. That matters more than it sounds: `#[tokio::test]` is single-threaded by default, so a fixture sharing your runtime would deadlock the moment you used a *blocking* Redis client — silently, with no error, until the test timed out. Here it does not matter whether your test is sync or async, which flavour it uses, or which client you reach for.

## Why this instead of a container

Starting a Redis container costs seconds per test class and needs a daemon that CI has to provide. This costs a millisecond and needs nothing. Dropping the `Server` — including when a test panics mid-assertion — stops the listener and every connection it accepted, so there is no leaked port, no leaked task, and no `--test-threads=1` workaround.

The **controllable clock is a capability Redis does not have**. Testing a 30-minute session expiry against real Redis means either sleeping for 30 minutes or not testing it. Here you advance the clock and assert.

## When to use this crate instead of the others

Use `kvlite-testing` in `[dev-dependencies]` when your code talks to Redis and you want that path covered by tests.

- Want to run a server for real? Install the [`kvlite`](https://crates.io/crates/kvlite) binary.
- Want to embed a server in a non-test binary? Use [`kvlite-server`](https://crates.io/crates/kvlite-server) directly.
- Want a store with no server at all? Use [`kvlite-core`](https://crates.io/crates/kvlite-core).

## Dependencies

No test framework, and nothing chosen by us beyond our own crates. `tokio` arrives transitively through [`kvlite-server`](https://crates.io/crates/kvlite-server), which is the one crate in the project that has a third-party dependency at all.

## MSRV

MSRV is 1.85.

## Full docs

<https://docs.rs/kvlite-testing>

# Kvlite

A Redis-compatible key-value store in Rust. Take one piece of it, or all of it.

> **Status: Phase 1, pre-release.** The engine, the RESP codec, the server and the test double work and are tested. Persistence, clustering and several data types are not built yet. See [Scope](#what-exists-today) below.

## Three doors

| You want | You install | It replaces |
|---|---|---|
| A store inside your process | `cargo add kvlite-core` | `HashMap` + hand-rolled TTL bookkeeping |
| A Redis substitute in your tests | `cargo add --dev kvlite-testing` | `testcontainers` Redis, `docker compose` in CI |
| A server to run | `cargo install kvlite` | Redis, Valkey |

**The test double is the point of entry.** You do not have to trust Kvlite with production data to delete a Redis container from your CI pipeline. It starts in under 50 ms on an ephemeral port with no Docker daemon, and it gives you something Redis cannot: **a clock you control**, so a test can assert that a 30-minute session expired without waiting 30 minutes or sleeping.

```rust
#[test]
fn sessions_expire() {
    let kv = kvlite_testing::Server::start();
    let client = redis::Client::open(kv.url()).unwrap();
    // ...your existing redis-rs test code, unchanged

    kv.clock().advance(Duration::from_secs(1801));
    assert!(!kv.store().contains("session:abc"));
}
```

No runtime needed, no `.await`, no Docker. Works from a plain `#[test]` or any flavour
of async test, with a blocking Redis client or an async one.

## The crates

Each is independently usable, and the graph is shallow and strictly one-directional.

| Crate | What it is | Third-party dependencies |
|---|---|---|
| [`kvlite-api`](crates/kvlite-api) | Traits and options. Contracts only. `no_std`. | **none** |
| [`kvlite-resp`](crates/kvlite-resp) | Sans-io RESP2/RESP3 codec. `no_std`. | **none** |
| [`kvlite-core`](crates/kvlite-core) | The engine. No I/O, no runtime. | **none** |
| [`kvlite-server`](crates/kvlite-server) | TCP, sessions, pub/sub. | `tokio` |
| [`kvlite-testing`](crates/kvlite-testing) | The test double. | `tokio`, transitively |
| [`kvlite`](crates/kvlite) | The binary. No lib target. | `tokio` |

`kvlite-core` has **zero non-`std` dependencies, permanently**, and CI fails if that changes. Every transitive dependency a crate carries is a reason somebody cannot adopt it, so the ones you build against carry none.

`kvlite-resp` is sans-io on purpose: no socket, no runtime, not even an optional one. It works under tokio, under async-std, under smol, in blocking code, in a fuzz harness, and on `wasm32`.

## What exists today

**Working and tested:** all five core data types — strings, hashes, lists, sets and sorted sets; per-key TTL with lazy expiry; 16 selectable keyspaces; RESP2 and RESP3 with `HELLO` negotiation; pipelining; inline commands; channel and pattern pub/sub; glob `KEYS` and `SCAN`; ~90 commands. The full list is in the [server README](crates/kvlite-server/README.md).

**Not built yet:** transactions, scripting, persistence, replication, clustering, TLS, authentication. Those are Phase 2 and later, and they are deliberately absent rather than stubbed — see the [anti-goals](kvlite-packaging.md#12-anti-goals).

## Development

```bash
cargo test --workspace          # everything, including the README examples
cargo xtask                     # the dependency rules, no_std floor, and packaging checks
cargo xtask check-layering      # just the rules from packaging spec section 4
```

`cargo xtask` is where the architecture is enforced. It reads the resolved dependency graph and fails if any crate reaches something it is not allowed to reach — that is what keeps `kvlite-core`'s zero-dependency invariant true rather than aspirational.

The READMEs are included as each crate's rustdoc, so **every example in them is compiled and run by `cargo test`**. A stale example is a failing build, not a bug report.

## Design

[`kvlite-packaging.md`](kvlite-packaging.md) is the governing document: the crate split, the four dependency rules and how they are enforced, the public API stability policy, versioning, and the anti-goals. It constrains code layout from the first commit, because retrofitting it later means moving types across crate boundaries — a breaking change for anyone who adopted early.

## Licence

MIT.

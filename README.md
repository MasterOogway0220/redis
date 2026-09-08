# Kvlite

A Redis-compatible key-value store in Rust. Take one piece of it, or all of it.

> **Status: Phase 1, pre-release.** The engine, the RESP codec, the server and the test double work and are tested. Persistence, clustering and several features are not built yet — see [What exists today](#what-exists-today). Usable now for tests, embedded caching and dev servers; **not** as a production datastore.

## Three doors

| You want | You add | It replaces |
|---|---|---|
| A store inside your process | `kvlite-core` | `HashMap` + hand-rolled TTL bookkeeping |
| A Redis substitute in your tests | `kvlite-testing` | `testcontainers` Redis, `docker compose` in CI |
| A server to run | the `kvlite` binary | Redis, Valkey |

**The test double is the point of entry.** You do not have to trust Kvlite with production data to delete a Redis container from your CI pipeline. It starts in under 50 ms on an ephemeral port with no Docker daemon, and it gives you something Redis cannot: **a clock you control**, so a test can assert that a 30-minute session expired without waiting 30 minutes or sleeping.

---

# Using Kvlite in your project

## 1. Requirements

- **Rust 1.85 or newer.** That is the declared MSRV and CI verifies it against that exact toolchain.
- **Nothing else.** No C toolchain, no Docker, no system Redis. `kvlite-core` has zero dependencies outside `std`.
- Linux, macOS and Windows. CI runs the full suite on all three.

## 2. Add the dependency

Kvlite is not on crates.io yet, so add it from git:

```toml
[dependencies]
kvlite-core = { git = "https://github.com/MasterOogway0220/redis", package = "kvlite-core" }

[dev-dependencies]
kvlite-testing = { git = "https://github.com/MasterOogway0220/redis", package = "kvlite-testing" }
```

Two things about that:

- **`package = "..."` is required.** This repository is a Cargo workspace holding six crates, so Cargo needs to be told which one you mean. Without it, Cargo looks for a crate named after the repository (`redis`) and fails.
- **Pin it.** As written, Cargo locks whatever commit is current into your `Cargo.lock` and only moves when you run `cargo update`. To pin explicitly, add `tag = "v0.1.0"`, `rev = "<sha>"`, or `branch = "main"`.

Once the crates are published this becomes:

```bash
cargo add kvlite-core
cargo add --dev kvlite-testing
```

Take only the door you need. `kvlite-core` does not pull in the server, and `kvlite-testing` belongs in `[dev-dependencies]` so it never ships in your release binary.

---

## Door 1 — an embedded store

No server, no port, no configuration file, no background thread, **no async runtime**.

```rust
use kvlite_core::{KvError, KvStore};
use std::time::Duration;

fn main() -> Result<(), KvError> {
    let store = KvStore::new();

    // Strings, with an optional TTL.
    store.set("session:abc", "user-42", Some(Duration::from_secs(30 * 60)));
    let user: Option<String> = store.get_str("session:abc")?;
    assert_eq!(user.as_deref(), Some("user-42"));

    // Counters. A missing key starts at zero.
    assert_eq!(store.incr_by("page:views", 1)?, 1);
    assert_eq!(store.incr_by("page:views", 9)?, 10);

    // Key lifetime.
    assert!(store.contains("session:abc"));
    assert!(store.time_to_live("session:abc").is_some());
    store.expire("session:abc", None);          // make it permanent
    store.remove("session:abc");

    Ok(())
}
```

### Keys and values are bytes

Everything is binary-safe. Anything that is `AsRef<[u8]>` works as a key or value — `&str`, `String`, `Vec<u8>`, `&[u8]`:

```rust
store.set([0u8, 0xff, 0x80], [0xde, 0xad, 0xbe, 0xef], None);
let raw: Option<Vec<u8>> = store.get([0u8, 0xff, 0x80])?;
```

`get` gives you the bytes; `get_str` decodes them as UTF-8, replacing anything invalid.

### The other data types

The bare methods on `KvStore` cover strings and counters on keyspace 0, which is what most embedded use needs. Lists, hashes, sets and sorted sets live on the keyspace itself:

```rust
use kvlite_core::{KvStore, KvWriteCondition};

let store = KvStore::new();
let kv = store.default_keyspace();

// Hash
kv.hash_set(b"user:1", b"name", b"ana")?;
kv.hash_incr_by(b"user:1", b"logins", 1)?;
let fields = kv.hash_entries(b"user:1")?;            // Vec<(Vec<u8>, Vec<u8>)>

// List
kv.push_back(b"queue", &[b"first", b"second"])?;
let job = kv.pop_front(b"queue")?;                    // Option<Vec<u8>>
let all = kv.list_range(b"queue", 0, -1)?;            // negative indices, as in Redis

// Set
kv.set_add(b"tags", &[b"rust", b"redis"])?;
assert!(kv.set_contains(b"tags", b"rust")?);
let shared = kv.set_intersect(&[b"tags", b"other"])?;

// Sorted set
kv.sorted_set_add(
    b"leaderboard",
    &[(120.0, b"ana"), (95.0, b"bo")],
    KvWriteCondition::Always,
    false,   // GT: only raise an existing score
    false,   // LT: only lower an existing score
)?;
let top = kv.sorted_set_range(b"leaderboard", 0, 0, true)?;   // reverse = highest first
assert_eq!(top[0].0, b"ana".to_vec());
```

Score ranges use `std::ops::Bound`, so `ZRANGEBYSCORE`-style queries are ordinary Rust:

```rust
use std::ops::Bound;

// Everything scoring above 100, exclusive.
let winners = kv.sorted_set_range_by_score(
    b"leaderboard",
    Bound::Excluded(100.0),
    Bound::Unbounded,
    false,
)?;
```

### Sharing it

`KvStore` is `Send + Sync`. Share one with an `Arc` — do **not** clone it, since a clone would be a second, unrelated store:

```rust
use std::sync::Arc;

let store = Arc::new(KvStore::new());
let worker = Arc::clone(&store);

std::thread::spawn(move || {
    worker.incr_by("jobs:done", 1).unwrap();
});
```

There is no static mutable state anywhere, so independent stores in one process never interfere — which is what makes them safe to create per test.

### Configuring it

```rust
use kvlite_core::{KvOptions, KvStore};

let mut options = KvOptions::default();
options.keyspace_count = 4;              // Redis SELECT databases; default 16
// options.clock = Some(my_clock);       // anything implementing KvClock

let store = KvStore::with_options(options);
```

**Expiry is lazy.** An expired key is invisible to every read the instant its deadline passes, whether or not anything has swept it — so correctness never depends on a background task, and nothing here spawns a thread. A long-lived process that wants the memory back from keys nobody reads calls `store.sweep_expired(20)` on its own schedule.

---

## Door 2 — a Redis substitute in your tests

This is the one to try first. It needs no Docker daemon, no async runtime, and no test framework.

```rust
// [dev-dependencies]
// kvlite-testing = { git = "...", package = "kvlite-testing" }
// redis = "0.27"

#[test]
fn reserves_inventory() {
    let kv = kvlite_testing::Server::start();

    // Point any Redis client at it. The wire protocol is the whole story.
    let client = redis::Client::open(kv.url()).unwrap();
    let mut conn = client.get_connection().unwrap();

    // ...your existing code under test, unchanged
    redis::cmd("SET").arg("cart:abc").arg("2 items").arg("EX").arg(1800)
        .exec(&mut conn).unwrap();

    // Assert a 30-minute expiry without waiting 30 minutes.
    kv.clock().advance(std::time::Duration::from_secs(1801));
    let gone: Option<String> = redis::cmd("GET").arg("cart:abc").query(&mut conn).unwrap();
    assert_eq!(gone, None);

    kv.reset();     // clean keyspace, no restart
}
```

### What the fixture guarantees

- **Starts in under 50 ms.** If it were slower than a container there would be no reason to use it. There is a test asserting this.
- **Ephemeral port.** `Server::start()` binds port 0 and `kv.url()` reports what it got, so tests running in parallel never collide.
- **Cleans up on `Drop`** — including when a test panics mid-assertion. No leaked port, no leaked task, no `--test-threads=1` workaround.
- **Needs no runtime of yours.** It owns a runtime internally, so it works from a plain `#[test]`, from any flavour of `#[tokio::test]`, and with a blocking client or an async one. This matters: `#[tokio::test]` is single-threaded by default, and a fixture sharing your runtime would deadlock silently the moment you used a blocking Redis client.
- **Time only moves when you move it.** The clock starts stopped, so a key with a TTL stays alive until you advance it. A test that depends on wall-clock time is a test that fails on a loaded CI machine.

### Reaching past the wire

`kv.store()` is the same `KvStore` the server is serving, so you can seed and assert directly — no round trip, no client:

```rust
let kv = kvlite_testing::Server::start();

kv.store().set("feature:beta", "on", None);        // seed
// ...run the code under test against kv.url()...
assert!(kv.store().contains("feature:beta"));      // assert
```

### Per-test isolation

Two patterns, both fine:

```rust
// One server per test — simplest, and cheap enough at ~1 ms.
#[test]
fn a() { let kv = kvlite_testing::Server::start(); /* ... */ }

// Or one shared server, reset between tests.
kv.reset();   // empties every keyspace, keeps connections open
```

---

## Door 3 — a standalone server

```bash
cargo install --git https://github.com/MasterOogway0220/redis kvlite
kvlite --port 6380

# from anywhere
redis-cli -p 6380 ping
```

```
-b, --bind <ADDRESS>       Address to listen on            [default: 127.0.0.1]
-p, --port <PORT>          Port to listen on, 0 for any    [default: 6380]
-d, --databases <COUNT>    Number of keyspaces             [default: 16]
    --sweep-ms <MILLIS>    Expiry sweep interval, 0 to disable   [default: 100]
-h, --help                 Print help
-V, --version              Print the version
```

It binds `127.0.0.1:6380` by default, not the standard Redis port, so installing it cannot quietly take over from a real Redis on the same machine. There is no configuration file and no authentication — bind to loopback.

### From other languages

Do not look for a Kvlite client for your language; there isn't one and there won't be. Wire compatibility already solved that. Point `redis-py`, `ioredis`, `go-redis`, `Jedis`, `phpredis` or `redis-rb` at the port and change nothing else.

---

## Errors

Every fallible operation returns `Result<T, KvError>`. The variants map one-to-one onto Redis's error strings, and `Display` prints exactly what Redis puts on the wire:

| Variant | Means |
|---|---|
| `WrongType` | a list command against a string key, and so on |
| `NotAnInteger` | `INCR` on something that is not a 64-bit integer |
| `OutOfRange` | the result would overflow |
| `NotAFloat` | an unusable sorted-set score, including NaN |
| `NaNResult` | arithmetic that would produce NaN, such as `+inf` plus `-inf` |

`KvError` implements `std::error::Error`, so `?` into `anyhow`, `eyre` or your own type works as expected.

---

## What is supported

**Data types:** strings, hashes, lists, sets, sorted sets — all five, with per-key TTL and 16 selectable keyspaces.

**Protocol:** RESP2 and RESP3 with `HELLO` negotiation, pipelining, inline commands, channel and pattern pub/sub.

**Commands:** around 90. The full list is in the [server README](crates/kvlite-server/README.md).

## What is not

**Not built yet:** persistence, replication, clustering, transactions (`MULTI`/`EXEC`), scripting, TLS, authentication, and the lexicographic sorted-set commands.

**Consequences you need to know about:**

- **Everything is in memory.** A restart loses all of it. This is why Kvlite is not a production datastore yet — persistence is Phase 2.
- **No authentication or TLS.** Bind to loopback.
- `SCAN` returns the whole keyspace in one pass with a cursor of `0`. That satisfies the `SCAN` contract but does not bound the reply.
- `SPOP` and `SRANDMEMBER` pick arbitrarily, not uniformly at random. Do not build a lottery on them.
- `INFO` reports `redis_version:7.4.0` so clients do not disable features that work. The honest number is `kvlite_version`.

---

# The crates

Each is independently usable, and the graph is shallow and strictly one-directional.

| Crate | What it is | Third-party dependencies |
|---|---|---|
| [`kvlite-api`](crates/kvlite-api) | Traits and options. Contracts only. `no_std`. | **none** |
| [`kvlite-resp`](crates/kvlite-resp) | Sans-io RESP2/RESP3 codec. `no_std`. | **none** |
| [`kvlite-core`](crates/kvlite-core) | The engine. No I/O, no runtime. | **none** |
| [`kvlite-server`](crates/kvlite-server) | TCP, sessions, pub/sub. | `tokio` |
| [`kvlite-testing`](crates/kvlite-testing) | The test double. | `tokio` |
| [`kvlite`](crates/kvlite) | The binary. No lib target. | `tokio` |

`kvlite-core` has **zero non-`std` dependencies, permanently**, and CI fails if that changes. Every transitive dependency a crate carries is a reason somebody cannot adopt it, so the ones you build against carry none.

`kvlite-resp` is sans-io on purpose: no socket, no runtime, not even an optional one. It works under tokio, under async-std, under smol, in blocking code, in a fuzz harness, and on `wasm32`.

**Writing a library on top of Kvlite?** Depend on `kvlite-api` instead of the engine, so your own users choose the implementation rather than being handed one.

# Development

```bash
cargo test --workspace          # everything, including the crate README examples
cargo xtask                     # dependency rules, no_std floor, packaging checks
cargo xtask public-api          # regenerate the tracked API snapshots

# Every example on this page, compiled and run from outside the workspace against
# a real third-party Redis client:
cargo run  --manifest-path examples/consumer/Cargo.toml
cargo test --manifest-path examples/consumer/Cargo.toml
```

[`examples/consumer`](examples/consumer) is a working reference project — a detached crate on edition 2021 that uses Kvlite exactly as this README describes. **Copy it as a starting point**, and note that it exists because a real bug got past everything else: the test fixture used to share the caller's async runtime, which silently deadlocks a blocking Redis client on the default `#[tokio::test]`. Every in-workspace test passed. Building a crate in isolation is not enough; it has to be *used* in isolation.

`cargo xtask` is where the architecture is enforced. It reads the resolved dependency graph and fails if any crate reaches something it is not allowed to reach — that is what keeps `kvlite-core`'s zero-dependency invariant true rather than aspirational.

Each crate's README is included as its rustdoc, so **every example in them is compiled and run by `cargo test`**. A stale example is a failing build, not a bug report.

# Design

[`kvlite-packaging.md`](kvlite-packaging.md) is the governing document: the crate split, the four dependency rules and how they are enforced, the public API stability policy, versioning, and the anti-goals. It constrains code layout from the first commit, because retrofitting it later means moving types across crate boundaries — a breaking change for anyone who adopted early.

# Licence

MIT.

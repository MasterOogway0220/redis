# Kvlite — Packaging & Distribution Specification

**Addendum to the Kvlite PRD v1.0. How consumers take one piece without taking the whole system.**

| | |
|---|---|
| **Version** | 2.0 (draft) |
| **Date** | 8 September 2026 |
| **Status** | For build — constrains Phase 1 onward |
| **Owner** | Aditya |
| **Supersedes** | v1.0 (.NET / C#), archived at `docs/kvlite-packaging-dotnet-v1.0-ARCHIVED.md` |

---

## 0. What changed in v2.0

The implementation language moved from C#/.NET to **Rust**. The argument of v1.0 is unchanged and every principle survives intact — this is not a rethink, it is a translation. What changed is every *mechanism*, because the mechanisms were all .NET-specific:

| v1.0 (.NET) | v2.0 (Rust) |
|---|---|
| NuGet packages | crates in one Cargo workspace |
| `Kvlite.Abstractions` interfaces | `kvlite-api` traits |
| `PublicApiAnalyzers` + `PublicAPI.Shipped.txt` | `cargo-public-api` + committed `public-api.txt`, plus `cargo-semver-checks` |
| `[Experimental("KVL001")]` | non-default `unstable` cargo feature, documented as semver-exempt |
| Trimming / NativeAOT annotations | `no_std` + `alloc`, LTO, static musl, binary-size budget |
| `netstandard2.0` floor for two packages | `no_std` support for `kvlite-api` and `kvlite-resp` only |
| `net8.0` floor | MSRV, declared in `rust-version` and gated in CI |
| Source Link + `.snupkg` symbols | docs.rs source browsing + published debug symbols |
| Package signing | crates.io cannot sign; cosign on images + SLSA provenance |
| `Kvlite.Extensions.Caching` (`IDistributedCache`) | **removed** — Rust has no universal cache trait; the ninth slot is the `kvlite` binary crate |
| xUnit `IClassFixture` | a plain struct with `Drop`; works with any test harness |

Three consequences are worth stating up front because they change engineering decisions, not just names:

1. **The zero-dependency rule bites harder in Rust.** In .NET, `Kvlite.Core` got a rich standard library for free. In Rust, "no third-party crates" also means no `hashbrown`, no `ahash`, no `parking_lot`, no `crossbeam-epoch`, no `smallvec`. Section 1 says what to do about it.
2. **The protocol crate must be sans-io.** A codec that depends on an async runtime is not a codec, it is a client. Section 3 makes this a hard rule.
3. **Rust enforces two of our four rules for us.** Crate cycles are a compile error, and there is no `InternalsVisibleTo` equivalent. Rule 4 is repurposed rather than retired.

---

## Contents

1. Why this is an architecture decision, not a release chore
2. The three doors
3. Crate split
4. Dependency rules and how they are enforced
5. Public API surface and stability policy
6. Consumption examples
7. Non-Rust consumers
8. Versioning and compatibility
9. Binary size, startup, and portability
10. Documentation requirements per crate
11. Release engineering
12. Anti-goals
13. Build checklist

---

## 1. Why this is an architecture decision, not a release chore

"People can use just the part they need" is not something you add at release time. It is a constraint on how the code is written from the first commit, and it is violated the first time someone in the storage engine needs a config value that lives in the server layer.

Every transitive dependency a crate carries is a reason somebody cannot adopt it. A caching library that drags in a TCP server, a Raft implementation, a TLS stack and a metrics exporter will lose to one that drags in nothing — regardless of which is technically better. In Rust the cost is more visible than it was in .NET, because the consumer sees it as compile time, as `Cargo.lock` churn, as `cargo audit` advisories they now own, and as a supply-chain review they now have to do. The engineering rule that follows:

> **`kvlite-core` has zero non-`std` dependencies. Permanently. This is a CI-enforced invariant, not a preference.**

If a feature cannot be built in `kvlite-core` against `std` alone, it does not belong in `kvlite-core`.

This is a real constraint, not a slogan. It rules out the crates a Rust engineer reaches for by reflex:

| Reflex | Why it is banned in core | What we do instead |
|---|---|---|
| `ahash` / `rustc-hash` | third-party — **and wrong here anyway** | `std`'s default `SipHash`, randomly seeded per map. Keys arrive from the network, so the hasher sits on a trust boundary: an unseeded `FxHash` lets a peer craft colliding keys and turn the keyspace into a linked list. Redis moved *to* SipHash for exactly this reason. Replace it only with something both faster **and** seeded, and only with a benchmark |
| `hashbrown` | third-party | `std::collections::HashMap` **is** hashbrown, re-exported. Nothing to gain |
| `parking_lot` | third-party | `std::sync::Mutex` — since Rust 1.62 it is a futex on Linux and an `SRWLOCK` on Windows, and the remaining gap does not justify a dependency |
| `crossbeam-epoch` | third-party | if epoch reclamation is genuinely required, hand-roll it in core under the `unsafe` policy in section 9 — or redesign so it is not required |
| `smallvec` / `arrayvec` | third-party | const-generic arrays, and `Vec::with_capacity` |
| `thiserror` | third-party, and a proc-macro compile-time cost | hand-written `Display` and `std::error::Error` impls. There are few enough error types to make this cheap |
| `serde` | third-party | core has no serialisation concern at all. Persistence and config own their formats |

Every one of these is a good crate. None of them is worth being the reason a consumer says no.

---

## 2. The three doors

There are exactly three ways someone adopts Kvlite. Each has a different crate, a different first-run experience, and a different competitor. Design each door for its own audience.

| Door | What they install | Replaces | Time to first success |
|---|---|---|---|
| **Embedded** | `cargo add kvlite-core` | `HashMap` + hand-rolled TTL bookkeeping; `moka` where Redis semantics are wanted | Under 2 minutes |
| **Test double** | `cargo add --dev kvlite-testing` | `testcontainers` Redis, `docker compose` in CI | Under 5 minutes |
| **Standalone** | `cargo install kvlite` / container | Redis, Valkey | Under 15 minutes |

**The test double is the wedge.** It is the lowest-risk way anyone tries this project. Nobody has to trust Kvlite with production data to delete a Redis container from their CI pipeline and cut two minutes off every build. That is a real, immediate, individually-decidable win — and it puts the engine in front of engineers who will later consider door 1 and door 3.

Ship door 2 in Phase 1. It requires no persistence, no clustering, and no replication — only correct data types and a working RESP server, which Phase 1 delivers anyway.

Door 1's competitive story is weaker in Rust than it was in .NET, and we should be honest about that. .NET had no good embedded key-value store, so `Kvlite.Core` was differentiated on its own. Rust has `moka`, `sled` and `redb`. What Kvlite offers instead is narrower and should be pitched narrowly: **Redis data-type and expiry semantics, in-process, with no server** — so the same code and the same mental model work embedded in a test, embedded in production, and against a remote server. That is the pitch. "A faster cache" is not.

---

## 3. Crate split

Nine crates in one Cargo workspace. Each is independently publishable and consumable, and the dependency graph is strictly acyclic and shallow.

| Crate | Contains | Depends on | Third-party deps |
|---|---|---|---|
| **`kvlite-api`** | Traits and option types only. `Store`, `Keyspace`, `KvOptions`, `KvClock`, error types. No implementation. `no_std` + `alloc`. | — | None |
| **`kvlite-resp`** | RESP2/RESP3 decoder and encoder, command framing, reply serialisation. **Sans-io.** Usable standalone by anyone building a Redis-compatible proxy, sniffer or mock. `no_std` + `alloc`. | — | None |
| **`kvlite-core`** | The engine. Data structures, expiry, eviction, arenas. No I/O, no network, no disk, **no async runtime**. | `kvlite-api` | **None — enforced** |
| **`kvlite-persistence`** | WAL, group commit, incremental checkpoint, recovery, RDB import. | `kvlite-api`, `kvlite-core` | Minimal |
| **`kvlite-server`** | TCP listener, session state, ACL, pub/sub, keyspace notifications, config. | `kvlite-api`, `kvlite-core`, `kvlite-resp`, `kvlite-persistence` | `tokio` |
| **`kvlite-cluster`** | Raft control plane, per-shard replication, slot ownership, resharding. | `kvlite-server` | Raft crate |
| **`kvlite-client`** | Async Rust client. Speaks RESP, so it works against Kvlite **and** real Redis. | `kvlite-resp` | `tokio` |
| **`kvlite-testing`** | In-process server on an ephemeral port, per-test isolation, `reset()`, controllable clock. | `kvlite-server` | `tokio`, transitively |
| **`kvlite`** | The standalone binary. **Bin target only, no lib target.** | `kvlite-server`, `kvlite-persistence`, `kvlite-cluster` | CLI, config, logging |

A tenth workspace member, `xtask`, holds the CI checks in section 4. It is `publish = false` and is not a product crate.

### Notes on specific crates

**`kvlite-resp` must be sans-io, and this is the single most important structural decision in this document.** It takes bytes and produces frames; it takes frames and produces bytes. It does not own a socket, does not know what a runtime is, and does not have a `tokio` dependency — not even an optional one. The moment it does, it stops being usable by `async-std`, by `smol`, by blocking code, by a fuzz harness, by a `no_std` embedded consumer, and by anybody who has already picked a different runtime. Sans-io costs a small amount of API awkwardness at the edges and buys the entire addressable audience. `kvlite-server` and `kvlite-client` are where the runtime lives.

**`kvlite-resp` standalone is a deliberate gift.** A clean, allocation-light, dependency-free, sans-io RESP codec does not exist in Rust as a standalone crate — `redis-rs` has one, welded to its client. Publishing ours costs nothing extra, because we are writing it anyway, and it draws in contributors who have no interest in the storage engine but do care about protocol correctness. Those contributors harden the most security-sensitive component we own. Fuzz it from day one; a sans-io codec is trivially fuzzable, which is another thing the design buys.

**`kvlite-client` working against real Redis is not a mistake.** It means a team can adopt the client without adopting the server, then change a connection string later. Never build a client that only talks to your server; that is a lock-in signal, and it halves the number of people who will try it. This crate competes directly with `redis-rs`, which is mature and good — so it is a Phase 4–5 item and it has to justify itself on merit. If it cannot, we do not ship it, and nothing is lost: our server speaks RESP, so `redis-rs` already works.

**`kvlite-api` exists so that other libraries can depend on Kvlite without depending on the engine.** A library author writing a rate limiter on top of Kvlite depends on `kvlite-api` only; their users pick the implementation. Without this crate, every downstream library forces the full engine on its consumers. Keeping it `no_std` makes that constraint structural rather than aspirational.

**`kvlite` is a binary crate with no lib target, and that is load-bearing.** Section 12 forbids a batteries-included meta-crate. Making the umbrella name a bin-only crate means `cargo add kvlite` gives a consumer nothing useful, while `cargo install kvlite` gives an operator exactly what they came for. The naming problem and the anti-goal solve each other.

**There is no framework-integration crate, and that is a deliberate omission.** v1.0 had `Kvlite.Extensions.Caching` because .NET has `IDistributedCache` — one universal interface that every .NET cache implements, behind which adoption is a single line in `Program.cs`. Rust has no equivalent. Implementing `tower_sessions::SessionStore`, or a `moka`-shaped trait, or an `axum` extractor would each bet the crate on one framework and serve a fraction of the audience. Add an adapter when a specific integration is asked for by name and can describe its use case — as `kvlite-tower`, out of tree if possible, and never as a dependency of anything else.

---

## 4. Dependency rules and how they are enforced

Rules stated in a document decay. These are enforced by checks that fail the build. They live in `xtask`, so they need no tooling beyond a Rust toolchain, and they read the resolved graph from `cargo metadata` rather than parsing `Cargo.toml` — the manifest stops telling the truth as soon as features are involved.

**Rule 1 — `kvlite-core` has no non-`std` dependencies.**

```rust
// xtask/src/layering.rs
/// The complete, intended dependency graph. Anything not listed is a violation.
/// Adding a line here is a deliberate architectural act, and it shows up in review.
const ALLOWED: &[(&str, &[&str])] = &[
    ("kvlite-api",         &[]),
    ("kvlite-resp",        &[]),
    ("kvlite-core",        &["kvlite-api"]),          // <- Rule 1 lives here
    ("kvlite-persistence", &["kvlite-api", "kvlite-core"]),
    ("kvlite-server",      &["kvlite-api", "kvlite-core", "kvlite-persistence", "kvlite-resp", "tokio"]),
    ("kvlite-cluster",     &["kvlite-server"]),
    ("kvlite-client",      &["kvlite-resp", "tokio"]),
    ("kvlite-testing",     &["kvlite-server"]),
];

pub fn check(meta: &Metadata) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    for (krate, allowed) in ALLOWED {
        // Direct AND transitive: a transitive dependency is still a dependency,
        // and it is still a reason somebody cannot adopt the crate.
        for dep in meta.transitive_deps(krate) {
            if !allowed.contains(&dep.as_str()) {
                errors.push(format!("{krate} must not depend on {dep}"));
            }
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}
```

Checking the *transitive* set is the point. A direct-dependency check passes happily while `kvlite-core` pulls in forty crates through one innocent-looking edge.

**Rule 2 — layering is one-directional.** The same `ALLOWED` table enforces it: `kvlite-core` cannot appear to depend on `kvlite-server`, because `kvlite-server` is not in its row. Cargo already makes crate *cycles* a hard compile error, so we get acyclicity for free and only have to check direction. The moment the engine needs something from the server, that something is in the wrong layer — move it down into `kvlite-api` or pass it in as a parameter.

**Rule 3 — every crate builds, tests and runs alone.** CI creates a throwaway crate outside the workspace, adds exactly one Kvlite crate from a local registry, runs a two-line smoke test, and fails if it does not compile and execute. Run it for all nine on every release.

In Rust this rule catches three distinct bugs, and only the first has a .NET analogue:

- **Path dependencies without a `version`.** Builds perfectly in the workspace, is unpublishable and broken from the registry.
- **Feature unification.** Inside a workspace, Cargo unifies features across members. A crate that only compiles because a *sibling* enabled `tokio/net` builds green all day locally and fails the instant it is consumed alone. This is the Rust form of v1.0's "works because of a sibling project reference", and it is more insidious, because nothing in the manifest looks wrong.
- **Missing feature gates.** A `#[cfg(feature = "x")]` block referring to items that are not themselves gated.

So the isolated check must build **outside the workspace directory** — a workspace root anywhere above the scratch crate silently re-absorbs it, so put an empty `[workspace]` table in the scratch manifest to detach it — and it must exercise the feature space, not just the default:

```bash
cargo publish --dry-run -p "$crate"          # catches unversioned path deps
cargo hack check -p "$crate" --feature-powerset --no-dev-deps
```

**Rule 4 — no upward backdoors.** Rust has no `InternalsVisibleTo`, so the v1.0 form of this rule is enforced by the language and needs no test. The Rust-shaped version of the same collapse is a lower layer exposing something so a higher one can reach it:

- No `#[doc(hidden)] pub` item used as a cross-crate escape hatch. `#[doc(hidden)]` hides an item from the docs; it does not make it private, and it is still a semver commitment people will depend on.
- No `pub use` of a dependency's types from a crate's own public API unless the re-export is deliberate and documented — see section 5, where the same practice carries a semver cost.

---

## 5. Public API surface and stability policy

Once people depend on this, every public symbol is a commitment. Three mechanisms:

**Track the public API in source control.** `cargo public-api` produces the full public surface of a crate as text. Commit `public-api.txt` per crate and fail CI when the generated surface differs from the committed file. Any change to the public surface then fails the build until the file is updated — which makes the API change a visible line in the pull request diff instead of something noticed after release. This is the single highest-value habit for a library project and it costs one CI step.

**Gate semver mechanically.** Add `cargo semver-checks` to CI. It catches the changes a human reviewer misses — a struct gaining a private field and breaking functional-record-update construction, an enum gaining a variant and breaking an exhaustive match, a trait gaining a method without a default. Mark every public enum and struct we expect to grow as `#[non_exhaustive]` **from the first release**; retrofitting it later is itself the breaking change.

**Mark experimental APIs.** New surface ships behind a non-default cargo feature named `unstable`, documented as exempt from semver and annotated in the rustdoc. A consumer who wants it opts in explicitly and knows what they took on. It buys the freedom to change a design after real usage without a major bump, and it is honest with adopters about what is settled.

**Do not leak dependency types through a public API.** This is Rust-specific and it is the most expensive mistake available here. A `pub fn` that takes or returns a `tokio::net::TcpStream` makes tokio's major version part of *our* semver: when tokio 2.0 ships, every consumer is stuck until we do a major release, and no consumer can mix us with a different runtime. Wrap dependency types in our own, or accept generic bounds — `AsyncRead + AsyncWrite`, `impl Into<Vec<u8>>` — instead of naming the foreign type. Where re-exporting genuinely is the right call, do it deliberately, document it, and treat that dependency's version as part of our public API.

**Keep the surface small.** Rust's default is private, which is the right default; the discipline is not to fight it. `pub(crate)` until someone has a concrete reason it should be `pub`. An API we never shipped costs nothing to change.

---

## 6. Consumption examples

These belong in the README of each crate, at the top, above everything else. Someone evaluating a library decides in about thirty seconds.

Because each README is included as the crate's own rustdoc (`#![doc = include_str!("../README.md")]`), these blocks are compiled and run as doctests by `cargo test`. A stale example in a README becomes a failing build rather than a bug report — which is a straight improvement on v1.0, where nothing checked them.

### Door 1 — embedded

```rust
// cargo add kvlite-core
use kvlite_core::KvStore;
use std::time::Duration;

let store = KvStore::new();

store.set("session:abc", "user-42", Some(Duration::from_secs(30 * 60)));
let user = store.get_str("session:abc");           // Option<String>
store.incr_by("page:views", 1);
```

No server, no port, no configuration file, no background process, no async runtime.

### Door 2 — integration tests

```rust
// cargo add --dev kvlite-testing
#[test]
fn reserves_inventory() {
    let kv = kvlite_testing::Server::start();         // ephemeral port, < 50 ms

    let client = redis::Client::open(kv.url()).unwrap();
    // ...existing redis-rs test code, unchanged

    kv.clock().advance(Duration::from_secs(31));      // TTL expiry, no sleeping
    assert!(!kv.store().contains("cart:abc"));        // assert without a round trip

    kv.reset();                                       // clean keyspace, no restart
}
```

The whole surface is synchronous, and starting a server needs no runtime from the
caller. That is not a stylistic choice, it is a correctness one, and it was found by
consuming the crate from outside the workspace rather than by reading the code:

> `#[tokio::test]` is **single-threaded by default**. A fixture that ran the server on
> the caller's runtime would deadlock the instant the test used a *blocking* Redis
> client — the client blocks the only thread, the accept loop never runs, and the test
> hangs with no error until CI kills it. Since a large share of Rust codebases use the
> blocking `redis` client, the default way to write the test would have been the
> broken way.

So the fixture owns a one-worker runtime of its own, and works from a plain `#[test]`,
from any flavour of async test, with a blocking client or an async one. There is a
regression test for exactly that combination.

This is the same class of bug Rule 3 exists to catch, and it argues for widening that
rule: **building a crate in isolation is not enough, it has to be *used* in isolation.**
Add a consumer smoke test that drives each door the way a real adopter would, with a
real third-party client, and run it in CI alongside the compatibility matrix in
section 7.

Requirements for this harness, all of them non-negotiable:

- Starts in **under 50 ms**. If it is slower than starting a container, there is no reason to use it.
- Binds an **ephemeral port**, so tests running in parallel never collide. Rust runs test threads in one process by default, which makes this sharper than it was in .NET: a fixed port is not merely fragile, it is broken on the first `cargo test`.
- `reset()` gives a clean keyspace **without a restart**.
- Exposes a **controllable clock**, so a test can advance time and assert TTL expiry without sleeping. Redis cannot do this. It is a genuine capability advantage and should be advertised as one.
- Cleans up on `Drop`, including on panic and on a failed assertion — no leaked ports, no leaked tasks, no `--test-threads=1` workaround.
- Depends on **no test framework**. It is a struct with a constructor and a `Drop` impl, so it works under the built-in harness, under `rstest`, under `tokio::test`, and under anything else. This is strictly better than v1.0, which needed per-framework adapters.
- Runs on Linux, Windows and macOS with no Docker daemon.

### Door 3 — standalone

```bash
cargo install kvlite
kvlite --port 6380

# or
docker run -p 6380:6380 kvlite/kvlite:1
```

A statically linked binary with no runtime to install — which in Rust is the default rather than an achievement, so do not oversell it.

---

## 7. Non-Rust consumers

**Do not write client libraries for other languages.** Wire compatibility already solved this. `redis-py`, `ioredis`, `go-redis`, `Jedis`, `Lettuce`, `phpredis`, `redis-rb` and `redis-rs` all work against Kvlite today with a changed port number.

**Do not ship an FFI layer either.** Rust makes this temptation stronger than .NET did: `cbindgen`, PyO3, `napi-rs` and `wasm-bindgen` all make "just expose the engine to Python" look like an afternoon. It is not. A C ABI is a second public API surface with its own semver, its own memory-ownership contract, its own crash modes on the consumer's side of the boundary, and its own build matrix — and it competes with the wire protocol we already support everywhere. The one exception worth considering later is `wasm32`, and only for `kvlite-resp`, where the sans-io design already makes it nearly free.

What non-Rust users need instead:

1. A **compatibility matrix** stating which clients are tested against Kvlite in CI, and at which versions. Test the top client for each of Python, Node, Go, Java, Ruby and PHP in the integration suite.
2. A **one-page migration note** per ecosystem: change the port, here is what is not yet supported, here is how to import an RDB file.
3. **`redis-cli` compatibility** as a hard requirement. It is how every operator will first poke at the server, and a failure there reads as "this project is not real".

For every language including Rust, being a better Redis server is the whole product. The embedded story in v1.0 was a .NET-specific differentiator; in Rust it is a convenience for people who already chose us, not a reason to choose us.

---

## 8. Versioning and compatibility

- **Semantic versioning**, strictly, as Cargo defines it — which means `0.x.y` gives no compatibility guarantee across minors, and `1.x` does.
- **All crates share one version** and release together, inherited from `[workspace.package]` with `version.workspace = true`. Mismatched Kvlite crate versions in one dependency graph are a support burden with no upside, and Cargo will happily link two majors of the same crate into one binary and produce type errors that read as nonsense.
- **Stay on `0.x` until Phase 1 exit criteria are met**, then go to `1.0.0`. Publishing `1.0.0` and then breaking it is worse than a long `0.x`; Rust consumers read `0.x` correctly and are not put off by it. Anything that has not met its phase exit criteria ships as `-preview.N`, which Cargo will not resolve to unless a consumer asks for it by name.
- `kvlite-core` **1.x supports the 1.x wire protocol**. State the supported protocol range in `INFO` and in the docs.
- **MSRV is part of the contract.** Declare `rust-version` in `[workspace.package]`, verify it in CI with that exact toolchain, and treat **raising it as a minor version bump**, with a note in the release notes. Never raise it in a patch. Kvlite targets an N-4 window, which is roughly six months of toolchains.
- **`Cargo.lock` is committed** — the workspace produces a shipped binary, so a reproducible build of it matters more than the library convention of leaving it out. CI additionally runs a `--locked` build, and a separate job with `-Z minimal-versions` to prove the declared lower bounds are real.
- **Persistence format is versioned independently** of the crate version, with a documented compatibility window: version N reads formats N-2 through N. Never break the ability to read an old checkpoint without a migration path — data people cannot get out of your system is data they will not put into it.
- **Deprecation:** `#[deprecated(since = "…", note = "use …")]` for one full minor cycle naming the replacement, then remove at the next major. Never remove without a prior warning release.

---

## 9. Binary size, startup, and portability

v1.0 called this section "Trimming, AOT, and startup cost" and framed it as adoption features rather than optimisations. That framing holds; the items change, because Rust gives us ahead-of-time compilation and a small runtime for free, and the interesting constraints move elsewhere.

- **`no_std` + `alloc` for `kvlite-api` and `kvlite-resp` only.** This is the direct analogue of v1.0's `netstandard2.0` floor for the two contract packages, and it serves the same goal: the pieces other people build against must impose as little as possible. It also unlocks `wasm32` and embedded consumers for the codec. Every other crate is `std`.
- **No proc-macro dependencies in `kvlite-api`, `kvlite-resp` or `kvlite-core`.** Proc macros are a compile-time tax the consumer pays on every clean build, and they are the largest single contributor to the "Rust compiles slowly" reputation. Budget: `cargo build -p kvlite-core` from cold completes in **under 5 seconds**. Track it as a regression gate — the number is a proxy for the dependency invariant, and it is what a prospective adopter actually feels.
- **No `build.rs` in any library crate.** It defeats caching, it is a supply-chain surface, and it breaks `cargo vet`-style review.
- **No static mutable state.** Two `KvStore` instances in one process must be fully independent. Tests will create dozens. Rust's ownership rules make this nearly automatic, but a `OnceLock` global or a `static` registry will slip in under the guise of convenience — ban them in core by review.
- **Cold start under 10 ms** for the embedded engine, measured as time to first successful `set`. Anything slower disqualifies it from short-lived processes and serverless functions. The realistic threat here is not codegen, it is eager allocation: do not preallocate arenas or spawn an expiry thread until the first write.
- **`#![forbid(unsafe_code)]` in `kvlite-api`, `kvlite-resp`, `kvlite-client` and `kvlite-testing`.** `kvlite-core` may use `unsafe` for arenas and reclamation, but only where a benchmark demonstrates it matters, only with a documented safety comment per block, and only with **Miri clean in CI** and a fuzz target covering it. Unsafe with neither a measurement justifying it nor a proof defending it is the one form of over-engineering in this project that can lose data.
- **Binary budget.** Release profile `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`, with debug symbols published separately per section 11. Target: the `kvlite` binary stays **under 8 MB** static-musl. Track it as a regression gate.
- **`panic = "abort"` is a release-workflow flag, not a profile setting.** Cargo profiles are workspace-scoped, so putting it in `[profile.release]` would apply it to every crate in the workspace and break `cargo test --release`. Pass it as `RUSTFLAGS` when building the shipped binary, and leave the profile alone. Never set it in a published library's profile either — it is inherited by consumers who did not ask for it.
- **`x86_64-unknown-linux-musl` builds statically**, which makes the distroless and `scratch` container images in section 11 trivial rather than an effort.

---

## 10. Documentation requirements per crate

Every crate README, in this order. No exceptions, no rearranging:

1. **One sentence** stating what this crate is and — critically — what it does *not* pull in.
2. **A runnable code block**, under ten lines, that does something useful.
3. **When to use this crate instead of the others**, with links to the others.
4. **Dependencies**, listed explicitly. "No dependencies outside `std`" is a selling point; say it out loud.
5. **MSRV**, and any `no_std` support.
6. **Link to full docs.**

Include the README as the crate-level rustdoc:

```rust
#![doc = include_str!("../README.md")]
```

This does three things at once: it puts the README on docs.rs as the landing page, it makes every example in it a doctest that CI runs, and it means there is exactly one copy of the introduction to keep current. Do it in every crate.

Do not put the project's history, motivation, benchmarks, or architecture in a crate README. Those belong in the repository README and the docs site. Someone reading a crate page on docs.rs or crates.io has one question — "does this solve my problem in the next five minutes?" — and everything else on that page is in the way.

---

## 11. Release engineering

- **Publish to crates.io on tag**, from CI, never from a laptop. Publish in dependency order; `cargo-release` or `release-plz` handles the ordering and the version bump across the workspace.
- **Pin the toolchain** with `rust-toolchain.toml` so a release build is reproducible, and build with `--locked`.
- **crates.io does not support package signing.** Do not claim otherwise. What we can do instead, and should: publish **SLSA provenance attestations** from CI for every release artifact, and **sign container images with cosign**. State plainly in the release docs what is and is not attested.
- **Source and symbols.** docs.rs hosts browsable source for every published crate automatically, which covers most of what Source Link did in v1.0. For the binary, publish stripped release artifacts alongside their separated debug symbols, and build with `--remap-path-prefix` so paths in a backtrace are stable and do not leak a builder's directory layout.
- **Publish an SBOM** per release (`cargo cyclonedx`), and run `cargo audit` and `cargo deny` in CI — advisories, licences, duplicate versions and banned crates. `cargo deny` also gives us a second, independent enforcement point for the section 4 dependency rules.
- **Container images** are multi-arch (`x86_64`, `aarch64`), with a distroless or `scratch` variant built from the static musl target.
- **Release notes** name every public API change, referencing the `public-api.txt` diff.
- **Prerelease channel** (`-preview.N`) for anything that has not met its phase exit criteria. Do not ship an unstable engine on a stable version number to hit a date.
- **Reserve the crate names now.** All nine, before the first real release. crates.io is a first-come registry with no takebacks, and losing `kvlite-core` to a stranger after the design is committed is an unrecoverable and entirely avoidable problem.

---

## 12. Anti-goals

Things that look like good packaging decisions and are not:

- **A single "batteries-included" `kvlite` library crate** that re-exports everything. It defeats the entire purpose of the split and becomes what everyone adds by default. The name is taken by the binary crate specifically to make this impossible.
- **A plugin or module loading system.** Redis modules are a large maintenance surface, a security boundary problem and a versioning nightmare. In Rust it is strictly worse than it was in .NET, because there is no stable ABI: a dynamically loaded plugin must be compiled by the exact same toolchain version as the host, or it is undefined behaviour. Composition happens at the crate level, at compile time, through cargo features.
- **Client libraries in other languages, or an FFI layer.** See section 7.
- **An async runtime in `kvlite-core` or `kvlite-resp`.** Not even optional, not even feature-gated. A feature-gated runtime dependency still appears in a consumer's `cargo audit`, still shows up in a supply-chain review, and can still be switched on accidentally by feature unification from an unrelated crate in their graph.
- **Public APIs added speculatively** for hypothetical users. Add them when someone asks and can describe the use case.
- **`unsafe` for performance without a benchmark showing it matters and Miri showing it is sound.** See section 9.
- **Splitting further than nine crates.** Granularity has a cost too: more READMEs to keep current, more feature combinations to test, more publish ordering to get right, more decisions pushed onto the consumer. Nine is enough.

---

## 13. Build checklist

Ordered. Items in Phase 1 are the ones that create adoption; everything else follows.

**Phase 1 (with the data types):**

- [x] Cargo workspace split into `kvlite-api`, `kvlite-resp`, `kvlite-core`, `kvlite-server`
- [x] `xtask` with Rule 1 and Rule 2 checks failing CI on violation — verified to fail on an injected violation, not merely to pass
- [x] `cargo public-api` snapshots committed per crate; CI fails on undeclared surface change
- [x] `#[non_exhaustive]` applied to every public enum and struct before the first release
- [x] `kvlite-testing` with ephemeral port, `reset()`, controllable clock, `Drop` cleanup, no test-framework dependency
- [x] `kvlite` binary crate — bin target only, and tested by spawning the built artifact rather than the library
- [x] Per-crate READMEs following the section 10 template, included via `#![doc = include_str!]`, so their examples are doctests
- [x] `no_std` builds verified for `kvlite-api` and `kvlite-resp`, against a bare-metal target
- [x] MSRV declared in `[workspace.package]`
- [x] Fuzz targets for `kvlite-resp` decode and round trip, running nightly on a schedule rather than per push — sixty seconds a commit finds almost nothing, and the invariants are pinned as ordinary unit tests so a regression still fails the push build
- [~] Rule 3 isolated-build check: `publish --dry-run` runs in `xtask` and passes for the two dependency-free crates; the rest report *pending* until their dependencies are on the registry. `--feature-powerset` runs in CI only, where `cargo-hack` is available.
- [ ] `cargo semver-checks` — deliberately absent from CI until the first release, because it has nothing to compare against and a permanently red job teaches people to ignore the signal. It goes in with the first publish.

**A note on what the API snapshots track.** `cargo public-api` is run with `--omit blanket-impls,auto-trait-impls,auto-derived-impls`, and the omission is load-bearing rather than cosmetic. Those impls are generated by the compiler, so they move when the *toolchain* moves — and the job runs on nightly. Tracking them would turn a rustc release into a red build with no cause in this repository, which is the failure mode that gets a check disabled. What remains is our own surface, which is the thing section 5 set out to make reviewable.
- [ ] Publish to crates.io on tag; all nine names reserved. **Do the name reservation first** — it is the one item here with an external deadline nobody controls.

**Data types.** All five are built and tested: strings, hashes, lists, sets and sorted sets.

Sets and sorted sets were added after the rest, which made them an unplanned test of section 5's claim that `#[non_exhaustive]` from the first release is what keeps growth cheap. It held: `KvValueType` and `KvError` each gained variants, `kvlite-core` gained 26 methods, and nothing already published would have broken. The `public-api.txt` diff showed the whole addition as reviewable lines, which is exactly what that mechanism is for.

Still absent: the lexicographic sorted-set commands (`ZRANGEBYLEX` and friends), which need a bytewise range over members rather than over scores.

**Phase 2:**

- [ ] `kvlite-persistence` extracted; persistence format version documented
- [ ] RDB import, so migration in is possible from day one

**Phase 4–5:**

- [ ] `kvlite-cluster` extracted
- [ ] `kvlite-client`, tested against both Kvlite and real Redis — or dropped, if it cannot beat `redis-rs`
- [ ] Cross-language client compatibility matrix in CI

**Ongoing:**

- [ ] `cargo audit`, `cargo deny`, SBOM per release
- [ ] Miri clean on every `unsafe` block in `kvlite-core`
- [ ] Cold-start, `kvlite-core` clean-build time, and binary size tracked as regression gates

---

*The packaging decisions in sections 3 and 4 constrain code layout from the first commit. Retrofitting them after Phase 3 means moving types across crate boundaries, which is a breaking change for anyone who adopted early.*

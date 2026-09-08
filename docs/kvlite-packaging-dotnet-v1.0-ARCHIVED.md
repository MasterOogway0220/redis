# Kvlite — Packaging & Distribution Specification

**Addendum to the Kvlite PRD v1.0. How consumers take one piece without taking the whole system.**

| | |
|---|---|
| **Version** | 1.0 (draft) |
| **Date** | 8 September 2026 |
| **Status** | For build — constrains Phase 1 onward |
| **Owner** | Aditya |

---

## Contents

1. Why this is an architecture decision, not a release chore
2. The three doors
3. Package split
4. Dependency rules and how they are enforced
5. Public API surface and stability policy
6. Consumption examples
7. Non-.NET consumers
8. Versioning and compatibility
9. Trimming, AOT, and startup cost
10. Documentation requirements per package
11. Release engineering
12. Anti-goals
13. Build checklist

---

## 1. Why this is an architecture decision, not a release chore

"People can use just the part they need" is not something you add at release time. It is a constraint on how the code is written from the first commit, and it is violated the first time someone in the storage engine needs a config value that lives in the server layer.

Every transitive dependency a package carries is a reason somebody cannot adopt it. A caching library that drags in a TCP server, a Raft implementation, a TLS stack and a metrics exporter will lose to one that drags in nothing — regardless of which is technically better. The engineering rule that follows:

> **`Kvlite.Core` has zero third-party package references. Permanently. This is a CI-enforced invariant, not a preference.**

If a feature cannot be built in `Kvlite.Core` without taking a dependency, it does not belong in `Kvlite.Core`.

---

## 2. The three doors

There are exactly three ways someone adopts Kvlite. Each has a different package, a different first-run experience, and a different competitor. Design each door for its own audience.

| Door | What they install | Replaces | Time to first success |
|---|---|---|---|
| **Embedded** | `Kvlite.Core` | `MemoryCache`, `ConcurrentDictionary` + hand-rolled TTL | Under 2 minutes |
| **Test double** | `Kvlite.Testing` | Testcontainers Redis, `docker-compose` in CI | Under 5 minutes |
| **Standalone** | `kvlite` binary / container | Redis, Valkey | Under 15 minutes |

**The test double is the wedge.** It is the lowest-risk way anyone tries this project. Nobody has to trust Kvlite with production data to delete a Redis container from their CI pipeline and cut two minutes off every build. That is a real, immediate, individually-decidable win — and it puts the engine in front of engineers who will later consider door 1 and door 3.

Ship door 2 in Phase 1. It requires no persistence, no clustering, and no replication — only correct data types and a working RESP server, which Phase 1 delivers anyway.

---

## 3. Package split

Nine packages. Each is independently installable, and the dependency graph is strictly acyclic and shallow.

| Package | Contains | Depends on | Third-party deps |
|---|---|---|---|
| **`Kvlite.Abstractions`** | Interfaces and options types only. `IKvStore`, `IKvKeyspace`, `KvOptions`, exception types. No implementation. | — | None |
| **`Kvlite.Protocol`** | RESP2/RESP3 reader and writer, command parsing, reply serialisation. Usable standalone by anyone building a Redis-compatible proxy, sniffer, or mock. | — | None |
| **`Kvlite.Core`** | The engine. Data structures, expiry wheel, eviction, slab arenas, epoch reclamation. No I/O, no network, no disk. | Abstractions | **None — enforced** |
| **`Kvlite.Persistence`** | WAL, group commit, incremental checkpoint, recovery, RDB import. | Core | None |
| **`Kvlite.Server`** | TCP listener, session state, ACL, pub/sub, keyspace notifications, config. | Core, Protocol, Persistence | Minimal |
| **`Kvlite.Cluster`** | Raft control plane, per-shard replication, slot ownership, resharding. | Server | Raft library |
| **`Kvlite.Client`** | .NET client. Speaks RESP, so it works against Kvlite **and** real Redis. | Protocol | None |
| **`Kvlite.Extensions.Caching`** | `IDistributedCache` implementation + DI registration. Backed by either the embedded engine or a remote server. | Abstractions + (Core or Client) | `Microsoft.Extensions.*` |
| **`Kvlite.Testing`** | In-process server on an ephemeral port, xUnit/NUnit fixtures, per-test isolation, deterministic clock. | Server | Test framework adapters |

### Notes on specific packages

**`Kvlite.Protocol` standalone is a deliberate gift.** A clean, allocation-light, dependency-free RESP codec for .NET does not really exist as a standalone package. Publishing one costs nothing extra — you are writing it anyway — and it draws in contributors who have no interest in the storage engine but do care about protocol correctness. Those contributors harden the most security-sensitive component you own.

**`Kvlite.Client` working against real Redis is not a mistake.** It means a team can adopt the client without adopting the server, then switch the connection string later. Never build a client that only talks to your server; that is a lock-in signal, and it halves the number of people who will try it.

**`Kvlite.Extensions.Caching` is the shortest path to .NET adoption.** Most .NET teams do not choose a cache by reading benchmarks. They choose whatever registers in one line in `Program.cs` behind `IDistributedCache`. Ship this in Phase 1.

**`Kvlite.Abstractions` exists so that other libraries can depend on Kvlite without depending on the engine.** A library author writing a rate limiter on top of Kvlite references `Abstractions` only; their users pick the implementation. Without this package, every downstream library forces the full engine on its consumers.

---

## 4. Dependency rules and how they are enforced

Rules stated in a document decay. These are enforced by tests that fail the build.

**Rule 1 — `Kvlite.Core` has no third-party references.**

```csharp
[Fact]
public void Core_HasNoThirdPartyDependencies()
{
    var allowed = new[] { "System.", "netstandard", "Kvlite.Abstractions" };
    var refs = typeof(KvStore).Assembly
        .GetReferencedAssemblies()
        .Select(a => a.Name!);

    Assert.All(refs, r =>
        Assert.True(allowed.Any(p => r!.StartsWith(p, StringComparison.Ordinal)),
            $"Kvlite.Core must not reference {r}"));
}
```

**Rule 2 — layering is one-directional.** `Core` must not reference `Protocol`, `Server`, `Persistence`, or `Cluster`. Assert the same way, per package. The moment the engine needs something from the server, that something is in the wrong layer — move it down to `Abstractions` or pass it in as a parameter.

**Rule 3 — every package installs and runs alone.** CI creates a throwaway console project, adds exactly one Kvlite package from the local feed, runs a two-line smoke test, and fails if it does not compile and execute. Run this for all nine on every release. This catches the most common packaging bug: a package that works in the solution because of a sibling project reference and breaks the instant it is consumed from NuGet.

**Rule 4 — no `InternalsVisibleTo` from a lower layer to a higher one.** It is the standard way layering quietly collapses.

---

## 5. Public API surface and stability policy

Once people depend on this, every public symbol is a commitment. Two mechanisms:

**Track the public API in source control.** Use `Microsoft.CodeAnalysis.PublicApiAnalyzers`. Each package gets `PublicAPI.Shipped.txt` and `PublicAPI.Unshipped.txt`. Any change to the public surface fails the build until the file is updated — which makes the API change a visible line in the pull request diff instead of something noticed after release. This is the single highest-value habit for a library project and it costs one NuGet reference.

**Mark experimental APIs.** New surface ships as `[Experimental("KVL001")]`, which produces a compiler warning the consumer must explicitly suppress. It buys the freedom to change a design after real usage without breaking semver, and it is honest with adopters about what is settled.

**Keep the surface small.** Public by exception, not by default. Everything is `internal` until someone has a concrete reason it should not be. An API you never shipped costs nothing to change.

---

## 6. Consumption examples

These belong in the README of each package, at the top, above everything else. Someone evaluating a library decides in about thirty seconds.

### Door 1 — embedded cache

```csharp
// dotnet add package Kvlite.Core
using var store = new KvStore();

store.Set("session:abc", "user-42", TimeSpan.FromMinutes(30));
var user = store.GetString("session:abc");
store.IncrementBy("page:views", 1);
```

No server, no port, no configuration file, no background process.

### Door 1b — ASP.NET, one line

```csharp
// dotnet add package Kvlite.Extensions.Caching
builder.Services.AddKvliteCache();                       // in-process
builder.Services.AddKvliteCache("localhost:6380");       // remote server
```

Registers `IDistributedCache`. Existing code that already uses `IDistributedCache` needs no changes at all — that is the entire pitch, and it should be the first line of that package's README.

### Door 2 — integration tests

```csharp
// dotnet add package Kvlite.Testing
public class OrderServiceTests : IClassFixture<KvliteFixture>
{
    private readonly KvliteFixture _kvlite;
    public OrderServiceTests(KvliteFixture kvlite) => _kvlite = kvlite;

    [Fact]
    public async Task Reserves_inventory()
    {
        await _kvlite.ResetAsync();                  // clean state per test
        var redis = ConnectionMultiplexer.Connect(_kvlite.ConnectionString);
        // ...existing StackExchange.Redis test code, unchanged
    }
}
```

Requirements for this fixture, all of them non-negotiable:

- Starts in **under 50 ms**. If it is slower than starting a container, there is no reason to use it.
- Binds an **ephemeral port**, so parallel test classes never collide.
- `ResetAsync()` gives a clean keyspace **without a restart**.
- Exposes a **controllable clock**, so a test can advance time and assert TTL expiry without `Thread.Sleep`. Redis cannot do this. It is a genuine capability advantage and should be advertised as one.
- Runs on Linux, Windows, and macOS with no Docker daemon.

### Door 3 — standalone

```bash
docker run -p 6380:6380 kvlite/kvlite:1
# or a single self-contained binary, no runtime install
```

---

## 7. Non-.NET consumers

**Do not write client libraries for other languages.** Wire compatibility already solved this. `redis-py`, `ioredis`, `go-redis`, `Jedis`, `Lettuce`, `phpredis` and `redis-rb` all work against Kvlite today with a changed port number.

What non-.NET users need instead:

1. A **compatibility matrix** stating which clients are tested against Kvlite in CI, and at which versions. Test the top client for each of Python, Node, Go, Java, Ruby and PHP in the integration suite.
2. A **one-page migration note** per ecosystem: change the port, here is what is not yet supported, here is how to import an RDB file.
3. **`redis-cli` compatibility** as a hard requirement. It is how every operator will first poke at the server, and a failure there reads as "this project is not real".

The .NET-specific packages exist because .NET is the ecosystem where an *embedded* store is a differentiated offering. For every other language, being a better Redis server is the whole product.

---

## 8. Versioning and compatibility

- **Semantic versioning**, strictly. Breaking changes only at majors.
- **All packages share a version number** and release together. Mismatched Kvlite package versions in one project are a support burden with no upside; a shared version makes compatibility unambiguous.
- `Kvlite.Core` **1.x supports the 1.x wire protocol**. State the supported protocol range in `INFO` and in the docs.
- **Persistence format is versioned independently** of the package version, with a documented compatibility window: version N reads formats N-2 through N. Never break the ability to read an old checkpoint without a migration path — data people cannot get out of your system is data they will not put into it.
- **Deprecation:** obsolete for one full minor cycle with a message naming the replacement, then remove at the next major. Never remove without a prior warning release.

---

## 9. Trimming, AOT, and startup cost

For a library, these are adoption features, not optimisations.

- **Trim-compatible.** Annotate the assemblies and set `IsTrimmable`. No reflection over user types, no dynamic code generation, no `Assembly.Load` on a startup path.
- **NativeAOT-compatible.** Verify with an AOT-published smoke test in CI. This matters for serverless and desktop consumers, and for the standalone binary's startup time.
- **Cold start under 10 ms** for the embedded engine. Anything slower disqualifies it from short-lived processes and serverless functions.
- **No static mutable state.** Two `KvStore` instances in one process must be fully independent. Tests will create dozens.
- **`net8.0` as the floor**, with `netstandard2.0` for `Kvlite.Abstractions` and `Kvlite.Protocol` only, so older consumers can still integrate against the contracts.

---

## 10. Documentation requirements per package

Every package README, in this order. No exceptions, no rearranging:

1. **One sentence** stating what this package is and — critically — what it does *not* pull in.
2. **A runnable code block**, under ten lines, that does something useful.
3. **When to use this package instead of the others**, with a link to the others.
4. **Dependencies**, listed explicitly. "No third-party dependencies" is a selling point; say it out loud.
5. **Link to full docs.**

Do not put the project's history, motivation, benchmarks, or architecture in a package README. Those belong in the main repository README and the docs site. Someone reading a package README on nuget.org has one question — "does this solve my problem in the next five minutes?" — and everything else on that page is in the way.

---

## 11. Release engineering

- **Publish to NuGet on tag**, from CI, never from a laptop.
- **Sign packages**, and publish with **deterministic builds** and **Source Link** so consumers can step into the source under a debugger.
- **Embed symbols** (`.snupkg`) — a store that cannot be debugged by its users will not be trusted with their data.
- **Publish an SBOM** per release.
- **Container images** are multi-arch (x64, ARM64) with a distroless variant.
- **Release notes** name every public API change, referencing the `PublicAPI.Shipped.txt` diff.
- **Prerelease channel** (`-preview.N`) for anything that has not met its phase exit criteria. Do not ship an unstable engine on a stable version number to hit a date.

---

## 12. Anti-goals

Things that look like good packaging decisions and are not:

- **A single "batteries-included" `Kvlite` package** that references everything. It defeats the entire purpose of the split and becomes what everyone installs by default.
- **A plugin or module loading system.** Redis modules are a large maintenance surface, a security boundary problem, and a versioning nightmare. Composition happens at the package level, at compile time.
- **Client libraries in other languages.** See section 7. This is a large ongoing maintenance commitment that duplicates work the Redis client ecosystem already did well.
- **Public APIs added speculatively** for hypothetical users. Add them when someone asks and can describe the use case.
- **Splitting further than nine packages.** Granularity has a cost too: more READMEs to keep current, more version-matrix combinations to test, more decisions pushed onto the consumer. Nine is enough.

---

## 13. Build checklist

Ordered. Items in Phase 1 are the ones that create adoption; everything else follows.

**Phase 1 (with the data types):**

- [ ] Split the repository into `Kvlite.Abstractions`, `Kvlite.Protocol`, `Kvlite.Core`, `Kvlite.Server`
- [ ] `PublicApiAnalyzers` wired into every package
- [ ] Rule 1 and Rule 2 dependency tests failing the build on violation
- [ ] `Kvlite.Testing` with the xUnit fixture, ephemeral port, `ResetAsync`, controllable clock
- [ ] `Kvlite.Extensions.Caching` with `AddKvliteCache()`
- [ ] Per-package READMEs following the section 10 template
- [ ] Rule 3 isolated-install smoke test in CI
- [ ] NuGet publish on tag, signed, Source Link, symbols

**Phase 2:**

- [ ] `Kvlite.Persistence` extracted; persistence format version documented
- [ ] RDB import, so migration in is possible from day one

**Phase 4–5:**

- [ ] `Kvlite.Cluster` extracted
- [ ] `Kvlite.Client`, tested against both Kvlite and real Redis
- [ ] Cross-language client compatibility matrix in CI

**Ongoing:**

- [ ] AOT and trimming smoke tests
- [ ] Cold-start benchmark tracked as a regression gate

---

*The packaging decisions in sections 3 and 4 constrain code layout from the first commit. Retrofitting them after Phase 3 means moving types across assembly boundaries, which is a breaking change for anyone who adopted early.*

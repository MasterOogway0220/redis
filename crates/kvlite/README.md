# kvlite

The [Kvlite](https://github.com/kvlite/kvlite) server binary: a Redis-compatible key-value server in a single statically linked executable. This crate has **no library target** — there is nothing here to depend on, only something to run.

```bash
cargo install kvlite
kvlite --port 6380

# then, from anywhere
redis-cli -p 6380 ping
```

Kvlite binds `127.0.0.1:6380` by default, not the standard Redis port, so installing it cannot quietly take over from a real Redis on the same machine.

## When to use this instead of the others

Install `kvlite` when you want to **run a server**. Everything else in the project is for building against.

- Want a store inside your own process? Use [`kvlite-core`](https://crates.io/crates/kvlite-core).
- Want to embed the server in your own binary? Use [`kvlite-server`](https://crates.io/crates/kvlite-server).
- Want a Redis substitute in your tests? Use [`kvlite-testing`](https://crates.io/crates/kvlite-testing).

`cargo add kvlite` will not give you a library, and that is deliberate: a batteries-included umbrella crate is an anti-goal of this project, and taking the obvious name for a binary is how we make sure one never appears.

## Options

```
-b, --bind <ADDRESS>       Address to listen on            [default: 127.0.0.1]
-p, --port <PORT>          Port to listen on, 0 for any    [default: 6380]
-d, --databases <COUNT>    Number of keyspaces             [default: 16]
    --sweep-ms <MILLIS>    Expiry sweep interval, 0 to disable   [default: 100]
-h, --help                 Print help
-V, --version              Print the version
```

There is no configuration file and no authentication yet. Bind to loopback.

## Dependencies

`tokio`, plus our own [`kvlite-core`](https://crates.io/crates/kvlite-core) and [`kvlite-server`](https://crates.io/crates/kvlite-server). No CLI framework: six flags do not justify a proc-macro on everyone's install.

## MSRV

MSRV is 1.85.

## Full docs

<https://github.com/kvlite/kvlite>

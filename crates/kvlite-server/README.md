# kvlite-server

A Redis-compatible server over the [Kvlite](https://github.com/kvlite/kvlite) engine. It speaks RESP2 and RESP3 on a TCP port, so `redis-cli` and every Redis client library work against it unchanged. It pulls in **tokio and nothing else** — no TLS stack, no metrics exporter, no clustering.

```rust
use kvlite_server::{Server, ServerConfig};
use std::net::SocketAddr;

# async fn example() -> std::io::Result<()> {
let mut config = ServerConfig::default();
config.bind = SocketAddr::from(([127, 0, 0, 1], 0));   // ephemeral port

let server = Server::bind(config).await?;
println!("listening on {}", server.local_addr());

server.shutdown().await;
# Ok(())
# }
```

Dropping a `Server` stops the listener and every connection it accepted — no leaked task, no held port.

## When to use this crate instead of the others

Use `kvlite-server` when you want to **embed a Redis-compatible endpoint in your own process**: a fixture for a test harness you are building, a sidecar inside a larger binary, or a base to build a proxy on.

- Just want to run one? Install the [`kvlite`](https://crates.io/crates/kvlite) binary instead.
- Want a Redis substitute in your tests? Use [`kvlite-testing`](https://crates.io/crates/kvlite-testing), which wraps this crate with the fixture ergonomics.
- Want a store with no server at all? Use [`kvlite-core`](https://crates.io/crates/kvlite-core).
- Only need to speak the protocol? Use [`kvlite-resp`](https://crates.io/crates/kvlite-resp).

## What is implemented

Connection and admin: `PING` `ECHO` `HELLO` `QUIT` `RESET` `SELECT` `CLIENT` `COMMAND` `CONFIG` `INFO` `TIME` `DBSIZE` `FLUSHDB` `FLUSHALL`.
Keys: `DEL` `UNLINK` `EXISTS` `EXPIRE` `PEXPIRE` `TTL` `PTTL` `PERSIST` `TYPE` `KEYS` `SCAN` `RANDOMKEY`.
Strings: `SET` (with `EX` `PX` `NX` `XX` `KEEPTTL` `GET`) `SETNX` `SETEX` `PSETEX` `GET` `GETSET` `MGET` `MSET` `APPEND` `STRLEN` `INCR` `DECR` `INCRBY` `DECRBY`.
Hashes: `HSET` `HMSET` `HGET` `HMGET` `HDEL` `HEXISTS` `HLEN` `HGETALL` `HKEYS` `HVALS` `HINCRBY`.
Lists: `LPUSH` `RPUSH` `LPOP` `RPOP` `LLEN` `LRANGE` `LINDEX` `LSET` `LTRIM` `LREM`.
Sets: `SADD` `SREM` `SISMEMBER` `SMISMEMBER` `SCARD` `SMEMBERS` `SPOP` `SRANDMEMBER` `SMOVE` `SUNION` `SINTER` `SDIFF` `SUNIONSTORE` `SINTERSTORE` `SDIFFSTORE`.
Sorted sets: `ZADD` (with `NX` `XX` `GT` `LT` `CH` `INCR`) `ZREM` `ZSCORE` `ZMSCORE` `ZCARD` `ZCOUNT` `ZINCRBY` `ZRANK` `ZREVRANK` `ZRANGE` `ZREVRANGE` `ZRANGEBYSCORE` `ZREVRANGEBYSCORE` (with `WITHSCORES` and `LIMIT`) `ZPOPMIN` `ZPOPMAX` `ZREMRANGEBYRANK` `ZREMRANGEBYSCORE`.
Pub/sub: `SUBSCRIBE` `UNSUBSCRIBE` `PSUBSCRIBE` `PUNSUBSCRIBE` `PUBLISH` `PUBSUB`.

Not yet: transactions, scripting, persistence, replication, TLS and authentication, and the lexicographic sorted-set commands (`ZRANGEBYLEX` and friends). `SCAN` returns the whole keyspace in one pass and a cursor of `0`, which satisfies the `SCAN` contract but does not bound the reply. `SPOP` and `SRANDMEMBER` pick members arbitrarily rather than uniformly at random — do not build a lottery on them. `INFO` reports `redis_version:7.4.0` for client compatibility and the real version as `kvlite_version`.

## Dependencies

`tokio`, plus our own [`kvlite-api`](https://crates.io/crates/kvlite-api), [`kvlite-core`](https://crates.io/crates/kvlite-core) and [`kvlite-resp`](https://crates.io/crates/kvlite-resp) — of which only this crate has any third-party dependency at all.

## MSRV

MSRV is 1.85.

## Full docs

<https://docs.rs/kvlite-server>

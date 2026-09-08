//! Door 2, the test double: the root README's examples, driven by a real
//! third-party Redis client.
//!
//! This is the check that a workspace member cannot perform. Section 4 of the
//! packaging spec requires every crate to build in isolation; this goes further and
//! requires it to be *usable* in isolation, by an adopter who reaches for `redis-rs`
//! and follows the README literally.

use std::time::Duration;

/// The headline example. A blocking client in a plain `#[test]`, which is how most
/// Rust codebases actually talk to Redis.
#[test]
fn reserves_inventory() {
    let kv = kvlite_testing::Server::start();

    // Point any Redis client at it. The wire protocol is the whole story.
    let client = redis::Client::open(kv.url()).unwrap();
    let mut conn = client.get_connection().unwrap();

    redis::cmd("SET")
        .arg("cart:abc")
        .arg("2 items")
        .arg("EX")
        .arg(1800)
        .exec(&mut conn)
        .unwrap();

    let value: String = redis::cmd("GET").arg("cart:abc").query(&mut conn).unwrap();
    assert_eq!(value, "2 items");

    // Assert a 30-minute expiry without waiting 30 minutes.
    kv.clock().advance(Duration::from_secs(1801));
    let gone: Option<String> = redis::cmd("GET").arg("cart:abc").query(&mut conn).unwrap();
    assert_eq!(gone, None);

    kv.reset();
    let size: i64 = redis::cmd("DBSIZE").query(&mut conn).unwrap();
    assert_eq!(size, 0);
}

/// The fixture is usable from an async test too, on any runtime flavour. The default
/// `#[tokio::test]` is single-threaded, and a blocking client on it is the exact
/// combination that would deadlock against a fixture sharing the caller's runtime.
#[tokio::test]
async fn a_blocking_client_in_a_single_threaded_async_test() {
    let kv = kvlite_testing::Server::start();
    let client = redis::Client::open(kv.url()).unwrap();
    let mut conn = client.get_connection().unwrap();

    redis::cmd("SADD").arg("tags").arg("a").arg("b").exec(&mut conn).unwrap();
    let count: i64 = redis::cmd("SCARD").arg("tags").query(&mut conn).unwrap();
    assert_eq!(count, 2);
}

/// Seeding and asserting through the store directly, with no round trip.
#[test]
fn reaching_past_the_wire() {
    let kv = kvlite_testing::Server::start();

    kv.store().set("feature:beta", "on", None);
    assert!(kv.store().contains("feature:beta"));

    // ...and the same key is visible over the wire.
    let client = redis::Client::open(kv.url()).unwrap();
    let mut conn = client.get_connection().unwrap();
    let value: String = redis::cmd("GET").arg("feature:beta").query(&mut conn).unwrap();
    assert_eq!(value, "on");
}

/// Every server gets its own port, so tests sharing a process never collide.
#[test]
fn one_server_per_test_is_cheap() {
    let first = kvlite_testing::Server::start();
    let second = kvlite_testing::Server::start();
    assert_ne!(first.port(), second.port());
}

/// The data types over the wire, so the README's claim about them is checked by a
/// client we did not write.
#[test]
fn all_five_data_types_over_the_wire() {
    let kv = kvlite_testing::Server::start();
    let client = redis::Client::open(kv.url()).unwrap();
    let mut conn = client.get_connection().unwrap();

    redis::cmd("SET").arg("s").arg("v").exec(&mut conn).unwrap();
    redis::cmd("HSET").arg("h").arg("f").arg("v").exec(&mut conn).unwrap();
    redis::cmd("RPUSH").arg("l").arg("a").arg("b").exec(&mut conn).unwrap();
    redis::cmd("SADD").arg("t").arg("x").exec(&mut conn).unwrap();
    redis::cmd("ZADD").arg("z").arg(1.5).arg("m").exec(&mut conn).unwrap();

    for (key, expected) in [("s", "string"), ("h", "hash"), ("l", "list"), ("t", "set"), ("z", "zset")]
    {
        let kind: String = redis::cmd("TYPE").arg(key).query(&mut conn).unwrap();
        assert_eq!(kind, expected, "TYPE {key}");
    }

    let score: String = redis::cmd("ZSCORE").arg("z").arg("m").query(&mut conn).unwrap();
    assert_eq!(score, "1.5");
}

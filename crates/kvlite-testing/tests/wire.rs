//! End-to-end tests over a real TCP socket, speaking real RESP.

use std::time::{Duration, Instant};

use kvlite_resp::{Frame, RespProtocol};
use kvlite_testing::Server;
use tokio::net::TcpStream;

mod common;
use common::{Client, text};

// ---- the harness itself ---------------------------------------------------

#[tokio::test]
async fn starts_fast_enough_to_beat_a_container() {
    let started = Instant::now();
    let server = Server::start();
    let elapsed = started.elapsed();

    assert!(server.port() != 0);
    assert!(
        elapsed < Duration::from_millis(50),
        "startup took {elapsed:?}; the whole pitch is that this beats starting a container"
    );
}

#[test]
fn starts_without_a_runtime_of_the_callers_own() {
    // A plain `#[test]`: no async, no tokio in sight. This is how a project using a
    // blocking Redis client should be able to use the fixture.
    let server = Server::start();
    let mut stream = std::net::TcpStream::connect(server.addr()).expect("connect");

    std::io::Write::write_all(&mut stream, b"PING\r\n").expect("write");
    let mut reply = [0u8; 7];
    std::io::Read::read_exact(&mut stream, &mut reply).expect("read");
    assert_eq!(&reply, b"+PONG\r\n");
}

#[tokio::test]
async fn a_blocking_client_cannot_deadlock_a_single_threaded_test() {
    // `#[tokio::test]` is current-thread by default. If the fixture ran on the
    // caller's runtime, this blocking read would starve the accept loop and the test
    // would hang with no error until CI killed it. The fixture owns its runtime
    // precisely so that this cannot happen.
    let server = Server::start();
    let mut stream = std::net::TcpStream::connect(server.addr()).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");

    std::io::Write::write_all(&mut stream, b"SET k v\r\nGET k\r\n").expect("write");

    let mut reply = [0u8; 12];
    std::io::Read::read_exact(&mut stream, &mut reply).expect("read");
    assert_eq!(&reply, b"+OK\r\n$1\r\nv\r\n", "got {:?}", String::from_utf8_lossy(&reply));
}

#[tokio::test]
async fn every_server_gets_its_own_port() {
    // Test threads share a process, so a fixed port would not merely be fragile.
    let first = Server::start();
    let second = Server::start();
    assert_ne!(first.port(), second.port());

    let mut client = Client::connect(&first).await;
    client.call(&[b"SET", b"k", b"v"]).await;

    let mut other = Client::connect(&second).await;
    assert_eq!(other.call(&[b"GET", b"k"]).await, Frame::Null, "servers share nothing");
}

#[tokio::test]
async fn dropping_the_server_releases_the_port() {
    let addr = {
        let server = Server::start();
        let mut client = Client::connect(&server).await;
        assert_eq!(text(&client.call(&[b"PING"]).await), "PONG");
        server.addr()
    };

    // Give the aborted accept task a moment to unwind.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "the listener should be gone once the server is dropped"
    );
}

#[tokio::test]
async fn reset_clears_the_keyspace_without_a_restart() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    client.call(&[b"SET", b"a", b"1"]).await;
    client.call(&[b"SET", b"b", b"2"]).await;
    assert_eq!(client.call(&[b"DBSIZE"]).await, Frame::Integer(2));

    server.reset();

    // Same connection, still usable, empty keyspace.
    assert_eq!(client.call(&[b"DBSIZE"]).await, Frame::Integer(0));
    assert_eq!(text(&client.call(&[b"PING"]).await), "PONG");
}

// ---- protocol -------------------------------------------------------------

#[tokio::test]
async fn answers_the_connection_commands() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    assert_eq!(text(&client.call(&[b"PING"]).await), "PONG");
    assert_eq!(text(&client.call(&[b"PING", b"hi"]).await), "hi");
    assert_eq!(text(&client.call(&[b"ECHO", b"hello"]).await), "hello");

    assert_eq!(text(&client.call(&[b"CLIENT", b"SETNAME", b"suite"]).await), "OK");
    assert_eq!(text(&client.call(&[b"CLIENT", b"GETNAME"]).await), "suite");
    assert!(matches!(client.call(&[b"CLIENT", b"ID"]).await, Frame::Integer(_)));

    let info = text(&client.call(&[b"INFO"]).await);
    assert!(info.contains("redis_version:"), "clients gate features on this line");
    assert!(info.contains("kvlite_version:"));
}

#[tokio::test]
async fn negotiates_resp3_and_replies_with_typed_frames() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    let Frame::Map(fields) = client.call(&[b"HELLO", b"3"]).await else {
        panic!("HELLO must reply with a map");
    };
    client.protocol = RespProtocol::Resp3;

    let proto = fields
        .iter()
        .find(|(key, _)| key.as_str() == Some("proto"))
        .map(|(_, value)| value.clone());
    assert_eq!(proto, Some(Frame::Integer(3)));

    // A miss is a typed null in RESP3, not a length of -1.
    assert_eq!(client.call(&[b"GET", b"absent"]).await, Frame::Null);

    client.call(&[b"HSET", b"h", b"f", b"v"]).await;
    assert_eq!(
        client.call(&[b"HGETALL", b"h"]).await,
        Frame::Map(vec![(Frame::bulk("f"), Frame::bulk("v"))]),
        "HGETALL is a real map on RESP3"
    );

    assert_eq!(text(&client.call(&[b"HELLO", b"4"]).await), "NOPROTO unsupported protocol version");
}

#[tokio::test]
async fn resp2_flattens_what_resp3_types() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    client.call(&[b"HSET", b"h", b"f", b"v"]).await;
    assert_eq!(
        client.call(&[b"HGETALL", b"h"]).await,
        Frame::Array(vec![Frame::bulk("f"), Frame::bulk("v")]),
        "the same reply flattens to an array for a RESP2 client"
    );
    assert_eq!(client.call(&[b"GET", b"absent"]).await, Frame::Null);
}

#[tokio::test]
async fn serves_a_whole_pipeline_from_one_read() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    let mut batch = Vec::new();
    client.encode(&[b"SET", b"k", b"v"], &mut batch);
    client.encode(&[b"INCR", b"n"], &mut batch);
    client.encode(&[b"GET", b"k"], &mut batch);
    client.send_raw(&batch).await;

    assert_eq!(text(&client.read().await), "OK");
    assert_eq!(client.read().await, Frame::Integer(1));
    assert_eq!(text(&client.read().await), "v");
}

#[tokio::test]
async fn accepts_inline_commands() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    // What a health check or a telnet session sends.
    client.send_raw(b"PING\r\n").await;
    assert_eq!(text(&client.read().await), "PONG");

    // A bare newline, and a blank line that must be ignored rather than answered.
    client.send_raw(b"\r\nSET a b\nGET a\r\n").await;
    assert_eq!(text(&client.read().await), "OK");
    assert_eq!(text(&client.read().await), "b");
}

#[tokio::test]
async fn a_protocol_error_is_reported_then_the_connection_closes() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    // A well-formed frame that is not a command. Note that a *truncated* frame is
    // not an error — the server would simply wait for the rest, forever.
    client.send_raw(b"*1\r\n+notbulk\r\n").await;
    assert!(text(&client.read().await).starts_with("ERR Protocol error"));
    assert!(client.is_closed().await);
}

#[tokio::test]
async fn quit_replies_before_hanging_up() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    assert_eq!(text(&client.call(&[b"QUIT"]).await), "OK");
    assert!(client.is_closed().await);
}

// ---- commands -------------------------------------------------------------

#[tokio::test]
async fn strings_and_counters_over_the_wire() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    assert_eq!(text(&client.call(&[b"SET", b"k", b"v"]).await), "OK");
    assert_eq!(text(&client.call(&[b"GET", b"k"]).await), "v");
    assert_eq!(client.call(&[b"EXISTS", b"k", b"k", b"nope"]).await, Frame::Integer(2));
    assert_eq!(client.call(&[b"STRLEN", b"k"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"APPEND", b"k", b"2"]).await, Frame::Integer(2));

    assert_eq!(client.call(&[b"INCR", b"n"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"INCRBY", b"n", b"9"]).await, Frame::Integer(10));
    assert_eq!(client.call(&[b"DECRBY", b"n", b"4"]).await, Frame::Integer(6));
    assert!(text(&client.call(&[b"INCR", b"k"]).await).starts_with("ERR value is not an integer"));

    assert_eq!(text(&client.call(&[b"MSET", b"a", b"1", b"b", b"2"]).await), "OK");
    assert_eq!(
        client.call(&[b"MGET", b"a", b"b", b"absent"]).await,
        Frame::Array(vec![Frame::bulk("1"), Frame::bulk("2"), Frame::Null])
    );

    assert_eq!(client.call(&[b"SETNX", b"a", b"x"]).await, Frame::Integer(0));
    assert_eq!(client.call(&[b"SET", b"a", b"x", b"XX"]).await, Frame::ok());
    assert_eq!(client.call(&[b"SET", b"a", b"y", b"NX"]).await, Frame::Null);
    assert_eq!(client.call(&[b"DEL", b"a", b"b"]).await, Frame::Integer(2));
}

#[tokio::test]
async fn reports_wrongtype_with_the_redis_error_code() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    client.call(&[b"RPUSH", b"l", b"a"]).await;
    assert!(
        text(&client.call(&[b"GET", b"l"]).await).starts_with("WRONGTYPE "),
        "clients match on the code, so it has to be exact"
    );
    assert_eq!(text(&client.call(&[b"TYPE", b"l"]).await), "list");
    assert_eq!(text(&client.call(&[b"TYPE", b"absent"]).await), "none");
}

#[tokio::test]
async fn hashes_and_lists_over_the_wire() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    assert_eq!(client.call(&[b"HSET", b"h", b"a", b"1", b"b", b"2"]).await, Frame::Integer(2));
    assert_eq!(client.call(&[b"HLEN", b"h"]).await, Frame::Integer(2));
    assert_eq!(text(&client.call(&[b"HGET", b"h", b"a"]).await), "1");
    assert_eq!(client.call(&[b"HINCRBY", b"h", b"a", b"4"]).await, Frame::Integer(5));
    assert_eq!(
        client.call(&[b"HMGET", b"h", b"a", b"missing"]).await,
        Frame::Array(vec![Frame::bulk("5"), Frame::Null])
    );
    assert_eq!(client.call(&[b"HDEL", b"h", b"a", b"b"]).await, Frame::Integer(2));
    assert_eq!(client.call(&[b"EXISTS", b"h"]).await, Frame::Integer(0));

    assert_eq!(client.call(&[b"RPUSH", b"l", b"a", b"b", b"c"]).await, Frame::Integer(3));
    assert_eq!(client.call(&[b"LPUSH", b"l", b"z"]).await, Frame::Integer(4));
    assert_eq!(
        client.call(&[b"LRANGE", b"l", b"0", b"-1"]).await,
        Frame::Array([b"z", b"a", b"b", b"c"].into_iter().map(Frame::bulk).collect::<Vec<_>>())
    );
    assert_eq!(text(&client.call(&[b"LINDEX", b"l", b"-1"]).await), "c");
    assert_eq!(text(&client.call(&[b"LPOP", b"l"]).await), "z");
    assert_eq!(text(&client.call(&[b"LTRIM", b"l", b"0", b"0"]).await), "OK");
    assert_eq!(client.call(&[b"LLEN", b"l"]).await, Frame::Integer(1));
}

#[tokio::test]
async fn ttls_expire_when_the_test_says_so() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    assert_eq!(text(&client.call(&[b"SET", b"s", b"v", b"EX", b"1800"]).await), "OK");
    assert_eq!(client.call(&[b"TTL", b"s"]).await, Frame::Integer(1800));

    server.clock().advance(Duration::from_secs(1799));
    assert_eq!(client.call(&[b"TTL", b"s"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"EXISTS", b"s"]).await, Frame::Integer(1));

    server.clock().advance(Duration::from_secs(1));
    assert_eq!(client.call(&[b"EXISTS", b"s"]).await, Frame::Integer(0));
    assert_eq!(client.call(&[b"TTL", b"s"]).await, Frame::Integer(-2), "-2 means no such key");

    client.call(&[b"SET", b"p", b"v"]).await;
    assert_eq!(client.call(&[b"TTL", b"p"]).await, Frame::Integer(-1), "-1 means no expiry");
    assert_eq!(client.call(&[b"EXPIRE", b"p", b"60"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"PERSIST", b"p"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"PERSIST", b"p"]).await, Frame::Integer(0));

    // A non-positive expiry deletes the key, as Redis does.
    assert_eq!(client.call(&[b"EXPIRE", b"p", b"0"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"EXISTS", b"p"]).await, Frame::Integer(0));
}

#[tokio::test]
async fn keyspaces_are_selectable_and_isolated() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    client.call(&[b"SET", b"k", b"zero"]).await;
    assert_eq!(text(&client.call(&[b"SELECT", b"1"]).await), "OK");
    assert_eq!(client.call(&[b"GET", b"k"]).await, Frame::Null);

    client.call(&[b"SET", b"k", b"one"]).await;
    client.call(&[b"FLUSHDB"]).await;
    assert_eq!(client.call(&[b"GET", b"k"]).await, Frame::Null);

    client.call(&[b"SELECT", b"0"]).await;
    assert_eq!(text(&client.call(&[b"GET", b"k"]).await), "zero", "FLUSHDB is not FLUSHALL");

    assert!(text(&client.call(&[b"SELECT", b"999"]).await).starts_with("ERR DB index"));
}

#[tokio::test]
async fn keys_and_scan_match_globs() {
    let server = Server::start();
    let mut client = Client::connect(&server).await;

    for key in [&b"user:1"[..], b"user:2", b"post:1"] {
        client.call(&[b"SET", key, b"v"]).await;
    }

    let Frame::Array(mut matched) = client.call(&[b"KEYS", b"user:*"]).await else {
        panic!("KEYS replies with an array");
    };
    matched.sort_by_key(|frame| frame.as_bytes().map(<[u8]>::to_vec));
    assert_eq!(matched, vec![Frame::bulk("user:1"), Frame::bulk("user:2")]);

    let Frame::Array(page) = client.call(&[b"SCAN", b"0", b"MATCH", b"post:*"]).await else {
        panic!("SCAN replies with a cursor and a page");
    };
    assert_eq!(page[0], Frame::bulk("0"), "the cursor comes back to zero");
    assert_eq!(page[1], Frame::Array(vec![Frame::bulk("post:1")]));
}

// ---- pub/sub --------------------------------------------------------------

#[tokio::test]
async fn delivers_messages_to_subscribers() {
    let server = Server::start();
    let mut subscriber = Client::connect(&server).await;
    let mut publisher = Client::connect(&server).await;

    assert_eq!(
        subscriber.call(&[b"SUBSCRIBE", b"news"]).await,
        Frame::Array(vec![Frame::bulk("subscribe"), Frame::bulk("news"), Frame::Integer(1)])
    );

    assert_eq!(publisher.call(&[b"PUBLISH", b"news", b"hello"]).await, Frame::Integer(1));
    assert_eq!(
        subscriber.read().await,
        Frame::Array(vec![Frame::bulk("message"), Frame::bulk("news"), Frame::bulk("hello")])
    );

    // A RESP2 subscriber is restricted to the pub/sub commands.
    assert!(text(&subscriber.call(&[b"GET", b"k"]).await).starts_with("ERR Can't execute 'get'"));

    assert_eq!(
        subscriber.call(&[b"UNSUBSCRIBE", b"news"]).await,
        Frame::Array(vec![Frame::bulk("unsubscribe"), Frame::bulk("news"), Frame::Integer(0)])
    );
    assert_eq!(publisher.call(&[b"PUBLISH", b"news", b"gone"]).await, Frame::Integer(0));
    assert_eq!(
        subscriber.call(&[b"GET", b"k"]).await,
        Frame::Null,
        "GET is allowed again once the last subscription is gone"
    );
}

#[tokio::test]
async fn delivers_to_pattern_subscribers() {
    let server = Server::start();
    let mut subscriber = Client::connect(&server).await;
    let mut publisher = Client::connect(&server).await;

    subscriber.call(&[b"PSUBSCRIBE", b"news.*"]).await;
    assert_eq!(publisher.call(&[b"PUBLISH", b"news.tech", b"body"]).await, Frame::Integer(1));

    assert_eq!(
        subscriber.read().await,
        Frame::Array(vec![
            Frame::bulk("pmessage"),
            Frame::bulk("news.*"),
            Frame::bulk("news.tech"),
            Frame::bulk("body"),
        ])
    );

    assert_eq!(
        publisher.call(&[b"PUBLISH", b"sport.today", b"body"]).await,
        Frame::Integer(0),
        "a non-matching channel reaches nobody"
    );
}

#[tokio::test]
async fn a_dropped_subscriber_is_forgotten() {
    let server = Server::start();
    let mut publisher = Client::connect(&server).await;

    {
        let mut subscriber = Client::connect(&server).await;
        subscriber.call(&[b"SUBSCRIBE", b"news"]).await;
        assert_eq!(publisher.call(&[b"PUBLISH", b"news", b"x"]).await, Frame::Integer(1));
    }

    // Let the connection task notice the closed socket and deregister.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        publisher.call(&[b"PUBLISH", b"news", b"x"]).await,
        Frame::Integer(0),
        "subscriptions must not outlive their connection"
    );
    assert_eq!(publisher.call(&[b"PUBSUB", b"CHANNELS"]).await, Frame::Array(Vec::new()));
}

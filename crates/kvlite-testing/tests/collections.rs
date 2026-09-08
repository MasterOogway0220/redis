//! Sets and sorted sets over a real TCP socket, speaking real RESP.

use kvlite_resp::{Frame, RespProtocol};
use kvlite_testing::Server;

mod common;
use common::{Client, members, sorted_members, text};

async fn client() -> (Server, Client) {
    let server = Server::start();
    let client = Client::connect(&server).await;
    (server, client)
}

// ---- sets -----------------------------------------------------------------

#[tokio::test]
async fn sets_add_read_and_remove() {
    let (_server, mut client) = client().await;

    assert_eq!(client.call(&[b"SADD", b"s", b"a", b"b", b"a"]).await, Frame::Integer(2));
    assert_eq!(client.call(&[b"SADD", b"s", b"c"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"SCARD", b"s"]).await, Frame::Integer(3));
    assert_eq!(text(&client.call(&[b"TYPE", b"s"]).await), "set");

    assert_eq!(client.call(&[b"SISMEMBER", b"s", b"a"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"SISMEMBER", b"s", b"z"]).await, Frame::Integer(0));
    assert_eq!(
        client.call(&[b"SMISMEMBER", b"s", b"a", b"z"]).await,
        Frame::Array(vec![Frame::Integer(1), Frame::Integer(0)])
    );

    assert_eq!(sorted_members(&client.call(&[b"SMEMBERS", b"s"]).await), ["a", "b", "c"]);

    assert_eq!(client.call(&[b"SREM", b"s", b"a", b"missing"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"SREM", b"s", b"b", b"c"]).await, Frame::Integer(2));
    assert_eq!(client.call(&[b"EXISTS", b"s"]).await, Frame::Integer(0), "an emptied set goes");
}

#[tokio::test]
async fn set_membership_commands_report_wrongtype() {
    let (_server, mut client) = client().await;
    client.call(&[b"SET", b"str", b"v"]).await;

    assert!(text(&client.call(&[b"SADD", b"str", b"a"]).await).starts_with("WRONGTYPE "));
    assert!(text(&client.call(&[b"SMEMBERS", b"str"]).await).starts_with("WRONGTYPE "));

    client.call(&[b"SADD", b"s", b"a"]).await;
    assert!(text(&client.call(&[b"GET", b"s"]).await).starts_with("WRONGTYPE "));
    assert!(text(&client.call(&[b"LLEN", b"s"]).await).starts_with("WRONGTYPE "));
}

#[tokio::test]
async fn popping_and_sampling_a_set() {
    let (_server, mut client) = client().await;
    client.call(&[b"SADD", b"s", b"a", b"b", b"c"]).await;

    // A bare SPOP returns one member, not a one-element array.
    let popped = client.call(&[b"SPOP", b"s"]).await;
    assert!(matches!(popped, Frame::Bulk(_)), "got {popped:?}");
    assert_eq!(client.call(&[b"SCARD", b"s"]).await, Frame::Integer(2));

    assert_eq!(members(&client.call(&[b"SPOP", b"s", b"5"]).await).len(), 2, "capped at the size");
    assert_eq!(client.call(&[b"EXISTS", b"s"]).await, Frame::Integer(0));

    client.call(&[b"SADD", b"s", b"a", b"b"]).await;
    assert_eq!(members(&client.call(&[b"SRANDMEMBER", b"s", b"5"]).await).len(), 2);
    assert_eq!(
        members(&client.call(&[b"SRANDMEMBER", b"s", b"-5"]).await).len(),
        5,
        "a negative count returns exactly that many, repeating as needed"
    );
    assert_eq!(client.call(&[b"SCARD", b"s"]).await, Frame::Integer(2), "nothing was removed");

    assert!(
        text(&client.call(&[b"SPOP", b"s", b"-1"]).await).starts_with("ERR value is out of range")
    );
}

#[tokio::test]
async fn set_algebra_over_the_wire() {
    let (_server, mut client) = client().await;
    client.call(&[b"SADD", b"a", b"1", b"2", b"3"]).await;
    client.call(&[b"SADD", b"b", b"2", b"3", b"4"]).await;

    assert_eq!(sorted_members(&client.call(&[b"SUNION", b"a", b"b"]).await), ["1", "2", "3", "4"]);
    assert_eq!(sorted_members(&client.call(&[b"SINTER", b"a", b"b"]).await), ["2", "3"]);
    assert_eq!(sorted_members(&client.call(&[b"SDIFF", b"a", b"b"]).await), ["1"]);

    // A missing key is the empty set.
    assert_eq!(
        sorted_members(&client.call(&[b"SINTER", b"a", b"gone"]).await),
        Vec::<String>::new()
    );
    assert_eq!(sorted_members(&client.call(&[b"SDIFF", b"a", b"gone"]).await), ["1", "2", "3"]);

    assert_eq!(client.call(&[b"SINTERSTORE", b"dest", b"a", b"b"]).await, Frame::Integer(2));
    assert_eq!(sorted_members(&client.call(&[b"SMEMBERS", b"dest"]).await), ["2", "3"]);

    // An empty result deletes the destination rather than storing an empty set.
    assert_eq!(client.call(&[b"SINTERSTORE", b"dest", b"a", b"gone"]).await, Frame::Integer(0));
    assert_eq!(client.call(&[b"EXISTS", b"dest"]).await, Frame::Integer(0));
}

#[tokio::test]
async fn moving_a_member_between_sets() {
    let (_server, mut client) = client().await;
    client.call(&[b"SADD", b"from", b"x", b"y"]).await;
    client.call(&[b"SADD", b"to", b"z"]).await;

    assert_eq!(client.call(&[b"SMOVE", b"from", b"to", b"x"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"SISMEMBER", b"from", b"x"]).await, Frame::Integer(0));
    assert_eq!(client.call(&[b"SISMEMBER", b"to", b"x"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"SMOVE", b"from", b"to", b"gone"]).await, Frame::Integer(0));
}

#[tokio::test]
async fn a_set_is_a_real_set_on_resp3() {
    let (_server, mut client) = client().await;
    client.call(&[b"HELLO", b"3"]).await;
    client.protocol = RespProtocol::Resp3;

    client.call(&[b"SADD", b"s", b"a"]).await;
    let reply = client.call(&[b"SMEMBERS", b"s"]).await;
    assert!(matches!(reply, Frame::Set(_)), "RESP3 has a set type; use it. Got {reply:?}");
}

// ---- sorted sets ----------------------------------------------------------

async fn seeded() -> (Server, Client) {
    let (server, mut client) = client().await;
    client.call(&[b"ZADD", b"z", b"1", b"a", b"2", b"b", b"2", b"c", b"3", b"d"]).await;
    (server, client)
}

#[tokio::test]
async fn sorted_sets_order_by_score_then_member() {
    let (_server, mut client) = seeded().await;

    assert_eq!(client.call(&[b"ZCARD", b"z"]).await, Frame::Integer(4));
    assert_eq!(text(&client.call(&[b"TYPE", b"z"]).await), "zset");
    assert_eq!(
        members(&client.call(&[b"ZRANGE", b"z", b"0", b"-1"]).await),
        ["a", "b", "c", "d"],
        "b and c share a score, so they fall in member order"
    );
    assert_eq!(
        members(&client.call(&[b"ZREVRANGE", b"z", b"0", b"-1"]).await),
        ["d", "c", "b", "a"]
    );
    assert_eq!(members(&client.call(&[b"ZRANGE", b"z", b"1", b"2"]).await), ["b", "c"]);
    assert_eq!(members(&client.call(&[b"ZRANGE", b"z", b"-2", b"-1"]).await), ["c", "d"]);
}

#[tokio::test]
async fn scores_come_back_the_way_redis_prints_them() {
    let (_server, mut client) = client().await;
    client.call(&[b"ZADD", b"z", b"1", b"whole", b"1.5", b"half", b"inf", b"top"]).await;

    assert_eq!(text(&client.call(&[b"ZSCORE", b"z", b"whole"]).await), "1", "no trailing .0");
    assert_eq!(text(&client.call(&[b"ZSCORE", b"z", b"half"]).await), "1.5");
    assert_eq!(text(&client.call(&[b"ZSCORE", b"z", b"top"]).await), "inf");
    assert_eq!(client.call(&[b"ZSCORE", b"z", b"missing"]).await, Frame::Null);

    assert_eq!(
        client.call(&[b"ZMSCORE", b"z", b"whole", b"missing"]).await,
        Frame::Array(vec![Frame::bulk("1"), Frame::Null])
    );
}

#[tokio::test]
async fn withscores_flattens_members_and_scores() {
    let (_server, mut client) = seeded().await;

    assert_eq!(
        members(&client.call(&[b"ZRANGE", b"z", b"0", b"1", b"WITHSCORES"]).await),
        ["a", "1", "b", "2"]
    );
    assert!(
        text(&client.call(&[b"ZRANGE", b"z", b"0", b"1", b"NONSENSE"]).await)
            .starts_with("ERR syntax error")
    );
}

#[tokio::test]
async fn zadd_flags_map_to_the_engine() {
    let (_server, mut client) = client().await;

    // XX cannot create; NX cannot update.
    assert_eq!(client.call(&[b"ZADD", b"z", b"XX", b"1", b"a"]).await, Frame::Integer(0));
    assert_eq!(client.call(&[b"EXISTS", b"z"]).await, Frame::Integer(0));

    assert_eq!(client.call(&[b"ZADD", b"z", b"5", b"a"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"ZADD", b"z", b"NX", b"1", b"a"]).await, Frame::Integer(0));
    assert_eq!(text(&client.call(&[b"ZSCORE", b"z", b"a"]).await), "5");

    // CH reports changes rather than additions.
    assert_eq!(client.call(&[b"ZADD", b"z", b"9", b"a"]).await, Frame::Integer(0));
    assert_eq!(client.call(&[b"ZADD", b"z", b"CH", b"7", b"a"]).await, Frame::Integer(1));

    // GT only raises, LT only lowers.
    client.call(&[b"ZADD", b"z", b"GT", b"3", b"a"]).await;
    assert_eq!(text(&client.call(&[b"ZSCORE", b"z", b"a"]).await), "7");
    client.call(&[b"ZADD", b"z", b"LT", b"3", b"a"]).await;
    assert_eq!(text(&client.call(&[b"ZSCORE", b"z", b"a"]).await), "3");

    // INCR replies with the new score, not a count.
    assert_eq!(text(&client.call(&[b"ZADD", b"z", b"INCR", b"2", b"a"]).await), "5");
    assert_eq!(
        client.call(&[b"ZADD", b"z", b"NX", b"INCR", b"2", b"a"]).await,
        Frame::Null,
        "a declined INCR replies with a null"
    );

    assert!(
        text(&client.call(&[b"ZADD", b"z", b"GT", b"NX", b"1", b"a"]).await).starts_with("ERR ")
    );
    assert!(
        text(&client.call(&[b"ZADD", b"z", b"1"]).await).starts_with("ERR syntax error"),
        "an odd number of score/member arguments is a syntax error"
    );
    assert!(
        text(&client.call(&[b"ZADD", b"z", b"nan", b"a"]).await)
            .starts_with("ERR value is not a valid float")
    );
}

#[tokio::test]
async fn ranks_and_counts() {
    let (_server, mut client) = seeded().await;

    assert_eq!(client.call(&[b"ZRANK", b"z", b"a"]).await, Frame::Integer(0));
    assert_eq!(client.call(&[b"ZRANK", b"z", b"d"]).await, Frame::Integer(3));
    assert_eq!(client.call(&[b"ZREVRANK", b"z", b"a"]).await, Frame::Integer(3));
    assert_eq!(client.call(&[b"ZRANK", b"z", b"missing"]).await, Frame::Null);

    assert_eq!(client.call(&[b"ZCOUNT", b"z", b"-inf", b"+inf"]).await, Frame::Integer(4));
    assert_eq!(client.call(&[b"ZCOUNT", b"z", b"2", b"2"]).await, Frame::Integer(2));
    assert_eq!(client.call(&[b"ZCOUNT", b"z", b"(2", b"+inf"]).await, Frame::Integer(1));
    assert!(text(&client.call(&[b"ZCOUNT", b"z", b"x", b"2"]).await).starts_with("ERR min or max"));
}

#[tokio::test]
async fn range_by_score_handles_bounds_limits_and_direction() {
    let (_server, mut client) = seeded().await;

    assert_eq!(
        members(&client.call(&[b"ZRANGEBYSCORE", b"z", b"-inf", b"+inf"]).await),
        ["a", "b", "c", "d"]
    );
    assert_eq!(members(&client.call(&[b"ZRANGEBYSCORE", b"z", b"2", b"2"]).await), ["b", "c"]);
    assert_eq!(
        members(&client.call(&[b"ZRANGEBYSCORE", b"z", b"(1", b"(3"]).await),
        ["b", "c"],
        "a leading ( makes a bound exclusive"
    );

    // ZREVRANGEBYSCORE takes its bounds the other way round.
    assert_eq!(
        members(&client.call(&[b"ZREVRANGEBYSCORE", b"z", b"+inf", b"2"]).await),
        ["d", "c", "b"]
    );

    assert_eq!(
        members(
            &client.call(&[b"ZRANGEBYSCORE", b"z", b"-inf", b"+inf", b"LIMIT", b"1", b"2"]).await
        ),
        ["b", "c"]
    );
    assert_eq!(
        members(
            &client.call(&[b"ZRANGEBYSCORE", b"z", b"-inf", b"+inf", b"LIMIT", b"2", b"-1"]).await
        ),
        ["c", "d"],
        "a negative count means to the end"
    );
    assert_eq!(
        members(
            &client
                .call(&[
                    b"ZRANGEBYSCORE",
                    b"z",
                    b"-inf",
                    b"+inf",
                    b"WITHSCORES",
                    b"LIMIT",
                    b"0",
                    b"1"
                ])
                .await
        ),
        ["a", "1"]
    );
}

#[tokio::test]
async fn incrementing_and_popping_scores() {
    let (_server, mut client) = seeded().await;

    assert_eq!(text(&client.call(&[b"ZINCRBY", b"z", b"1.5", b"a"]).await), "2.5");
    assert_eq!(text(&client.call(&[b"ZINCRBY", b"z", b"1", b"brand-new"]).await), "1");
    assert!(
        text(&client.call(&[b"ZINCRBY", b"z", b"nope", b"a"]).await)
            .starts_with("ERR value is not a valid float")
    );

    let popped = members(&client.call(&[b"ZPOPMIN", b"z"]).await);
    assert_eq!(popped, ["brand-new", "1"], "ZPOPMIN replies with member and score");

    assert_eq!(members(&client.call(&[b"ZPOPMAX", b"z", b"2"]).await), ["d", "3", "a", "2.5"]);
    assert_eq!(client.call(&[b"ZCARD", b"z"]).await, Frame::Integer(2));
}

#[tokio::test]
async fn removing_by_member_rank_and_score() {
    let (_server, mut client) = seeded().await;

    assert_eq!(client.call(&[b"ZREM", b"z", b"a", b"missing"]).await, Frame::Integer(1));
    assert_eq!(client.call(&[b"ZREMRANGEBYRANK", b"z", b"0", b"0"]).await, Frame::Integer(1));
    assert_eq!(members(&client.call(&[b"ZRANGE", b"z", b"0", b"-1"]).await), ["c", "d"]);

    assert_eq!(client.call(&[b"ZREMRANGEBYSCORE", b"z", b"3", b"+inf"]).await, Frame::Integer(1));
    assert_eq!(members(&client.call(&[b"ZRANGE", b"z", b"0", b"-1"]).await), ["c"]);

    client.call(&[b"ZREM", b"z", b"c"]).await;
    assert_eq!(client.call(&[b"EXISTS", b"z"]).await, Frame::Integer(0));
}

#[tokio::test]
async fn sorted_set_commands_report_wrongtype() {
    let (_server, mut client) = client().await;
    client.call(&[b"SET", b"str", b"v"]).await;

    assert!(text(&client.call(&[b"ZADD", b"str", b"1", b"a"]).await).starts_with("WRONGTYPE "));
    assert!(text(&client.call(&[b"ZSCORE", b"str", b"a"]).await).starts_with("WRONGTYPE "));
    assert!(text(&client.call(&[b"ZRANGE", b"str", b"0", b"-1"]).await).starts_with("WRONGTYPE "));

    client.call(&[b"ZADD", b"z", b"1", b"a"]).await;
    assert!(text(&client.call(&[b"SMEMBERS", b"z"]).await).starts_with("WRONGTYPE "));
    assert!(text(&client.call(&[b"GET", b"z"]).await).starts_with("WRONGTYPE "));
}

#[tokio::test]
async fn collections_expire_when_the_test_says_so() {
    let (server, mut client) = client().await;

    client.call(&[b"SADD", b"s", b"a"]).await;
    client.call(&[b"ZADD", b"z", b"1", b"a"]).await;
    client.call(&[b"EXPIRE", b"s", b"60"]).await;
    client.call(&[b"EXPIRE", b"z", b"60"]).await;

    server.clock().advance(std::time::Duration::from_secs(61));

    assert_eq!(client.call(&[b"SCARD", b"s"]).await, Frame::Integer(0));
    assert_eq!(client.call(&[b"ZCARD", b"z"]).await, Frame::Integer(0));
    assert_eq!(text(&client.call(&[b"TYPE", b"s"]).await), "none");
    assert_eq!(text(&client.call(&[b"TYPE", b"z"]).await), "none");
}

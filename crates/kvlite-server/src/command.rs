//! Command dispatch.
//!
//! One function per family, one `match` at the top. Adding a command is adding an
//! arm; there is no registry, no trait object per command, and no macro. That is
//! deliberate — a dispatch table of this size is easier to read as a match than as
//! anything cleverer.

use std::ops::Bound;
use std::time::Duration;

use kvlite_api::{KvError, KvWriteCondition};
use kvlite_core::{KvKeyspace, format_score, parse_int, parse_score};
use kvlite_resp::{Frame, RespProtocol};

use crate::glob;
use crate::state::{ServerState, Session};

/// What the connection loop should do after a command.
pub(crate) enum Action {
    /// Send these frames, in order.
    Reply(Vec<Frame>),
    /// Send these frames, then close the connection.
    Close(Vec<Frame>),
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Redis version we claim in `INFO` and `HELLO`.
///
/// Clients gate features on this, so reporting our own version would make them
/// disable things that work. `INFO` also carries `kvlite_version`, which is the
/// honest one.
const REDIS_COMPAT_VERSION: &str = "7.4.0";

pub(crate) fn dispatch(session: &mut Session, state: &ServerState, args: &[Vec<u8>]) -> Action {
    let Some(name) = args.first() else {
        return reply(Frame::Null);
    };
    let command = name.to_ascii_uppercase();
    let rest = &args[1..];

    // A RESP2 connection in subscribe mode may only run a handful of commands.
    // RESP3 lifted that restriction, because pushes are distinguishable there.
    if session.protocol == RespProtocol::Resp2
        && session.is_subscribed()
        && !allowed_while_subscribed(&command)
    {
        return error(format!(
            "ERR Can't execute '{}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context",
            String::from_utf8_lossy(name).to_lowercase()
        ));
    }

    match command.as_slice() {
        // ---- connection ---------------------------------------------------
        b"PING" => match rest {
            [] if session.is_subscribed() && session.protocol == RespProtocol::Resp2 => {
                reply(Frame::Array(vec![Frame::bulk("pong"), Frame::bulk("")]))
            }
            [] => reply(Frame::simple("PONG")),
            [message] => reply(Frame::Bulk(message.clone())),
            _ => wrong_arity("ping"),
        },
        b"ECHO" => match rest {
            [message] => reply(Frame::Bulk(message.clone())),
            _ => wrong_arity("echo"),
        },
        b"QUIT" => Action::Close(vec![Frame::ok()]),
        b"HELLO" => hello(session, rest),
        b"RESET" => {
            for channel in std::mem::take(&mut session.channels) {
                state.pubsub.unsubscribe(session.id, &channel);
            }
            for pattern in std::mem::take(&mut session.patterns) {
                state.pubsub.unsubscribe_pattern(session.id, &pattern);
            }
            session.keyspace = 0;
            session.name.clear();
            reply(Frame::simple("RESET"))
        }
        b"SELECT" => match rest {
            [index] => match parse_int(index) {
                Some(index) if index >= 0 && (index as usize) < state.store.keyspace_count() => {
                    session.keyspace = index as usize;
                    ok()
                }
                Some(_) => error("ERR DB index is out of range"),
                None => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("select"),
        },
        b"CLIENT" => client(session, rest),
        b"COMMAND" => match rest.first().map(|s| s.to_ascii_uppercase()).as_deref() {
            Some(b"COUNT") => reply(Frame::Integer(0)),
            _ => reply(Frame::Array(Vec::new())),
        },
        b"CONFIG" => config(state, rest),
        b"INFO" => reply(Frame::Bulk(info(state).into_bytes())),
        b"TIME" => {
            let now = state.store.clock().now_millis();
            reply(Frame::Array(vec![
                Frame::bulk((now / 1000).to_string()),
                Frame::bulk((now % 1000 * 1000).to_string()),
            ]))
        }

        // ---- keyspace admin -----------------------------------------------
        b"DBSIZE" => reply(Frame::Integer(keyspace(state, session).len() as i64)),
        b"FLUSHDB" => {
            keyspace(state, session).clear();
            ok()
        }
        b"FLUSHALL" => {
            state.store.clear();
            ok()
        }

        // ---- keys ----------------------------------------------------------
        b"DEL" | b"UNLINK" => {
            if rest.is_empty() {
                return wrong_arity("del");
            }
            let keyspace = keyspace(state, session);
            let removed = rest.iter().filter(|key| keyspace.remove(key)).count();
            reply(Frame::Integer(removed as i64))
        }
        b"EXISTS" => {
            if rest.is_empty() {
                return wrong_arity("exists");
            }
            let keyspace = keyspace(state, session);
            // Redis counts a repeated key once per repetition.
            let found = rest.iter().filter(|key| keyspace.contains(key)).count();
            reply(Frame::Integer(found as i64))
        }
        b"EXPIRE" => expire(state, session, rest, 1000, "expire"),
        b"PEXPIRE" => expire(state, session, rest, 1, "pexpire"),
        b"TTL" => time_to_live(state, session, rest, 1000, "ttl"),
        b"PTTL" => time_to_live(state, session, rest, 1, "pttl"),
        b"PERSIST" => match rest {
            [key] => {
                let keyspace = keyspace(state, session);
                let had_expiry = keyspace.time_to_live(key).is_some();
                reply(Frame::Integer(i64::from(had_expiry && keyspace.expire(key, None))))
            }
            _ => wrong_arity("persist"),
        },
        b"TYPE" => match rest {
            [key] => reply(Frame::simple(keyspace(state, session).kind(key).wire_name())),
            _ => wrong_arity("type"),
        },
        b"KEYS" => match rest {
            [pattern] => reply(Frame::Array(
                keyspace(state, session)
                    .keys()
                    .into_iter()
                    .filter(|key| glob::matches(pattern, key))
                    .map(Frame::Bulk)
                    .collect(),
            )),
            _ => wrong_arity("keys"),
        },
        b"SCAN" => scan(state, session, rest),
        b"RANDOMKEY" => {
            let keys = keyspace(state, session).keys();
            // ponytail: pick by clock parity rather than pulling in a PRNG. Nothing
            // depends on this being uniform; swap in a real PRNG if anything ever does.
            match keys.is_empty() {
                true => reply(Frame::Null),
                false => {
                    let index = (state.store.clock().now_millis() as usize) % keys.len();
                    reply(Frame::Bulk(keys[index].clone()))
                }
            }
        }

        // ---- strings --------------------------------------------------------
        b"SET" => set(state, session, rest),
        b"SETNX" => match rest {
            [key, value] => {
                let applied = keyspace(state, session).set(
                    key,
                    value,
                    None,
                    KvWriteCondition::NotExists,
                    false,
                );
                reply(Frame::Integer(i64::from(applied)))
            }
            _ => wrong_arity("setnx"),
        },
        b"SETEX" => set_with_expiry(state, session, rest, 1000, "setex"),
        b"PSETEX" => set_with_expiry(state, session, rest, 1, "psetex"),
        b"GET" => match rest {
            [key] => into_reply(keyspace(state, session).get(key).map(bulk_or_null)),
            _ => wrong_arity("get"),
        },
        b"GETSET" => match rest {
            [key, value] => {
                into_reply(keyspace(state, session).get_set(key, value).map(bulk_or_null))
            }
            _ => wrong_arity("getset"),
        },
        b"MGET" => {
            if rest.is_empty() {
                return wrong_arity("mget");
            }
            let keyspace = keyspace(state, session);
            reply(Frame::Array(
                rest.iter()
                    // A wrong-typed key is a null in MGET, not an error.
                    .map(|key| keyspace.get(key).ok().flatten().map_or(Frame::Null, Frame::Bulk))
                    .collect(),
            ))
        }
        b"MSET" => {
            if rest.is_empty() || rest.len() % 2 != 0 {
                return wrong_arity("mset");
            }
            let keyspace = keyspace(state, session);
            for pair in rest.chunks_exact(2) {
                keyspace.set(&pair[0], &pair[1], None, KvWriteCondition::Always, false);
            }
            ok()
        }
        b"APPEND" => match rest {
            [key, suffix] => {
                into_reply(keyspace(state, session).append(key, suffix).map(as_integer))
            }
            _ => wrong_arity("append"),
        },
        b"STRLEN" => match rest {
            [key] => into_reply(keyspace(state, session).strlen(key).map(as_integer)),
            _ => wrong_arity("strlen"),
        },
        b"INCR" => increment(state, session, rest, 1, "incr"),
        b"DECR" => increment(state, session, rest, -1, "decr"),
        b"INCRBY" => increment_by(state, session, rest, false, "incrby"),
        b"DECRBY" => increment_by(state, session, rest, true, "decrby"),

        // ---- hashes ---------------------------------------------------------
        b"HSET" | b"HMSET" => {
            if rest.len() < 3 || (rest.len() - 1) % 2 != 0 {
                return wrong_arity("hset");
            }
            let keyspace = keyspace(state, session);
            let mut added = 0;
            for pair in rest[1..].chunks_exact(2) {
                match keyspace.hash_set(&rest[0], &pair[0], &pair[1]) {
                    Ok(true) => added += 1,
                    Ok(false) => {}
                    Err(err) => return from_error(err),
                }
            }
            // HMSET predates HSET and replies +OK where HSET replies with a count.
            if command == b"HMSET" { ok() } else { reply(Frame::Integer(added)) }
        }
        b"HGET" => match rest {
            [key, field] => {
                into_reply(keyspace(state, session).hash_get(key, field).map(bulk_or_null))
            }
            _ => wrong_arity("hget"),
        },
        b"HMGET" => {
            if rest.len() < 2 {
                return wrong_arity("hmget");
            }
            let keyspace = keyspace(state, session);
            let mut items = Vec::with_capacity(rest.len() - 1);
            for field in &rest[1..] {
                match keyspace.hash_get(&rest[0], field) {
                    Ok(value) => items.push(bulk_or_null(value)),
                    Err(err) => return from_error(err),
                }
            }
            reply(Frame::Array(items))
        }
        b"HDEL" => {
            if rest.len() < 2 {
                return wrong_arity("hdel");
            }
            let keyspace = keyspace(state, session);
            let mut removed = 0;
            for field in &rest[1..] {
                match keyspace.hash_remove(&rest[0], field) {
                    Ok(true) => removed += 1,
                    Ok(false) => {}
                    Err(err) => return from_error(err),
                }
            }
            reply(Frame::Integer(removed))
        }
        b"HEXISTS" => match rest {
            [key, field] => into_reply(
                keyspace(state, session)
                    .hash_contains(key, field)
                    .map(|found| Frame::Integer(i64::from(found))),
            ),
            _ => wrong_arity("hexists"),
        },
        b"HLEN" => match rest {
            [key] => into_reply(keyspace(state, session).hash_len(key).map(as_integer)),
            _ => wrong_arity("hlen"),
        },
        b"HGETALL" => match rest {
            [key] => into_reply(keyspace(state, session).hash_entries(key).map(|entries| {
                // A Map frame encodes as a RESP3 map or a flat RESP2 array, which is
                // exactly the difference between the two protocols here.
                Frame::Map(
                    entries
                        .into_iter()
                        .map(|(field, value)| (Frame::Bulk(field), Frame::Bulk(value)))
                        .collect(),
                )
            })),
            _ => wrong_arity("hgetall"),
        },
        b"HKEYS" | b"HVALS" => match rest {
            [key] => {
                let want_keys = command == b"HKEYS";
                into_reply(keyspace(state, session).hash_entries(key).map(|entries| {
                    Frame::Array(
                        entries
                            .into_iter()
                            .map(|(field, value)| {
                                Frame::Bulk(if want_keys { field } else { value })
                            })
                            .collect(),
                    )
                }))
            }
            _ => wrong_arity("hkeys"),
        },
        b"HINCRBY" => match rest {
            [key, field, delta] => match parse_int(delta) {
                Some(delta) => into_reply(
                    keyspace(state, session).hash_incr_by(key, field, delta).map(Frame::Integer),
                ),
                None => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("hincrby"),
        },

        // ---- lists ----------------------------------------------------------
        b"LPUSH" | b"RPUSH" => {
            if rest.len() < 2 {
                return wrong_arity("lpush");
            }
            let values: Vec<&[u8]> = rest[1..].iter().map(Vec::as_slice).collect();
            let keyspace = keyspace(state, session);
            let pushed = if command == b"LPUSH" {
                keyspace.push_front(&rest[0], &values)
            } else {
                keyspace.push_back(&rest[0], &values)
            };
            into_reply(pushed.map(as_integer))
        }
        b"LPOP" | b"RPOP" => match rest {
            [key] => {
                let keyspace = keyspace(state, session);
                let popped = if command == b"LPOP" {
                    keyspace.pop_front(key)
                } else {
                    keyspace.pop_back(key)
                };
                into_reply(popped.map(bulk_or_null))
            }
            _ => wrong_arity("lpop"),
        },
        b"LLEN" => match rest {
            [key] => into_reply(keyspace(state, session).list_len(key).map(as_integer)),
            _ => wrong_arity("llen"),
        },
        b"LRANGE" => match rest {
            [key, start, stop] => match (parse_int(start), parse_int(stop)) {
                (Some(start), Some(stop)) => into_reply(
                    keyspace(state, session)
                        .list_range(key, start, stop)
                        .map(|items| Frame::Array(items.into_iter().map(Frame::Bulk).collect())),
                ),
                _ => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("lrange"),
        },
        b"LINDEX" => match rest {
            [key, index] => match parse_int(index) {
                Some(index) => {
                    into_reply(keyspace(state, session).list_index(key, index).map(bulk_or_null))
                }
                None => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("lindex"),
        },
        b"LSET" => match rest {
            [key, index, value] => match parse_int(index) {
                Some(index) => match keyspace(state, session).list_set(key, index, value) {
                    Ok(true) => ok(),
                    Ok(false) => error("ERR index out of range"),
                    Err(err) => from_error(err),
                },
                None => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("lset"),
        },
        b"LTRIM" => match rest {
            [key, start, stop] => match (parse_int(start), parse_int(stop)) {
                (Some(start), Some(stop)) => {
                    match keyspace(state, session).list_trim(key, start, stop) {
                        Ok(()) => ok(),
                        Err(err) => from_error(err),
                    }
                }
                _ => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("ltrim"),
        },
        b"LREM" => match rest {
            [key, count, value] => match parse_int(count) {
                Some(count) => into_reply(
                    keyspace(state, session).list_remove(key, count, value).map(as_integer),
                ),
                None => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("lrem"),
        },

        // ---- sets -----------------------------------------------------------
        b"SADD" | b"SREM" => {
            if rest.len() < 2 {
                return wrong_arity(if command == b"SADD" { "sadd" } else { "srem" });
            }
            let members = slices(&rest[1..]);
            let keyspace = keyspace(state, session);
            let changed = if command == b"SADD" {
                keyspace.set_add(&rest[0], &members)
            } else {
                keyspace.set_remove(&rest[0], &members)
            };
            into_reply(changed.map(as_integer))
        }
        b"SISMEMBER" => match rest {
            [key, member] => into_reply(
                keyspace(state, session)
                    .set_contains(key, member)
                    .map(|found| Frame::Integer(i64::from(found))),
            ),
            _ => wrong_arity("sismember"),
        },
        b"SMISMEMBER" => {
            if rest.len() < 2 {
                return wrong_arity("smismember");
            }
            let keyspace = keyspace(state, session);
            let mut found = Vec::with_capacity(rest.len() - 1);
            for member in &rest[1..] {
                match keyspace.set_contains(&rest[0], member) {
                    Ok(present) => found.push(Frame::Integer(i64::from(present))),
                    Err(err) => return from_error(err),
                }
            }
            reply(Frame::Array(found))
        }
        b"SCARD" => match rest {
            [key] => into_reply(keyspace(state, session).set_len(key).map(as_integer)),
            _ => wrong_arity("scard"),
        },
        b"SMEMBERS" => match rest {
            [key] => into_reply(keyspace(state, session).set_members(key).map(bulk_set)),
            _ => wrong_arity("smembers"),
        },
        b"SPOP" => match rest {
            [key] => into_reply(
                keyspace(state, session)
                    .set_pop(key, 1)
                    .map(|members| bulk_or_null(members.into_iter().next())),
            ),
            [key, count] => match parse_int(count) {
                // A negative count is an error for SPOP, unlike SRANDMEMBER.
                Some(count) if count >= 0 => into_reply(
                    keyspace(state, session)
                        .set_pop(key, count as usize)
                        .map(|members| Frame::Set(members.into_iter().map(Frame::Bulk).collect())),
                ),
                Some(_) => error("ERR value is out of range, must be positive"),
                None => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("spop"),
        },
        b"SRANDMEMBER" => match rest {
            [key] => into_reply(
                keyspace(state, session)
                    .set_random_members(key, 1)
                    .map(|members| bulk_or_null(members.into_iter().next())),
            ),
            [key, count] => match parse_int(count) {
                Some(count) => {
                    into_reply(keyspace(state, session).set_random_members(key, count).map(
                        |members| Frame::Array(members.into_iter().map(Frame::Bulk).collect()),
                    ))
                }
                None => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("srandmember"),
        },
        b"SMOVE" => match rest {
            [source, destination, member] => into_reply(
                keyspace(state, session)
                    .set_move(source, destination, member)
                    .map(|moved| Frame::Integer(i64::from(moved))),
            ),
            _ => wrong_arity("smove"),
        },
        b"SUNION" | b"SINTER" | b"SDIFF" => {
            if rest.is_empty() {
                return wrong_arity("sunion");
            }
            let keys = slices(rest);
            let keyspace = keyspace(state, session);
            let combined = match command.as_slice() {
                b"SUNION" => keyspace.set_union(&keys),
                b"SINTER" => keyspace.set_intersect(&keys),
                _ => keyspace.set_difference(&keys),
            };
            into_reply(combined.map(bulk_set))
        }
        b"SUNIONSTORE" | b"SINTERSTORE" | b"SDIFFSTORE" => {
            if rest.len() < 2 {
                return wrong_arity("sunionstore");
            }
            let keys = slices(&rest[1..]);
            let keyspace = keyspace(state, session);
            let stored = match command.as_slice() {
                b"SUNIONSTORE" => keyspace.set_union_store(&rest[0], &keys),
                b"SINTERSTORE" => keyspace.set_intersect_store(&rest[0], &keys),
                _ => keyspace.set_difference_store(&rest[0], &keys),
            };
            into_reply(stored.map(as_integer))
        }

        // ---- sorted sets ------------------------------------------------------
        b"ZADD" => sorted_set_add(state, session, rest),
        b"ZREM" => {
            if rest.len() < 2 {
                return wrong_arity("zrem");
            }
            let members = slices(&rest[1..]);
            into_reply(
                keyspace(state, session).sorted_set_remove(&rest[0], &members).map(as_integer),
            )
        }
        b"ZSCORE" => match rest {
            [key, member] => into_reply(
                keyspace(state, session)
                    .sorted_set_score(key, member)
                    .map(|score| score.map_or(Frame::Null, score_frame)),
            ),
            _ => wrong_arity("zscore"),
        },
        b"ZMSCORE" => {
            if rest.len() < 2 {
                return wrong_arity("zmscore");
            }
            let keyspace = keyspace(state, session);
            let mut scores = Vec::with_capacity(rest.len() - 1);
            for member in &rest[1..] {
                match keyspace.sorted_set_score(&rest[0], member) {
                    Ok(score) => scores.push(score.map_or(Frame::Null, score_frame)),
                    Err(err) => return from_error(err),
                }
            }
            reply(Frame::Array(scores))
        }
        b"ZCARD" => match rest {
            [key] => into_reply(keyspace(state, session).sorted_set_len(key).map(as_integer)),
            _ => wrong_arity("zcard"),
        },
        b"ZRANK" | b"ZREVRANK" => match rest {
            [key, member] => into_reply(
                keyspace(state, session)
                    .sorted_set_rank(key, member, command == b"ZREVRANK")
                    .map(|rank| rank.map_or(Frame::Null, |rank| Frame::Integer(rank as i64))),
            ),
            _ => wrong_arity("zrank"),
        },
        b"ZINCRBY" => match rest {
            [key, delta, member] => match parse_score(delta) {
                Some(delta) => into_reply(
                    keyspace(state, session)
                        .sorted_set_incr_by(key, member, delta)
                        .map(score_frame),
                ),
                None => error("ERR value is not a valid float"),
            },
            _ => wrong_arity("zincrby"),
        },
        b"ZCOUNT" => match rest {
            [key, min, max] => match (parse_bound(min), parse_bound(max)) {
                (Some(min), Some(max)) => into_reply(
                    keyspace(state, session).sorted_set_count(key, min, max).map(as_integer),
                ),
                _ => error("ERR min or max is not a float"),
            },
            _ => wrong_arity("zcount"),
        },
        b"ZRANGE" | b"ZREVRANGE" => sorted_set_range(state, session, rest, command == b"ZREVRANGE"),
        b"ZRANGEBYSCORE" | b"ZREVRANGEBYSCORE" => {
            sorted_set_range_by_score(state, session, rest, command == b"ZREVRANGEBYSCORE")
        }
        b"ZPOPMIN" | b"ZPOPMAX" => {
            let from_max = command == b"ZPOPMAX";
            let (key, count) = match rest {
                [key] => (key, 1),
                [key, count] => match parse_int(count) {
                    Some(count) if count >= 0 => (key, count as usize),
                    Some(_) => return error("ERR value is out of range, must be positive"),
                    None => return error("ERR value is not an integer or out of range"),
                },
                _ => return wrong_arity("zpopmin"),
            };
            into_reply(
                keyspace(state, session).sorted_set_pop(key, count, from_max).map(flat_scored),
            )
        }
        b"ZREMRANGEBYRANK" => match rest {
            [key, start, stop] => match (parse_int(start), parse_int(stop)) {
                (Some(start), Some(stop)) => into_reply(
                    keyspace(state, session)
                        .sorted_set_remove_range_by_rank(key, start, stop)
                        .map(as_integer),
                ),
                _ => error("ERR value is not an integer or out of range"),
            },
            _ => wrong_arity("zremrangebyrank"),
        },
        b"ZREMRANGEBYSCORE" => match rest {
            [key, min, max] => match (parse_bound(min), parse_bound(max)) {
                (Some(min), Some(max)) => into_reply(
                    keyspace(state, session)
                        .sorted_set_remove_range_by_score(key, min, max)
                        .map(as_integer),
                ),
                _ => error("ERR min or max is not a float"),
            },
            _ => wrong_arity("zremrangebyscore"),
        },

        // ---- pub/sub ---------------------------------------------------------
        b"SUBSCRIBE" | b"PSUBSCRIBE" => subscribe(session, state, rest, command == b"PSUBSCRIBE"),
        b"UNSUBSCRIBE" | b"PUNSUBSCRIBE" => {
            unsubscribe(session, state, rest, command == b"PUNSUBSCRIBE")
        }
        b"PUBLISH" => match rest {
            [channel, payload] => {
                reply(Frame::Integer(state.pubsub.publish(channel, payload) as i64))
            }
            _ => wrong_arity("publish"),
        },
        b"PUBSUB" => pubsub(state, rest),

        _ => error(format!(
            "ERR unknown command '{}', with args beginning with: ",
            String::from_utf8_lossy(name)
        )),
    }
}

// ---- families -------------------------------------------------------------

fn hello(session: &mut Session, rest: &[Vec<u8>]) -> Action {
    if let Some(version) = rest.first() {
        match parse_int(version)
            .and_then(|v| u8::try_from(v).ok())
            .and_then(RespProtocol::from_version)
        {
            Some(protocol) => session.protocol = protocol,
            None => {
                return error("NOPROTO unsupported protocol version");
            }
        }
    }

    // AUTH and SETNAME options. There is no authentication yet, so AUTH is refused
    // rather than silently accepted — pretending to check a password is worse than
    // saying there is none.
    let mut index = 1;
    while index < rest.len() {
        match rest[index].to_ascii_uppercase().as_slice() {
            b"AUTH" if index + 2 < rest.len() => {
                return error("ERR Client sent AUTH, but no password is set");
            }
            b"SETNAME" if index + 1 < rest.len() => {
                session.name = rest[index + 1].clone();
                index += 2;
            }
            _ => return error("ERR syntax error in HELLO"),
        }
    }

    reply(Frame::Map(vec![
        (Frame::bulk("server"), Frame::bulk("kvlite")),
        (Frame::bulk("version"), Frame::bulk(REDIS_COMPAT_VERSION)),
        (Frame::bulk("kvlite_version"), Frame::bulk(VERSION)),
        (Frame::bulk("proto"), Frame::Integer(i64::from(session.protocol.version()))),
        (Frame::bulk("id"), Frame::Integer(session.id as i64)),
        (Frame::bulk("mode"), Frame::bulk("standalone")),
        (Frame::bulk("role"), Frame::bulk("master")),
        (Frame::bulk("modules"), Frame::Array(Vec::new())),
    ]))
}

fn client(session: &mut Session, rest: &[Vec<u8>]) -> Action {
    match rest.first().map(|s| s.to_ascii_uppercase()).as_deref() {
        Some(b"SETNAME") => match rest.get(1) {
            Some(name) if !name.iter().any(|b| b.is_ascii_whitespace()) => {
                session.name = name.clone();
                ok()
            }
            Some(_) => {
                error("ERR Client names cannot contain spaces, newlines or special characters.")
            }
            None => wrong_arity("client|setname"),
        },
        Some(b"GETNAME") => reply(if session.name.is_empty() {
            Frame::Null
        } else {
            Frame::Bulk(session.name.clone())
        }),
        Some(b"ID") => reply(Frame::Integer(session.id as i64)),
        Some(b"SETINFO") => {
            // Clients announce their library name and version here. Record it for
            // INFO and move on; there is nothing to validate.
            if let Some(value) = rest.get(2) {
                session.library = value.clone();
            }
            ok()
        }
        Some(b"INFO") => reply(Frame::Bulk(
            format!(
                "id={} addr={} name={} lib={} db={}",
                session.id,
                session.peer,
                String::from_utf8_lossy(&session.name),
                String::from_utf8_lossy(&session.library),
                session.keyspace
            )
            .into_bytes(),
        )),
        // NO-EVICT, NO-TOUCH, REPLY ON and friends: accept and ignore. A client that
        // asks for a behaviour we do not have is better served by success than by an
        // error it will treat as a broken server.
        Some(_) => ok(),
        None => wrong_arity("client"),
    }
}

fn config(state: &ServerState, rest: &[Vec<u8>]) -> Action {
    match rest.first().map(|s| s.to_ascii_uppercase()).as_deref() {
        Some(b"GET") => {
            let databases = state.store.keyspace_count().to_string();
            let known: [(&str, &str); 6] = [
                ("maxmemory", "0"),
                ("maxmemory-policy", "noeviction"),
                ("timeout", "0"),
                ("save", ""),
                ("appendonly", "no"),
                ("databases", databases.as_str()),
            ];
            let mut pairs = Vec::new();
            for pattern in &rest[1..] {
                for (key, value) in &known {
                    if glob::matches(pattern, key.as_bytes()) {
                        pairs.push((Frame::bulk(*key), Frame::bulk(*value)));
                    }
                }
            }
            reply(Frame::Map(pairs))
        }
        // Accepting a SET we do not honour is a lie, but refusing it breaks clients
        // that set a parameter defensively at connect time. Redis-compatible servers
        // universally accept it.
        Some(_) => ok(),
        None => wrong_arity("config"),
    }
}

fn info(state: &ServerState) -> String {
    let uptime = state.started.elapsed().as_secs();
    let mut body = format!(
        "# Server\r\n\
         redis_version:{REDIS_COMPAT_VERSION}\r\n\
         kvlite_version:{VERSION}\r\n\
         redis_mode:standalone\r\n\
         os:{}\r\n\
         arch_bits:{}\r\n\
         process_id:{}\r\n\
         uptime_in_seconds:{uptime}\r\n\
         \r\n# Clients\r\nconnected_clients:1\r\n\
         \r\n# Replication\r\nrole:master\r\nconnected_slaves:0\r\n\
         \r\n# Keyspace\r\n",
        std::env::consts::OS,
        usize::BITS,
        std::process::id(),
    );

    for index in 0..state.store.keyspace_count() {
        let Some(keyspace) = state.store.keyspace(index) else {
            continue;
        };
        let keys = keyspace.len();
        if keys > 0 {
            body.push_str(&format!("db{index}:keys={keys},expires=0,avg_ttl=0\r\n"));
        }
    }
    body
}

fn scan(state: &ServerState, session: &Session, rest: &[Vec<u8>]) -> Action {
    let Some(cursor) = rest.first().and_then(|c| parse_int(c)) else {
        return error("ERR invalid cursor");
    };

    let mut pattern: Option<Vec<u8>> = None;
    let mut index = 1;
    while index < rest.len() {
        match rest[index].to_ascii_uppercase().as_slice() {
            b"MATCH" if index + 1 < rest.len() => {
                pattern = Some(rest[index + 1].clone());
                index += 2;
            }
            // COUNT is a hint even in Redis, and we return everything in one pass.
            b"COUNT" if index + 1 < rest.len() => index += 2,
            b"TYPE" if index + 1 < rest.len() => index += 2,
            _ => return error("ERR syntax error"),
        }
    }

    // ponytail: one pass, cursor always returns to 0. That satisfies the SCAN
    // contract — every key present throughout the iteration is returned exactly
    // once — but it does not bound the reply. Add real cursor stepping when a
    // keyspace large enough to care about exists.
    if cursor != 0 {
        return reply(Frame::Array(vec![Frame::bulk("0"), Frame::Array(Vec::new())]));
    }

    let keys = keyspace(state, session)
        .keys()
        .into_iter()
        .filter(|key| pattern.as_ref().is_none_or(|p| glob::matches(p, key)))
        .map(Frame::Bulk)
        .collect();

    reply(Frame::Array(vec![Frame::bulk("0"), Frame::Array(keys)]))
}

fn set(state: &ServerState, session: &Session, rest: &[Vec<u8>]) -> Action {
    let ([key, value], options) = (
        match rest.get(..2) {
            Some([key, value]) => [key, value],
            _ => return wrong_arity("set"),
        },
        &rest[2..],
    );

    let mut time_to_live = None;
    let mut condition = KvWriteCondition::Always;
    let mut keep_time_to_live = false;
    let mut return_previous = false;

    let mut index = 0;
    while index < options.len() {
        let option = options[index].to_ascii_uppercase();
        match option.as_slice() {
            b"NX" => condition = KvWriteCondition::NotExists,
            b"XX" => condition = KvWriteCondition::Exists,
            b"KEEPTTL" => keep_time_to_live = true,
            b"GET" => return_previous = true,
            b"EX" | b"PX" => {
                let Some(amount) = options.get(index + 1).and_then(|a| parse_int(a)) else {
                    return error("ERR value is not an integer or out of range");
                };
                if amount <= 0 {
                    return error("ERR invalid expire time in 'set' command");
                }
                let multiplier = if option == b"EX" { 1000 } else { 1 };
                time_to_live =
                    Some(Duration::from_millis((amount as u64).saturating_mul(multiplier)));
                index += 1;
            }
            _ => return error("ERR syntax error"),
        }
        index += 1;
    }

    if keep_time_to_live && time_to_live.is_some() {
        return error("ERR syntax error");
    }

    let keyspace = keyspace(state, session);

    let previous = if return_previous {
        match keyspace.get(key) {
            Ok(value) => Some(value),
            Err(err) => return from_error(err),
        }
    } else {
        None
    };

    let applied = keyspace.set(key, value, time_to_live, condition, keep_time_to_live);

    match (return_previous, previous) {
        (true, Some(value)) => reply(bulk_or_null(value)),
        // A skipped NX/XX write replies with a null, not +OK.
        _ if !applied => reply(Frame::Null),
        _ => ok(),
    }
}

fn set_with_expiry(
    state: &ServerState,
    session: &Session,
    rest: &[Vec<u8>],
    multiplier: u64,
    name: &str,
) -> Action {
    let [key, amount, value] = match rest.get(..3) {
        Some([key, amount, value]) => [key, amount, value],
        _ => return wrong_arity(name),
    };
    let Some(amount) = parse_int(amount) else {
        return error("ERR value is not an integer or out of range");
    };
    if amount <= 0 {
        return error(format!("ERR invalid expire time in '{name}' command"));
    }

    let time_to_live = Duration::from_millis((amount as u64).saturating_mul(multiplier));
    keyspace(state, session).set(key, value, Some(time_to_live), KvWriteCondition::Always, false);
    ok()
}

fn expire(
    state: &ServerState,
    session: &Session,
    rest: &[Vec<u8>],
    multiplier: u64,
    name: &str,
) -> Action {
    let [key, amount] = match rest.get(..2) {
        Some([key, amount]) => [key, amount],
        _ => return wrong_arity(name),
    };
    let Some(amount) = parse_int(amount) else {
        return error("ERR value is not an integer or out of range");
    };

    let keyspace = keyspace(state, session);
    if amount <= 0 {
        // A non-positive expiry deletes the key immediately, as Redis does.
        return reply(Frame::Integer(i64::from(keyspace.remove(key))));
    }

    let time_to_live = Duration::from_millis((amount as u64).saturating_mul(multiplier));
    reply(Frame::Integer(i64::from(keyspace.expire(key, Some(time_to_live)))))
}

fn time_to_live(
    state: &ServerState,
    session: &Session,
    rest: &[Vec<u8>],
    divisor: u64,
    name: &str,
) -> Action {
    let [key] = match rest.get(..1) {
        Some([key]) => [key],
        _ => return wrong_arity(name),
    };

    let keyspace = keyspace(state, session);
    if !keyspace.contains(key) {
        return reply(Frame::Integer(-2));
    }
    match keyspace.time_to_live(key) {
        None => reply(Frame::Integer(-1)),
        Some(remaining) => {
            let millis = remaining.as_millis() as u64;
            // Redis rounds seconds to nearest rather than truncating.
            let value = if divisor == 1 { millis } else { (millis + 500) / 1000 };
            reply(Frame::Integer(value as i64))
        }
    }
}

fn increment(
    state: &ServerState,
    session: &Session,
    rest: &[Vec<u8>],
    delta: i64,
    name: &str,
) -> Action {
    match rest {
        [key] => into_reply(keyspace(state, session).incr_by(key, delta).map(Frame::Integer)),
        _ => wrong_arity(name),
    }
}

fn increment_by(
    state: &ServerState,
    session: &Session,
    rest: &[Vec<u8>],
    negate: bool,
    name: &str,
) -> Action {
    let [key, amount] = match rest.get(..2) {
        Some([key, amount]) => [key, amount],
        _ => return wrong_arity(name),
    };
    let Some(delta) = parse_int(amount) else {
        return error("ERR value is not an integer or out of range");
    };
    // DECRBY i64::MIN cannot be negated, and Redis reports exactly this.
    let Some(delta) = (if negate { delta.checked_neg() } else { Some(delta) }) else {
        return error("ERR decrement would overflow");
    };
    into_reply(keyspace(state, session).incr_by(key, delta).map(Frame::Integer))
}

/// `ZADD key [NX|XX] [GT|LT] [CH] [INCR] score member [score member ...]`
fn sorted_set_add(state: &ServerState, session: &Session, rest: &[Vec<u8>]) -> Action {
    let Some(key) = rest.first() else {
        return wrong_arity("zadd");
    };

    let mut condition = KvWriteCondition::Always;
    let (mut greater, mut less, mut report_changed, mut increment) = (false, false, false, false);

    let mut index = 1;
    while index < rest.len() {
        match rest[index].to_ascii_uppercase().as_slice() {
            b"NX" => condition = KvWriteCondition::NotExists,
            b"XX" => condition = KvWriteCondition::Exists,
            b"GT" => greater = true,
            b"LT" => less = true,
            b"CH" => report_changed = true,
            b"INCR" => increment = true,
            // The first thing that is not a flag begins the score/member pairs.
            _ => break,
        }
        index += 1;
    }

    if greater && less {
        return error("ERR GT, LT, and/or NX options at the same time are not compatible");
    }
    if (greater || less) && condition == KvWriteCondition::NotExists {
        return error("ERR GT, LT, and/or NX options at the same time are not compatible");
    }

    let pairs = &rest[index..];
    if pairs.is_empty() || pairs.len() % 2 != 0 {
        return error("ERR syntax error");
    }

    let mut entries = Vec::with_capacity(pairs.len() / 2);
    for pair in pairs.chunks_exact(2) {
        let Some(score) = parse_score(&pair[0]) else {
            return error("ERR value is not a valid float");
        };
        entries.push((score, pair[1].as_slice()));
    }

    let keyspace = keyspace(state, session);

    if increment {
        // INCR takes exactly one pair and replies with the new score, which makes it
        // ZINCRBY wearing a different hat — including honouring NX, XX, GT and LT.
        if entries.len() != 1 {
            return error("ERR INCR option supports a single increment-element pair");
        }
        let (delta, member) = entries[0];
        let existing = match keyspace.sorted_set_score(key, member) {
            Ok(existing) => existing,
            Err(err) => return from_error(err),
        };
        let declined = match existing {
            Some(current) => {
                condition == KvWriteCondition::NotExists
                    || (greater && delta < 0.0)
                    || (less && delta > 0.0)
                    || (greater && current + delta <= current)
                    || (less && current + delta >= current)
            }
            None => condition == KvWriteCondition::Exists,
        };
        if declined {
            return reply(Frame::Null);
        }
        return into_reply(keyspace.sorted_set_incr_by(key, member, delta).map(score_frame));
    }

    match keyspace.sorted_set_add(key, &entries, condition, greater, less) {
        Ok((added, changed)) => {
            reply(Frame::Integer(if report_changed { changed as i64 } else { added as i64 }))
        }
        Err(err) => from_error(err),
    }
}

/// `ZRANGE key start stop [WITHSCORES]`, and the `REV` spelling of it.
fn sorted_set_range(
    state: &ServerState,
    session: &Session,
    rest: &[Vec<u8>],
    reverse: bool,
) -> Action {
    let [key, start, stop] = match rest.get(..3) {
        Some([key, start, stop]) => [key, start, stop],
        _ => return wrong_arity("zrange"),
    };
    let (Some(start), Some(stop)) = (parse_int(start), parse_int(stop)) else {
        return error("ERR value is not an integer or out of range");
    };
    let Some(with_scores) = parse_with_scores(&rest[3..]) else {
        return error("ERR syntax error");
    };

    into_reply(
        keyspace(state, session)
            .sorted_set_range(key, start, stop, reverse)
            .map(|entries| scored_reply(entries, with_scores)),
    )
}

/// `ZRANGEBYSCORE key min max [WITHSCORES] [LIMIT offset count]`.
fn sorted_set_range_by_score(
    state: &ServerState,
    session: &Session,
    rest: &[Vec<u8>],
    reverse: bool,
) -> Action {
    let [key, first, second] = match rest.get(..3) {
        Some([key, first, second]) => [key, first, second],
        _ => return wrong_arity("zrangebyscore"),
    };

    // ZREVRANGEBYSCORE takes its bounds high-then-low, the opposite way round.
    let (raw_min, raw_max) = if reverse { (second, first) } else { (first, second) };
    let (Some(min), Some(max)) = (parse_bound(raw_min), parse_bound(raw_max)) else {
        return error("ERR min or max is not a float");
    };

    let mut with_scores = false;
    let mut limit: Option<(i64, i64)> = None;
    let mut index = 3;
    while index < rest.len() {
        match rest[index].to_ascii_uppercase().as_slice() {
            b"WITHSCORES" => index += 1,
            b"LIMIT" if index + 2 < rest.len() => {
                let (Some(offset), Some(count)) =
                    (parse_int(&rest[index + 1]), parse_int(&rest[index + 2]))
                else {
                    return error("ERR value is not an integer or out of range");
                };
                limit = Some((offset, count));
                index += 3;
                continue;
            }
            _ => return error("ERR syntax error"),
        }
        with_scores = true;
    }

    let found = match keyspace(state, session).sorted_set_range_by_score(key, min, max, reverse) {
        Ok(found) => found,
        Err(err) => return from_error(err),
    };

    let limited = match limit {
        None => found,
        // A negative offset yields nothing; a negative count means "to the end".
        Some((offset, _)) if offset < 0 => Vec::new(),
        Some((offset, count)) => {
            let skipped = found.into_iter().skip(offset as usize);
            if count < 0 { skipped.collect() } else { skipped.take(count as usize).collect() }
        }
    };

    reply(scored_reply(limited, with_scores))
}

/// Parses a trailing `WITHSCORES`, or nothing. `None` means something else was there.
fn parse_with_scores(tail: &[Vec<u8>]) -> Option<bool> {
    match tail {
        [] => Some(false),
        [flag] if flag.eq_ignore_ascii_case(b"WITHSCORES") => Some(true),
        _ => None,
    }
}

/// Parses a `ZRANGEBYSCORE` bound: `1.5`, `(1.5` for exclusive, or `+inf` / `-inf`.
fn parse_bound(raw: &[u8]) -> Option<Bound<f64>> {
    match raw.first() {
        Some(b'(') => parse_score(&raw[1..]).map(Bound::Excluded),
        _ => parse_score(raw).map(Bound::Included),
    }
}

fn score_frame(score: f64) -> Frame {
    Frame::Bulk(format_score(score))
}

/// `(member, score)` pairs as the flat array Redis sends for `WITHSCORES`.
fn flat_scored(entries: Vec<(Vec<u8>, f64)>) -> Frame {
    let mut items = Vec::with_capacity(entries.len() * 2);
    for (member, score) in entries {
        items.push(Frame::Bulk(member));
        items.push(score_frame(score));
    }
    Frame::Array(items)
}

fn scored_reply(entries: Vec<(Vec<u8>, f64)>, with_scores: bool) -> Frame {
    if with_scores {
        flat_scored(entries)
    } else {
        Frame::Array(entries.into_iter().map(|(member, _)| Frame::Bulk(member)).collect())
    }
}

/// A set reply. RESP3 has a set type; RESP2 flattens it to an array.
fn bulk_set(members: Vec<Vec<u8>>) -> Frame {
    Frame::Set(members.into_iter().map(Frame::Bulk).collect())
}

/// Borrows a slice of owned arguments as a slice of byte slices.
fn slices(args: &[Vec<u8>]) -> Vec<&[u8]> {
    args.iter().map(Vec::as_slice).collect()
}

fn subscribe(
    session: &mut Session,
    state: &ServerState,
    rest: &[Vec<u8>],
    by_pattern: bool,
) -> Action {
    if rest.is_empty() {
        return wrong_arity(if by_pattern { "psubscribe" } else { "subscribe" });
    }

    let mut frames = Vec::with_capacity(rest.len());
    for target in rest {
        if by_pattern {
            state.pubsub.subscribe_pattern(session.id, target, session.outbox.clone());
            session.patterns.insert(target.clone());
        } else {
            state.pubsub.subscribe(session.id, target, session.outbox.clone());
            session.channels.insert(target.clone());
        }
        frames.push(confirmation(
            if by_pattern { "psubscribe" } else { "subscribe" },
            target,
            session.subscription_count(),
        ));
    }
    Action::Reply(frames)
}

fn unsubscribe(
    session: &mut Session,
    state: &ServerState,
    rest: &[Vec<u8>],
    by_pattern: bool,
) -> Action {
    let kind = if by_pattern { "punsubscribe" } else { "unsubscribe" };

    let targets: Vec<Vec<u8>> = if rest.is_empty() {
        // No arguments means every subscription of that kind.
        if by_pattern {
            session.patterns.iter().cloned().collect()
        } else {
            session.channels.iter().cloned().collect()
        }
    } else {
        rest.to_vec()
    };

    if targets.is_empty() {
        // Redis still acknowledges, with a null channel and a count of zero.
        return Action::Reply(vec![Frame::Push(vec![
            Frame::bulk(kind),
            Frame::Null,
            Frame::Integer(session.subscription_count() as i64),
        ])]);
    }

    let mut frames = Vec::with_capacity(targets.len());
    for target in targets {
        if by_pattern {
            state.pubsub.unsubscribe_pattern(session.id, &target);
            session.patterns.remove(&target);
        } else {
            state.pubsub.unsubscribe(session.id, &target);
            session.channels.remove(&target);
        }
        frames.push(confirmation(kind, &target, session.subscription_count()));
    }
    Action::Reply(frames)
}

fn pubsub(state: &ServerState, rest: &[Vec<u8>]) -> Action {
    match rest.first().map(|s| s.to_ascii_uppercase()).as_deref() {
        Some(b"CHANNELS") => reply(Frame::Array(
            state
                .pubsub
                .active_channels(rest.get(1).map(Vec::as_slice))
                .into_iter()
                .map(Frame::Bulk)
                .collect(),
        )),
        Some(b"NUMSUB") => {
            let mut pairs = Vec::new();
            for channel in &rest[1..] {
                let count = state.pubsub.active_channels(Some(channel)).len();
                pairs.push(Frame::Bulk(channel.clone()));
                pairs.push(Frame::Integer(count as i64));
            }
            reply(Frame::Array(pairs))
        }
        Some(b"NUMPAT") => reply(Frame::Integer(0)),
        _ => wrong_arity("pubsub"),
    }
}

// ---- helpers --------------------------------------------------------------

fn keyspace<'a>(state: &'a ServerState, session: &Session) -> &'a KvKeyspace {
    state
        .store
        .keyspace(session.keyspace)
        // SELECT validated the index against this same store, and keyspace count is
        // fixed at construction, so this cannot fail.
        .expect("session keyspace was validated by SELECT")
}

fn allowed_while_subscribed(command: &[u8]) -> bool {
    matches!(
        command,
        b"SUBSCRIBE"
            | b"UNSUBSCRIBE"
            | b"PSUBSCRIBE"
            | b"PUNSUBSCRIBE"
            | b"SSUBSCRIBE"
            | b"SUNSUBSCRIBE"
            | b"PING"
            | b"QUIT"
            | b"RESET"
            | b"HELLO"
    )
}

fn confirmation(kind: &str, target: &[u8], count: usize) -> Frame {
    Frame::Push(vec![Frame::bulk(kind), Frame::bulk(target), Frame::Integer(count as i64)])
}

fn reply(frame: Frame) -> Action {
    Action::Reply(vec![frame])
}

fn ok() -> Action {
    reply(Frame::ok())
}

fn error(message: impl AsRef<str>) -> Action {
    reply(Frame::error(message.as_ref()))
}

fn wrong_arity(name: &str) -> Action {
    error(format!("ERR wrong number of arguments for '{name}' command"))
}

fn from_error(err: KvError) -> Action {
    reply(Frame::error(err.to_string()))
}

fn into_reply(result: Result<Frame, KvError>) -> Action {
    match result {
        Ok(frame) => reply(frame),
        Err(err) => from_error(err),
    }
}

fn bulk_or_null(value: Option<Vec<u8>>) -> Frame {
    value.map_or(Frame::Null, Frame::Bulk)
}

fn as_integer(value: u64) -> Frame {
    Frame::Integer(value as i64)
}

#[cfg(test)]
mod tests {
    use kvlite_api::KvValueType;

    /// `KvValueType::wire_name` is what `TYPE` replies with, so a rename there is a
    /// wire-visible change.
    #[test]
    fn type_names_match_the_redis_wire_names() {
        assert_eq!(KvValueType::None.wire_name(), "none");
        assert_eq!(KvValueType::String.wire_name(), "string");
        assert_eq!(KvValueType::List.wire_name(), "list");
        assert_eq!(KvValueType::Hash.wire_name(), "hash");
    }
}

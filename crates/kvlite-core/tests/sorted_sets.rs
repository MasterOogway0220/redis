//! Sorted-set behaviour, written against the public API only.
//!
//! Ordering is the whole point of the type, so most of this is about what comes
//! back and in what order — including the boundary cases that a naive score index
//! gets wrong: tied scores, exclusive bounds, infinities, and negative zero.

use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kvlite_core::{KvClock, KvError, KvOptions, KvStore, KvValueType, KvWriteCondition};

#[derive(Debug, Default)]
struct ManualClock(AtomicU64);

impl ManualClock {
    fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }
}

impl KvClock for ManualClock {
    fn now_millis(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Members only, as text, so a failing assertion reads as names rather than bytes.
fn names(entries: &[(Vec<u8>, f64)]) -> Vec<String> {
    entries.iter().map(|(member, _)| String::from_utf8_lossy(member).into_owned()).collect()
}

/// Four members, two of which share a score, so tie-breaking is always exercised.
fn seeded() -> KvStore {
    let store = KvStore::new();
    store
        .default_keyspace()
        .sorted_set_add(
            b"z",
            &[(1.0, b"a"), (2.0, b"b"), (2.0, b"c"), (3.0, b"d")],
            KvWriteCondition::Always,
            false,
            false,
        )
        .unwrap();
    store
}

#[test]
fn orders_by_score_then_by_member() {
    let store = seeded();
    let keyspace = store.default_keyspace();

    assert_eq!(keyspace.sorted_set_len(b"z").unwrap(), 4);
    assert_eq!(keyspace.kind(b"z"), KvValueType::SortedSet);
    assert_eq!(
        names(&keyspace.sorted_set_range(b"z", 0, -1, false).unwrap()),
        ["a", "b", "c", "d"],
        "b and c share a score, so they fall in member order"
    );
    assert_eq!(names(&keyspace.sorted_set_range(b"z", 0, -1, true).unwrap()), ["d", "c", "b", "a"]);

    assert_eq!(keyspace.sorted_set_score(b"z", b"b").unwrap(), Some(2.0));
    assert_eq!(keyspace.sorted_set_score(b"z", b"missing").unwrap(), None);
    assert_eq!(keyspace.sorted_set_score(b"absent", b"b").unwrap(), None);
}

#[test]
fn rank_ranges_follow_the_redis_clamping_rules() {
    let store = seeded();
    let keyspace = store.default_keyspace();
    let range = |start, stop| names(&keyspace.sorted_set_range(b"z", start, stop, false).unwrap());

    assert_eq!(range(1, 2), ["b", "c"]);
    assert_eq!(range(-2, -1), ["c", "d"]);
    assert_eq!(range(-100, 100), ["a", "b", "c", "d"], "out-of-range bounds clamp");
    assert!(range(3, 1).is_empty(), "an inverted range is empty");
    assert!(range(10, 20).is_empty());
    assert!(keyspace.sorted_set_range(b"absent", 0, -1, false).unwrap().is_empty());
}

#[test]
fn adding_reports_added_and_changed_separately() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    let add = |entries: &[(f64, &[u8])]| {
        keyspace.sorted_set_add(b"z", entries, KvWriteCondition::Always, false, false).unwrap()
    };

    assert_eq!(add(&[(1.0, b"a"), (2.0, b"b")]), (2, 2), "both are new");
    assert_eq!(add(&[(1.0, b"a")]), (0, 0), "the same score is neither added nor changed");
    assert_eq!(add(&[(9.0, b"a")]), (0, 1), "a re-score is changed but not added");
    assert_eq!(add(&[(5.0, b"c"), (9.0, b"a")]), (1, 1));
}

#[test]
fn add_conditions_map_to_nx_xx_gt_and_lt() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    let add = |entries: &[(f64, &[u8])], condition, greater, less| {
        keyspace.sorted_set_add(b"z", entries, condition, greater, less).unwrap()
    };
    let score = || keyspace.sorted_set_score(b"z", b"a").unwrap();

    // XX cannot create.
    assert_eq!(add(&[(1.0, b"a")], KvWriteCondition::Exists, false, false), (0, 0));
    assert!(!keyspace.contains(b"z"), "a declined add must not leave an empty key behind");

    add(&[(5.0, b"a")], KvWriteCondition::Always, false, false);

    // NX cannot update.
    assert_eq!(add(&[(1.0, b"a")], KvWriteCondition::NotExists, false, false), (0, 0));
    assert_eq!(score(), Some(5.0));

    // GT only raises.
    add(&[(3.0, b"a")], KvWriteCondition::Always, true, false);
    assert_eq!(score(), Some(5.0), "GT refused a decrease");
    add(&[(7.0, b"a")], KvWriteCondition::Always, true, false);
    assert_eq!(score(), Some(7.0));

    // LT only lowers.
    add(&[(9.0, b"a")], KvWriteCondition::Always, false, true);
    assert_eq!(score(), Some(7.0), "LT refused an increase");
    add(&[(2.0, b"a")], KvWriteCondition::Always, false, true);
    assert_eq!(score(), Some(2.0));
}

#[test]
fn ranks_count_from_either_end() {
    let store = seeded();
    let keyspace = store.default_keyspace();

    assert_eq!(keyspace.sorted_set_rank(b"z", b"a", false).unwrap(), Some(0));
    assert_eq!(keyspace.sorted_set_rank(b"z", b"d", false).unwrap(), Some(3));
    assert_eq!(keyspace.sorted_set_rank(b"z", b"a", true).unwrap(), Some(3));
    assert_eq!(keyspace.sorted_set_rank(b"z", b"missing", false).unwrap(), None);
    assert_eq!(keyspace.sorted_set_rank(b"absent", b"a", false).unwrap(), None);
}

#[test]
fn score_ranges_handle_exclusive_and_infinite_bounds() {
    let store = seeded();
    let keyspace = store.default_keyspace();
    let range =
        |min, max| names(&keyspace.sorted_set_range_by_score(b"z", min, max, false).unwrap());

    assert_eq!(range(Bound::Unbounded, Bound::Unbounded), ["a", "b", "c", "d"]);
    assert_eq!(range(Bound::Included(f64::NEG_INFINITY), Bound::Included(f64::INFINITY)).len(), 4);
    assert_eq!(range(Bound::Included(2.0), Bound::Included(2.0)), ["b", "c"]);
    assert_eq!(
        range(Bound::Excluded(1.0), Bound::Excluded(3.0)),
        ["b", "c"],
        "exclusive bounds drop every member sitting exactly on them"
    );
    assert!(range(Bound::Excluded(3.0), Bound::Unbounded).is_empty());

    assert_eq!(keyspace.sorted_set_count(b"z", Bound::Included(2.0), Bound::Unbounded).unwrap(), 3);
    assert_eq!(
        keyspace.sorted_set_count(b"absent", Bound::Unbounded, Bound::Unbounded).unwrap(),
        0
    );

    // Reversing flips the result; the bounds are still given low to high.
    assert_eq!(
        names(
            &keyspace
                .sorted_set_range_by_score(b"z", Bound::Included(2.0), Bound::Unbounded, true)
                .unwrap()
        ),
        ["d", "c", "b"]
    );
}

#[test]
fn negative_zero_is_not_a_separate_score() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    keyspace.sorted_set_add(b"z", &[(-0.0, b"a")], KvWriteCondition::Always, false, false).unwrap();

    // Without normalisation this member would sort below every lookup for 0.0 and
    // be invisible to a range over it.
    assert_eq!(keyspace.sorted_set_rank(b"z", b"a", false).unwrap(), Some(0));
    assert_eq!(
        keyspace.sorted_set_count(b"z", Bound::Included(0.0), Bound::Included(0.0)).unwrap(),
        1
    );
    // And re-adding 0.0 over -0.0 is not a change, because they are the same score.
    assert_eq!(
        keyspace.sorted_set_add(b"z", &[(0.0, b"a")], KvWriteCondition::Always, false, false),
        Ok((0, 0))
    );
}

#[test]
fn incrementing_a_score_creates_the_member_if_needed() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    assert_eq!(keyspace.sorted_set_incr_by(b"z", b"a", 2.5).unwrap(), 2.5);
    assert_eq!(keyspace.sorted_set_incr_by(b"z", b"a", -1.0).unwrap(), 1.5);
    assert_eq!(keyspace.sorted_set_rank(b"z", b"a", false).unwrap(), Some(0));
}

#[test]
fn a_score_with_no_place_in_the_order_is_refused() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    assert_eq!(
        keyspace.sorted_set_add(b"z", &[(f64::NAN, b"a")], KvWriteCondition::Always, false, false),
        Err(KvError::NotAFloat)
    );
    assert!(!keyspace.contains(b"z"), "a rejected add must not create the key");
    assert_eq!(keyspace.sorted_set_incr_by(b"z", b"a", f64::NAN), Err(KvError::NotAFloat));

    // Adding opposite infinities is the one arithmetic path that reaches NaN.
    keyspace.sorted_set_incr_by(b"z", b"a", f64::INFINITY).unwrap();
    assert_eq!(keyspace.sorted_set_incr_by(b"z", b"a", f64::NEG_INFINITY), Err(KvError::NaNResult));
    assert_eq!(
        keyspace.sorted_set_score(b"z", b"a").unwrap(),
        Some(f64::INFINITY),
        "the member must keep the score it had"
    );
}

#[test]
fn popping_takes_from_the_requested_end() {
    let store = seeded();
    let keyspace = store.default_keyspace();

    assert_eq!(keyspace.sorted_set_pop(b"z", 1, false).unwrap(), vec![(b"a".to_vec(), 1.0)]);
    assert_eq!(keyspace.sorted_set_pop(b"z", 1, true).unwrap(), vec![(b"d".to_vec(), 3.0)]);
    assert_eq!(keyspace.sorted_set_len(b"z").unwrap(), 2);

    assert_eq!(keyspace.sorted_set_pop(b"z", 99, false).unwrap().len(), 2, "takes what there is");
    assert!(!keyspace.contains(b"z"), "an emptied sorted set takes its key with it");
}

#[test]
fn removing_by_member_by_rank_and_by_score() {
    let store = seeded();
    let keyspace = store.default_keyspace();

    assert_eq!(keyspace.sorted_set_remove(b"z", &[b"a", b"missing"]).unwrap(), 1);
    assert_eq!(keyspace.sorted_set_remove_range_by_rank(b"z", 0, 0).unwrap(), 1);
    assert_eq!(names(&keyspace.sorted_set_range(b"z", 0, -1, false).unwrap()), ["c", "d"]);

    assert_eq!(
        keyspace
            .sorted_set_remove_range_by_score(b"z", Bound::Included(3.0), Bound::Unbounded)
            .unwrap(),
        1
    );
    assert_eq!(names(&keyspace.sorted_set_range(b"z", 0, -1, false).unwrap()), ["c"]);

    keyspace.sorted_set_remove(b"z", &[b"c"]).unwrap();
    assert!(!keyspace.contains(b"z"));
}

#[test]
fn rescoring_never_leaves_a_ghost_behind() {
    // The failure mode of a two-structure sorted set: the score map is updated but
    // the ordered index keeps the old entry, so the member appears twice in a range
    // and its rank is wrong.
    let store = seeded();
    let keyspace = store.default_keyspace();

    for score in [10.0, 0.5, 2.0, -1.0] {
        keyspace
            .sorted_set_add(b"z", &[(score, b"a")], KvWriteCondition::Always, false, false)
            .unwrap();

        let all = keyspace.sorted_set_range(b"z", 0, -1, false).unwrap();
        assert_eq!(all.len(), 4, "re-scoring to {score} changed the member count");
        assert_eq!(
            names(&all).iter().filter(|name| *name == "a").count(),
            1,
            "member 'a' appears more than once after re-scoring to {score}"
        );
        assert_eq!(keyspace.sorted_set_score(b"z", b"a").unwrap(), Some(score));
    }
}

#[test]
fn commands_reject_the_wrong_type() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.set(b"str", b"v", None, KvWriteCondition::Always, false);

    assert_eq!(
        keyspace.sorted_set_add(b"str", &[(1.0, b"a")], KvWriteCondition::Always, false, false),
        Err(KvError::WrongType)
    );
    assert_eq!(keyspace.sorted_set_score(b"str", b"a"), Err(KvError::WrongType));
    assert_eq!(keyspace.sorted_set_incr_by(b"str", b"a", 1.0), Err(KvError::WrongType));

    keyspace.sorted_set_add(b"z", &[(1.0, b"a")], KvWriteCondition::Always, false, false).unwrap();
    assert_eq!(keyspace.set_members(b"z"), Err(KvError::WrongType));
    assert_eq!(keyspace.get(b"z"), Err(KvError::WrongType));
    assert_eq!(keyspace.list_len(b"z"), Err(KvError::WrongType));
}

#[test]
fn sets_and_sorted_sets_expire_like_everything_else() {
    let clock = Arc::new(ManualClock::default());
    let mut options = KvOptions::default();
    options.clock = Some(clock.clone());
    let store = KvStore::with_options(options);
    let keyspace = store.default_keyspace();

    keyspace.set_add(b"s", &[b"a"]).unwrap();
    keyspace.sorted_set_add(b"z", &[(1.0, b"a")], KvWriteCondition::Always, false, false).unwrap();
    keyspace.expire(b"s", Some(Duration::from_secs(1)));
    keyspace.expire(b"z", Some(Duration::from_secs(1)));

    clock.advance(Duration::from_secs(1));

    assert_eq!(keyspace.set_len(b"s").unwrap(), 0);
    assert_eq!(keyspace.sorted_set_len(b"z").unwrap(), 0);
    assert_eq!(keyspace.kind(b"s"), KvValueType::None);
    assert_eq!(keyspace.kind(b"z"), KvValueType::None);

    // An expired collection is replaced rather than added to.
    assert_eq!(keyspace.set_add(b"s", &[b"b"]).unwrap(), 1);
    assert_eq!(keyspace.set_len(b"s").unwrap(), 1);
    assert_eq!(keyspace.time_to_live(b"s"), None);
}

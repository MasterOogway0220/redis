//! Behavioural tests for the engine, written against the public API only.
//!
//! Expiry is asserted with a controllable clock rather than by sleeping. That is the
//! capability the test-double story is sold on, so it had better work here first.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kvlite_core::{KvClock, KvError, KvOptions, KvStore, KvValueType, KvWriteCondition, Store};

/// A clock that only moves when a test moves it.
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

fn store_with_clock() -> (KvStore, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::default());
    let mut options = KvOptions::default();
    options.clock = Some(clock.clone());
    (KvStore::with_options(options), clock)
}

// ---- strings and counters -------------------------------------------------

#[test]
fn stores_and_reads_a_string() {
    let store = KvStore::new();
    assert!(store.set("k", "v", None));
    assert_eq!(store.get_str("k").unwrap().as_deref(), Some("v"));
    assert_eq!(store.get("missing").unwrap(), None);
}

#[test]
fn keys_and_values_are_binary_safe() {
    let store = KvStore::new();
    let key = [0u8, 0xff, b'\n', 0x80];
    let value = [0xde, 0xad, 0, 0xbe, 0xef];

    store.set(key, value, None);
    assert_eq!(store.get(key).unwrap().as_deref(), Some(&value[..]));
}

#[test]
fn counters_start_at_zero_and_reject_nonsense() {
    let store = KvStore::new();
    assert_eq!(store.incr_by("hits", 1).unwrap(), 1);
    assert_eq!(store.incr_by("hits", 41).unwrap(), 42);
    assert_eq!(store.incr_by("hits", -42).unwrap(), 0);

    store.set("word", "banana", None);
    assert_eq!(store.incr_by("word", 1), Err(KvError::NotAnInteger));

    store.set("big", i64::MAX.to_string(), None);
    assert_eq!(store.incr_by("big", 1), Err(KvError::OutOfRange));
}

#[test]
fn write_conditions_behave_like_nx_and_xx() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    assert!(keyspace.set(b"k", b"first", None, KvWriteCondition::NotExists, false));
    assert!(!keyspace.set(b"k", b"second", None, KvWriteCondition::NotExists, false));
    assert_eq!(store.get_str("k").unwrap().as_deref(), Some("first"));

    assert!(keyspace.set(b"k", b"third", None, KvWriteCondition::Exists, false));
    assert!(!keyspace.set(b"absent", b"v", None, KvWriteCondition::Exists, false));
    assert_eq!(store.get_str("k").unwrap().as_deref(), Some("third"));
}

#[test]
fn append_and_strlen_track_each_other() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    assert_eq!(keyspace.append(b"k", b"ab").unwrap(), 2);
    assert_eq!(keyspace.append(b"k", b"cd").unwrap(), 4);
    assert_eq!(keyspace.strlen(b"k").unwrap(), 4);
    assert_eq!(keyspace.strlen(b"missing").unwrap(), 0);
    assert_eq!(store.get_str("k").unwrap().as_deref(), Some("abcd"));
}

// ---- expiry ---------------------------------------------------------------

#[test]
fn a_key_disappears_the_instant_its_deadline_passes() {
    let (store, clock) = store_with_clock();
    store.set("k", "v", Some(Duration::from_secs(30)));

    clock.advance(Duration::from_secs(29));
    assert!(store.contains("k"));

    clock.advance(Duration::from_secs(1));
    assert!(!store.contains("k"), "deadline is inclusive, as it is in Redis");
    assert_eq!(store.get("k").unwrap(), None);
    assert_eq!(store.default_keyspace().len(), 0);
}

#[test]
fn time_to_live_counts_down_and_can_be_cleared() {
    let (store, clock) = store_with_clock();
    store.set("k", "v", Some(Duration::from_secs(10)));
    assert_eq!(store.time_to_live("k"), Some(Duration::from_secs(10)));

    clock.advance(Duration::from_secs(4));
    assert_eq!(store.time_to_live("k"), Some(Duration::from_secs(6)));

    assert!(store.expire("k", None));
    assert_eq!(store.time_to_live("k"), None);
    assert!(store.contains("k"), "clearing the TTL must not remove the key");

    assert!(!store.expire("absent", Some(Duration::from_secs(1))));
}

#[test]
fn incrementing_preserves_the_expiry_and_setting_replaces_it() {
    let (store, clock) = store_with_clock();

    store.set("n", "1", Some(Duration::from_secs(10)));
    store.incr_by("n", 1).unwrap();
    assert_eq!(store.time_to_live("n"), Some(Duration::from_secs(10)));

    // A plain SET clears the old deadline.
    store.set("n", "1", None);
    assert_eq!(store.time_to_live("n"), None);

    // KEEPTTL puts it back under the caller's control.
    store.set("n", "1", Some(Duration::from_secs(10)));
    store.default_keyspace().set(b"n", b"2", None, KvWriteCondition::Always, true);
    assert_eq!(store.time_to_live("n"), Some(Duration::from_secs(10)));

    clock.advance(Duration::from_secs(10));
    assert!(!store.contains("n"));
}

#[test]
fn sweeping_reclaims_what_nobody_read() {
    let (store, clock) = store_with_clock();
    for i in 0..10 {
        store.set(format!("k{i}"), "v", Some(Duration::from_secs(1)));
    }
    assert_eq!(store.sweep_expired(100), 0, "nothing has expired yet");

    clock.advance(Duration::from_secs(2));
    assert_eq!(store.sweep_expired(4), 4, "sampling is bounded");
    assert_eq!(store.sweep_expired(100), 6);
    assert_eq!(store.sweep_expired(100), 0);
}

#[test]
fn an_expired_key_is_replaced_not_appended_to() {
    let (store, clock) = store_with_clock();
    let keyspace = store.default_keyspace();

    keyspace.push_back(b"list", &[b"a"]).unwrap();
    keyspace.expire(b"list", Some(Duration::from_secs(1)));
    clock.advance(Duration::from_secs(1));

    // The old list is gone, so this starts a fresh one rather than resurrecting it.
    assert_eq!(keyspace.push_back(b"list", &[b"b"]).unwrap(), 1);
    assert_eq!(keyspace.list_range(b"list", 0, -1).unwrap(), vec![b"b".to_vec()]);
    assert_eq!(keyspace.time_to_live(b"list"), None);
}

// ---- type errors ----------------------------------------------------------

#[test]
fn the_wrong_type_is_an_error_not_a_surprise() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    keyspace.push_back(b"list", &[b"a"]).unwrap();
    assert_eq!(keyspace.get(b"list"), Err(KvError::WrongType));
    assert_eq!(keyspace.incr_by(b"list", 1), Err(KvError::WrongType));
    assert_eq!(keyspace.hash_get(b"list", b"f"), Err(KvError::WrongType));

    keyspace.set(b"str", b"v", None, KvWriteCondition::Always, false);
    assert_eq!(keyspace.list_len(b"str"), Err(KvError::WrongType));
    assert_eq!(keyspace.push_back(b"str", &[b"a"]), Err(KvError::WrongType));

    // SET is type-agnostic and replaces whatever was there.
    assert!(keyspace.set(b"list", b"now a string", None, KvWriteCondition::Always, false));
    assert_eq!(keyspace.kind(b"list"), KvValueType::String);
}

#[test]
fn reports_the_type_of_each_key() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    keyspace.set(b"s", b"v", None, KvWriteCondition::Always, false);
    keyspace.push_back(b"l", &[b"a"]).unwrap();
    keyspace.hash_set(b"h", b"f", b"v").unwrap();

    assert_eq!(keyspace.kind(b"s"), KvValueType::String);
    assert_eq!(keyspace.kind(b"l"), KvValueType::List);
    assert_eq!(keyspace.kind(b"h"), KvValueType::Hash);
    assert_eq!(keyspace.kind(b"absent"), KvValueType::None);
}

// ---- lists ----------------------------------------------------------------

#[test]
fn lists_push_and_pop_from_both_ends() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    // LPUSH inserts each argument in turn, so the last one ends up first.
    assert_eq!(keyspace.push_front(b"l", &[b"b", b"a"]).unwrap(), 2);
    assert_eq!(keyspace.push_back(b"l", &[b"c"]).unwrap(), 3);
    assert_eq!(
        keyspace.list_range(b"l", 0, -1).unwrap(),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );

    assert_eq!(keyspace.pop_front(b"l").unwrap(), Some(b"a".to_vec()));
    assert_eq!(keyspace.pop_back(b"l").unwrap(), Some(b"c".to_vec()));
    assert_eq!(keyspace.list_len(b"l").unwrap(), 1);
}

#[test]
fn an_emptied_list_takes_its_key_with_it() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    keyspace.push_back(b"l", &[b"only"]).unwrap();
    keyspace.pop_front(b"l").unwrap();

    assert!(!keyspace.contains(b"l"), "Redis does not keep an empty list");
    assert_eq!(keyspace.kind(b"l"), KvValueType::None);
    assert_eq!(keyspace.pop_front(b"l").unwrap(), None);
}

#[test]
fn list_ranges_follow_the_redis_clamping_rules() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.push_back(b"l", &[b"a", b"b", b"c", b"d", b"e"]).unwrap();

    let range = |start, stop| keyspace.list_range(b"l", start, stop).unwrap();

    assert_eq!(range(0, -1).len(), 5);
    assert_eq!(range(1, 3), vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]);
    assert_eq!(range(-2, -1), vec![b"d".to_vec(), b"e".to_vec()]);
    assert_eq!(range(-100, 100), range(0, -1), "out-of-range bounds clamp");
    assert_eq!(range(3, 1), Vec::<Vec<u8>>::new(), "an inverted range is empty");
    assert_eq!(range(10, 20), Vec::<Vec<u8>>::new());
    assert_eq!(keyspace.list_range(b"absent", 0, -1).unwrap(), Vec::<Vec<u8>>::new());
}

#[test]
fn list_index_and_set_accept_negative_positions() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.push_back(b"l", &[b"a", b"b", b"c"]).unwrap();

    assert_eq!(keyspace.list_index(b"l", 0).unwrap(), Some(b"a".to_vec()));
    assert_eq!(keyspace.list_index(b"l", -1).unwrap(), Some(b"c".to_vec()));
    assert_eq!(keyspace.list_index(b"l", 3).unwrap(), None);
    assert_eq!(keyspace.list_index(b"l", -4).unwrap(), None);

    assert!(keyspace.list_set(b"l", -1, b"z").unwrap());
    assert_eq!(keyspace.list_index(b"l", 2).unwrap(), Some(b"z".to_vec()));
    assert!(!keyspace.list_set(b"l", 99, b"z").unwrap());
}

#[test]
fn trimming_keeps_a_window_and_can_empty_the_key() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.push_back(b"l", &[b"a", b"b", b"c", b"d"]).unwrap();

    keyspace.list_trim(b"l", 1, 2).unwrap();
    assert_eq!(keyspace.list_range(b"l", 0, -1).unwrap(), vec![b"b".to_vec(), b"c".to_vec()]);

    keyspace.list_trim(b"l", 5, 10).unwrap();
    assert!(!keyspace.contains(b"l"));
}

#[test]
fn removing_honours_the_lrem_count_sign() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    let fill = || {
        keyspace.remove(b"l");
        keyspace.push_back(b"l", &[b"x", b"a", b"x", b"b", b"x"]).unwrap();
    };

    fill();
    assert_eq!(keyspace.list_remove(b"l", 2, b"x").unwrap(), 2);
    assert_eq!(
        keyspace.list_range(b"l", 0, -1).unwrap(),
        vec![b"a".to_vec(), b"b".to_vec(), b"x".to_vec()],
        "a positive count removes from the head"
    );

    fill();
    assert_eq!(keyspace.list_remove(b"l", -2, b"x").unwrap(), 2);
    assert_eq!(
        keyspace.list_range(b"l", 0, -1).unwrap(),
        vec![b"x".to_vec(), b"a".to_vec(), b"b".to_vec()],
        "a negative count removes from the tail"
    );

    fill();
    assert_eq!(keyspace.list_remove(b"l", 0, b"x").unwrap(), 3, "zero removes them all");
}

// ---- hashes ---------------------------------------------------------------

#[test]
fn hashes_set_read_and_delete_fields() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    assert!(keyspace.hash_set(b"h", b"a", b"1").unwrap(), "a new field reports true");
    assert!(!keyspace.hash_set(b"h", b"a", b"2").unwrap(), "an overwrite reports false");
    assert_eq!(keyspace.hash_get(b"h", b"a").unwrap(), Some(b"2".to_vec()));
    assert_eq!(keyspace.hash_get(b"h", b"absent").unwrap(), None);

    keyspace.hash_set(b"h", b"b", b"3").unwrap();
    assert_eq!(keyspace.hash_len(b"h").unwrap(), 2);
    assert!(keyspace.hash_contains(b"h", b"b").unwrap());

    let mut entries = keyspace.hash_entries(b"h").unwrap();
    entries.sort();
    assert_eq!(entries, vec![(b"a".to_vec(), b"2".to_vec()), (b"b".to_vec(), b"3".to_vec())]);

    assert!(keyspace.hash_remove(b"h", b"a").unwrap());
    assert!(!keyspace.hash_remove(b"h", b"a").unwrap());
}

#[test]
fn an_emptied_hash_takes_its_key_with_it() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    keyspace.hash_set(b"h", b"only", b"v").unwrap();
    keyspace.hash_remove(b"h", b"only").unwrap();
    assert!(!keyspace.contains(b"h"));
}

#[test]
fn hash_counters_behave_like_string_counters() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    assert_eq!(keyspace.hash_incr_by(b"h", b"n", 5).unwrap(), 5);
    assert_eq!(keyspace.hash_incr_by(b"h", b"n", -2).unwrap(), 3);

    keyspace.hash_set(b"h", b"word", b"nope").unwrap();
    assert_eq!(keyspace.hash_incr_by(b"h", b"word", 1), Err(KvError::NotAnInteger));
}

// ---- keyspaces and the store ----------------------------------------------

#[test]
fn keyspaces_are_isolated_from_each_other() {
    let store = KvStore::new();
    store.keyspace(0).unwrap().set(b"k", b"zero", None, KvWriteCondition::Always, false);
    store.keyspace(1).unwrap().set(b"k", b"one", None, KvWriteCondition::Always, false);

    assert_eq!(store.keyspace(0).unwrap().get(b"k").unwrap(), Some(b"zero".to_vec()));
    assert_eq!(store.keyspace(1).unwrap().get(b"k").unwrap(), Some(b"one".to_vec()));

    store.keyspace(0).unwrap().clear();
    assert!(!store.keyspace(0).unwrap().contains(b"k"));
    assert!(store.keyspace(1).unwrap().contains(b"k"), "FLUSHDB is not FLUSHALL");

    assert_eq!(store.keyspace_count(), 16);
    assert!(store.keyspace(16).is_none());
}

#[test]
fn two_stores_share_nothing() {
    let first = KvStore::new();
    let second = KvStore::new();

    first.set("k", "v", None);
    assert!(!second.contains("k"), "there is no static mutable state to share");
}

#[test]
fn the_abstraction_forwards_to_the_engine() {
    // If a trait method accidentally called itself instead of the inherent method,
    // this would blow the stack rather than fail an assertion.
    let store = KvStore::new();
    let dynamic: &dyn Store = &store;

    let keyspace = dynamic.keyspace(0).expect("keyspace 0 exists");
    assert_eq!(keyspace.index(), 0);
    assert!(keyspace.is_empty());

    assert!(keyspace.set(b"k", b"v", None, KvWriteCondition::Always, false));
    assert_eq!(keyspace.get(b"k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(keyspace.strlen(b"k").unwrap(), 1);
    assert_eq!(keyspace.kind(b"k"), KvValueType::String);
    assert_eq!(keyspace.len(), 1);
    assert!(keyspace.contains(b"k"));

    assert!(keyspace.expire(b"k", Some(Duration::from_secs(60))));
    assert!(keyspace.time_to_live(b"k").is_some());

    assert_eq!(keyspace.incr_by(b"n", 3).unwrap(), 3);
    assert!(keyspace.remove(b"k"));
    keyspace.clear();
    assert!(keyspace.is_empty());
    assert_eq!(dynamic.keyspace_count(), 16);
}

#[test]
fn a_store_is_shareable_across_threads() {
    let store = Arc::new(KvStore::new());
    let mut handles = Vec::new();

    for worker in 0..8 {
        let store = Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                store.incr_by("shared", 1).unwrap();
            }
            store.set(format!("worker:{worker}"), "done", None);
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(store.get_str("shared").unwrap().as_deref(), Some("8000"));
    assert_eq!(store.default_keyspace().len(), 9);
}

#[test]
#[should_panic(expected = "at least one keyspace")]
fn a_store_with_no_keyspaces_is_a_programming_error() {
    let mut options = KvOptions::default();
    options.keyspace_count = 0;
    let _ = KvStore::with_options(options);
}

// ---- sets -----------------------------------------------------------------

#[test]
fn sets_hold_each_member_once() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    assert_eq!(keyspace.set_add(b"s", &[b"a", b"b", b"a"]).unwrap(), 2, "duplicates count once");
    assert_eq!(keyspace.set_add(b"s", &[b"b", b"c"]).unwrap(), 1);
    assert_eq!(keyspace.set_len(b"s").unwrap(), 3);

    assert!(keyspace.set_contains(b"s", b"a").unwrap());
    assert!(!keyspace.set_contains(b"s", b"z").unwrap());
    assert!(!keyspace.set_contains(b"absent", b"a").unwrap());

    let mut members = keyspace.set_members(b"s").unwrap();
    members.sort();
    assert_eq!(members, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);

    assert_eq!(keyspace.set_remove(b"s", &[b"a", b"missing"]).unwrap(), 1);
    assert_eq!(keyspace.kind(b"s"), KvValueType::Set);
}

#[test]
fn an_emptied_set_takes_its_key_with_it() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();

    keyspace.set_add(b"s", &[b"only"]).unwrap();
    keyspace.set_remove(b"s", &[b"only"]).unwrap();
    assert!(!keyspace.contains(b"s"));

    // And adding nothing at all must not conjure an empty one.
    assert_eq!(keyspace.set_add(b"fresh", &[]).unwrap(), 0);
    assert!(!keyspace.contains(b"fresh"));
}

#[test]
fn popping_removes_what_it_returns() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.set_add(b"s", &[b"a", b"b", b"c"]).unwrap();

    let popped = keyspace.set_pop(b"s", 2).unwrap();
    assert_eq!(popped.len(), 2);
    assert_eq!(keyspace.set_len(b"s").unwrap(), 1);
    for member in &popped {
        assert!(!keyspace.set_contains(b"s", member).unwrap());
    }

    assert_eq!(keyspace.set_pop(b"s", 99).unwrap().len(), 1, "takes what there is");
    assert!(!keyspace.contains(b"s"));
}

#[test]
fn random_members_honour_the_sign_of_the_count() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.set_add(b"s", &[b"a", b"b"]).unwrap();

    assert_eq!(keyspace.set_random_members(b"s", 5).unwrap().len(), 2, "capped at the set size");
    assert_eq!(
        keyspace.set_random_members(b"s", -5).unwrap().len(),
        5,
        "a negative count returns exactly that many, repeating as needed"
    );
    assert_eq!(keyspace.set_len(b"s").unwrap(), 2, "nothing was removed");
    assert!(keyspace.set_random_members(b"absent", 3).unwrap().is_empty());
}

#[test]
fn moving_a_member_between_sets() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.set_add(b"from", &[b"x", b"y"]).unwrap();
    keyspace.set_add(b"to", &[b"z"]).unwrap();

    assert!(keyspace.set_move(b"from", b"to", b"x").unwrap());
    assert!(!keyspace.set_contains(b"from", b"x").unwrap());
    assert!(keyspace.set_contains(b"to", b"x").unwrap());

    assert!(!keyspace.set_move(b"from", b"to", b"absent").unwrap());

    // Moving into the set it is already in keeps the key and its expiry intact.
    keyspace.expire(b"to", Some(Duration::from_secs(60)));
    assert!(keyspace.set_move(b"to", b"to", b"z").unwrap());
    assert!(keyspace.time_to_live(b"to").is_some());
}

#[test]
fn set_algebra_treats_a_missing_key_as_empty() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.set_add(b"a", &[b"1", b"2", b"3"]).unwrap();
    keyspace.set_add(b"b", &[b"2", b"3", b"4"]).unwrap();

    let sorted = |mut members: Vec<Vec<u8>>| {
        members.sort();
        members
    };

    assert_eq!(
        sorted(keyspace.set_union(&[b"a", b"b"]).unwrap()),
        vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec(), b"4".to_vec()]
    );
    assert_eq!(
        sorted(keyspace.set_intersect(&[b"a", b"b"]).unwrap()),
        vec![b"2".to_vec(), b"3".to_vec()]
    );
    assert_eq!(sorted(keyspace.set_difference(&[b"a", b"b"]).unwrap()), vec![b"1".to_vec()]);

    assert_eq!(sorted(keyspace.set_union(&[b"a", b"absent"]).unwrap()).len(), 3);
    assert!(keyspace.set_intersect(&[b"a", b"absent"]).unwrap().is_empty());
    assert_eq!(sorted(keyspace.set_difference(&[b"a", b"absent"]).unwrap()).len(), 3);
}

#[test]
fn storing_a_combination_replaces_the_destination() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.set_add(b"a", &[b"1", b"2"]).unwrap();
    keyspace.set_add(b"b", &[b"2", b"3"]).unwrap();
    keyspace.set_add(b"dest", &[b"stale"]).unwrap();

    assert_eq!(keyspace.set_intersect_store(b"dest", &[b"a", b"b"]).unwrap(), 1);
    assert_eq!(keyspace.set_members(b"dest").unwrap(), vec![b"2".to_vec()]);

    // An empty result deletes the destination rather than leaving an empty set.
    assert_eq!(keyspace.set_intersect_store(b"dest", &[b"a", b"absent"]).unwrap(), 0);
    assert!(!keyspace.contains(b"dest"));
}

#[test]
fn set_commands_reject_the_wrong_type() {
    let store = KvStore::new();
    let keyspace = store.default_keyspace();
    keyspace.set(b"str", b"v", None, KvWriteCondition::Always, false);

    assert_eq!(keyspace.set_add(b"str", &[b"a"]), Err(KvError::WrongType));
    assert_eq!(keyspace.set_members(b"str"), Err(KvError::WrongType));
    assert_eq!(keyspace.set_union(&[b"str"]), Err(KvError::WrongType));

    keyspace.set_add(b"s", &[b"a"]).unwrap();
    assert_eq!(keyspace.get(b"s"), Err(KvError::WrongType));
    assert_eq!(keyspace.list_len(b"s"), Err(KvError::WrongType));
}

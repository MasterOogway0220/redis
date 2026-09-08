//! Door 1, embedded: every example from the root README's embedded section.
//!
//! The crate READMEs are included as rustdoc and so their examples run as doctests.
//! The *root* README has no such crate to hang off, and it is the page an adopter
//! reads first — so it gets checked here instead. If you change an example there,
//! change it here, and CI will tell you when they disagree.
//!
//! Run it with: `cargo run --manifest-path examples/consumer/Cargo.toml`

use kvlite_core::{KvError, KvOptions, KvStore, KvWriteCondition};
use std::ops::Bound;
use std::sync::Arc;
use std::time::Duration;

fn main() -> Result<(), KvError> {
    strings_and_counters()?;
    binary_keys_and_values()?;
    the_other_data_types()?;
    sharing_it()?;
    configuring_it();

    println!("every embedded example from the root README ran");
    Ok(())
}

fn strings_and_counters() -> Result<(), KvError> {
    let store = KvStore::new();

    // Strings, with an optional TTL.
    store.set("session:abc", "user-42", Some(Duration::from_secs(30 * 60)));
    let user: Option<String> = store.get_str("session:abc")?;
    assert_eq!(user.as_deref(), Some("user-42"));

    // Counters. A missing key starts at zero.
    assert_eq!(store.incr_by("page:views", 1)?, 1);
    assert_eq!(store.incr_by("page:views", 9)?, 10);

    // Key lifetime.
    assert!(store.contains("session:abc"));
    assert!(store.time_to_live("session:abc").is_some());
    store.expire("session:abc", None);
    assert!(store.time_to_live("session:abc").is_none());
    assert!(store.remove("session:abc"));

    Ok(())
}

fn binary_keys_and_values() -> Result<(), KvError> {
    let store = KvStore::new();

    store.set([0u8, 0xff, 0x80], [0xde, 0xad, 0xbe, 0xef], None);
    let raw: Option<Vec<u8>> = store.get([0u8, 0xff, 0x80])?;
    assert_eq!(raw.as_deref(), Some(&[0xde, 0xad, 0xbe, 0xef][..]));

    Ok(())
}

fn the_other_data_types() -> Result<(), KvError> {
    let store = KvStore::new();
    let kv = store.default_keyspace();

    // Hash
    kv.hash_set(b"user:1", b"name", b"ana")?;
    kv.hash_incr_by(b"user:1", b"logins", 1)?;
    let fields = kv.hash_entries(b"user:1")?;
    assert_eq!(fields.len(), 2);

    // List
    kv.push_back(b"queue", &[b"first", b"second"])?;
    let job = kv.pop_front(b"queue")?;
    let all = kv.list_range(b"queue", 0, -1)?;
    assert_eq!(job, Some(b"first".to_vec()));
    assert_eq!(all, vec![b"second".to_vec()]);

    // Set
    kv.set_add(b"tags", &[b"rust", b"redis"])?;
    assert!(kv.set_contains(b"tags", b"rust")?);
    let shared = kv.set_intersect(&[b"tags", b"other"])?;
    assert!(shared.is_empty());

    // Sorted set
    kv.sorted_set_add(
        b"leaderboard",
        &[(120.0, b"ana"), (95.0, b"bo")],
        KvWriteCondition::Always,
        false, // GT: only raise an existing score
        false, // LT: only lower an existing score
    )?;
    let top = kv.sorted_set_range(b"leaderboard", 0, 0, true)?;
    assert_eq!(top[0].0, b"ana".to_vec());

    // Everything scoring above 100, exclusive.
    let winners = kv.sorted_set_range_by_score(
        b"leaderboard",
        Bound::Excluded(100.0),
        Bound::Unbounded,
        false,
    )?;
    assert_eq!(winners.len(), 1);

    Ok(())
}

fn sharing_it() -> Result<(), KvError> {
    let store = Arc::new(KvStore::new());
    let worker = Arc::clone(&store);

    let handle = std::thread::spawn(move || {
        worker.incr_by("jobs:done", 1).unwrap();
    });
    handle.join().unwrap();

    assert_eq!(store.get_str("jobs:done")?.as_deref(), Some("1"));
    Ok(())
}

fn configuring_it() {
    let mut options = KvOptions::default();
    options.keyspace_count = 4; // Redis SELECT databases; default 16
    // options.clock = Some(my_clock);  // anything implementing KvClock

    let store = KvStore::with_options(options);
    assert_eq!(store.keyspace_count(), 4);

    // Expiry is lazy; this is only about reclaiming memory from keys nobody reads.
    store.sweep_expired(20);
}

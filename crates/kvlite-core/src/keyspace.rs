use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kvlite_api::{KvClock, KvError, KvResult, KvValueType, KvWriteCondition};

use crate::int::{format_int, parse_int};
use crate::sorted_set::SortedSet;

/// What a key holds.
enum Value {
    String(Vec<u8>),
    List(VecDeque<Vec<u8>>),
    Hash(HashMap<Vec<u8>, Vec<u8>>),
    Set(HashSet<Vec<u8>>),
    SortedSet(SortedSet),
}

impl Value {
    fn kind(&self) -> KvValueType {
        match self {
            Self::String(_) => KvValueType::String,
            Self::List(_) => KvValueType::List,
            Self::Hash(_) => KvValueType::Hash,
            Self::Set(_) => KvValueType::Set,
            Self::SortedSet(_) => KvValueType::SortedSet,
        }
    }

    /// Whether an aggregate has become empty. Redis drops such a key entirely.
    fn is_empty_aggregate(&self) -> bool {
        match self {
            Self::String(_) => false,
            Self::List(items) => items.is_empty(),
            Self::Hash(fields) => fields.is_empty(),
            Self::Set(members) => members.is_empty(),
            Self::SortedSet(members) => members.is_empty(),
        }
    }
}

struct Entry {
    value: Value,
    /// Absolute deadline in milliseconds since the epoch. `None` means no expiry.
    expires_at: Option<u64>,
}

type Entries = HashMap<Vec<u8>, Entry>;

/// One logical database.
///
/// Every method takes `&self` and locks internally, so a keyspace is shared rather
/// than owned. Keys and values are binary-safe byte strings, not UTF-8.
///
/// Expiry is lazy. A key past its deadline is invisible to every read and is dropped
/// the moment it is next touched, so nothing here needs a background thread. See
/// [`KvKeyspace::sweep_expired`] for reclaiming keys nobody reads.
pub struct KvKeyspace {
    index: usize,
    clock: Arc<dyn KvClock>,
    // ponytail: one lock per keyspace. Redis is single-threaded, so this is
    // semantically identical and much easier to reason about. Shard by key hash
    // if a benchmark ever shows the lock is the bottleneck.
    entries: Mutex<Entries>,
}

impl KvKeyspace {
    pub(crate) fn new(index: usize, clock: Arc<dyn KvClock>) -> Self {
        Self { index, clock, entries: Mutex::new(HashMap::new()) }
    }

    /// Zero-based index of this keyspace within its store.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// A poisoned lock is recovered rather than propagated: one panicking caller
    /// should not take the whole store down with it.
    fn guard(&self) -> MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads the clock before taking the lock, so no syscall happens under it.
    fn now(&self) -> u64 {
        self.clock.now_millis()
    }

    // ---- keys -------------------------------------------------------------

    /// Number of live keys.
    ///
    /// This walks the keyspace, because a key that has passed its deadline but has
    /// not been touched is still in the map and must not be counted. Redis's
    /// `DBSIZE` is O(1) and does count those; ours is O(n) and does not, which is
    /// the behaviour a test asserting expiry actually wants.
    #[must_use]
    pub fn len(&self) -> u64 {
        let now = self.now();
        self.guard().values().filter(|entry| !is_expired(entry, now)).count() as u64
    }

    /// Whether the keyspace holds no live keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the key exists and has not expired.
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        let now = self.now();
        live(&mut self.guard(), key, now).is_some()
    }

    /// Removes the key. Returns whether it existed.
    pub fn remove(&self, key: &[u8]) -> bool {
        let now = self.now();
        let mut map = self.guard();
        live(&mut map, key, now).is_some() && map.remove(key).is_some()
    }

    /// Removes every key.
    pub fn clear(&self) {
        self.guard().clear();
    }

    /// Every live key. O(n), as `KEYS` is in Redis.
    #[must_use]
    pub fn keys(&self) -> Vec<Vec<u8>> {
        let now = self.now();
        self.guard()
            .iter()
            .filter(|(_, entry)| !is_expired(entry, now))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Type of the value stored at the key.
    #[must_use]
    pub fn kind(&self, key: &[u8]) -> KvValueType {
        let now = self.now();
        live(&mut self.guard(), key, now).map_or(KvValueType::None, |entry| entry.value.kind())
    }

    /// Sets or clears the key's time to live. Returns whether the key existed.
    pub fn expire(&self, key: &[u8], time_to_live: Option<Duration>) -> bool {
        let now = self.now();
        let mut map = self.guard();
        let Some(entry) = live(&mut map, key, now) else {
            return false;
        };
        entry.expires_at = time_to_live.map(|ttl| deadline(now, ttl));
        true
    }

    /// Remaining lifetime of the key.
    ///
    /// `None` covers both "no such key" and "key has no expiry"; use
    /// [`KvKeyspace::contains`] to tell them apart.
    #[must_use]
    pub fn time_to_live(&self, key: &[u8]) -> Option<Duration> {
        let now = self.now();
        let mut map = self.guard();
        let expires_at = live(&mut map, key, now)?.expires_at?;
        Some(Duration::from_millis(expires_at.saturating_sub(now)))
    }

    /// Drops up to `sample_size` expired keys and returns how many it removed.
    ///
    /// Sampling rather than scanning keeps the pause bounded no matter how large the
    /// keyspace is. A caller that wants memory back promptly calls this on a timer;
    /// correctness never depends on it, because expiry is already lazy.
    pub fn sweep_expired(&self, sample_size: usize) -> usize {
        let now = self.now();
        let mut map = self.guard();

        let doomed: Vec<Vec<u8>> = map
            .iter()
            .filter(|(_, entry)| is_expired(entry, now))
            .take(sample_size)
            .map(|(key, _)| key.clone())
            .collect();

        for key in &doomed {
            map.remove(key);
        }
        doomed.len()
    }

    // ---- strings ----------------------------------------------------------

    /// Stores a binary-safe string. Returns whether the write was applied.
    ///
    /// `keep_time_to_live` retains the existing deadline and ignores `time_to_live`,
    /// matching Redis `KEEPTTL`. Setting a key that holds another type replaces it,
    /// as Redis `SET` does.
    pub fn set(
        &self,
        key: &[u8],
        value: &[u8],
        time_to_live: Option<Duration>,
        condition: KvWriteCondition,
        keep_time_to_live: bool,
    ) -> bool {
        let now = self.now();
        let mut map = self.guard();

        let existing = live(&mut map, key, now).map(|entry| entry.expires_at);
        match condition {
            KvWriteCondition::NotExists if existing.is_some() => return false,
            KvWriteCondition::Exists if existing.is_none() => return false,
            _ => {}
        }

        let expires_at = if keep_time_to_live {
            existing.flatten()
        } else {
            time_to_live.map(|ttl| deadline(now, ttl))
        };

        map.insert(key.to_vec(), Entry { value: Value::String(value.to_vec()), expires_at });
        true
    }

    /// Reads a binary-safe string, or `None` when the key is missing.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a string.
    pub fn get(&self, key: &[u8]) -> KvResult<Option<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(None),
            Some(entry) => Ok(Some(as_string(entry)?.clone())),
        }
    }

    /// Sets the key and returns what it held before, as Redis `GETSET` does.
    ///
    /// Clears any existing expiry, which is also what `GETSET` does.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a string.
    pub fn get_set(&self, key: &[u8], value: &[u8]) -> KvResult<Option<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();

        let previous = match live(&mut map, key, now) {
            None => None,
            Some(entry) => Some(as_string(entry)?.clone()),
        };

        map.insert(key.to_vec(), Entry { value: Value::String(value.to_vec()), expires_at: None });
        Ok(previous)
    }

    /// Adds `delta` to the integer at the key and returns the result.
    ///
    /// A missing key counts as zero. Any existing expiry is preserved, as Redis does.
    ///
    /// # Errors
    /// [`KvError::NotAnInteger`] when the stored value is not a 64-bit integer,
    /// [`KvError::OutOfRange`] when the result would overflow,
    /// [`KvError::WrongType`] when the key holds something that is not a string.
    pub fn incr_by(&self, key: &[u8], delta: i64) -> KvResult<i64> {
        let now = self.now();
        let mut map = self.guard();

        match live(&mut map, key, now) {
            None => {
                map.insert(
                    key.to_vec(),
                    Entry { value: Value::String(format_int(delta)), expires_at: None },
                );
                Ok(delta)
            }
            Some(entry) => {
                let text = as_string(entry)?;
                let current = parse_int(text).ok_or(KvError::NotAnInteger)?;
                let next = current.checked_add(delta).ok_or(KvError::OutOfRange)?;
                *text = format_int(next);
                Ok(next)
            }
        }
    }

    /// Length in bytes of the string at the key, or 0 when it is missing.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a string.
    pub fn strlen(&self, key: &[u8]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(0),
            Some(entry) => Ok(as_string(entry)?.len() as u64),
        }
    }

    /// Appends to the string at the key and returns its new length.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a string.
    pub fn append(&self, key: &[u8], suffix: &[u8]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();

        match live(&mut map, key, now) {
            None => {
                map.insert(
                    key.to_vec(),
                    Entry { value: Value::String(suffix.to_vec()), expires_at: None },
                );
                Ok(suffix.len() as u64)
            }
            Some(entry) => {
                let text = as_string(entry)?;
                text.extend_from_slice(suffix);
                Ok(text.len() as u64)
            }
        }
    }

    // ---- lists ------------------------------------------------------------

    /// Prepends values, last argument innermost, as Redis `LPUSH` does.
    /// Returns the new length.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn push_front(&self, key: &[u8], values: &[&[u8]]) -> KvResult<u64> {
        self.push(key, values, true)
    }

    /// Appends values and returns the new length.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn push_back(&self, key: &[u8], values: &[&[u8]]) -> KvResult<u64> {
        self.push(key, values, false)
    }

    fn push(&self, key: &[u8], values: &[&[u8]], front: bool) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();

        let entry = fresh_or_existing(&mut map, key, now, || Value::List(VecDeque::new()));

        let Value::List(items) = &mut entry.value else {
            return Err(KvError::WrongType);
        };

        for value in values {
            if front {
                items.push_front((*value).to_vec());
            } else {
                items.push_back((*value).to_vec());
            }
        }
        let length = items.len() as u64;

        // An empty push leaves an empty list behind, which Redis would not keep.
        if length == 0 {
            map.remove(key);
        }
        Ok(length)
    }

    /// Removes and returns the first element.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn pop_front(&self, key: &[u8]) -> KvResult<Option<Vec<u8>>> {
        self.pop(key, true)
    }

    /// Removes and returns the last element.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn pop_back(&self, key: &[u8]) -> KvResult<Option<Vec<u8>>> {
        self.pop(key, false)
    }

    fn pop(&self, key: &[u8], front: bool) -> KvResult<Option<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(None);
        };
        let Value::List(items) = &mut entry.value else {
            return Err(KvError::WrongType);
        };

        let popped = if front { items.pop_front() } else { items.pop_back() };
        drop_if_empty(&mut map, key);
        Ok(popped)
    }

    /// Number of elements in the list, or 0 when the key is missing.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn list_len(&self, key: &[u8]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(0),
            Some(entry) => Ok(as_list(entry)?.len() as u64),
        }
    }

    /// Elements between `start` and `stop`, both inclusive, with Redis's
    /// negative-index and clamping rules.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn list_range(&self, key: &[u8], start: i64, stop: i64) -> KvResult<Vec<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(Vec::new());
        };
        let items = as_list(entry)?;

        let Some((from, to)) = range_bounds(start, stop, items.len()) else {
            return Ok(Vec::new());
        };
        Ok(items.iter().skip(from).take(to - from + 1).cloned().collect())
    }

    /// The element at `index`, which may be negative.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn list_index(&self, key: &[u8], index: i64) -> KvResult<Option<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(None);
        };
        let items = as_list(entry)?;
        Ok(normalize(index, items.len()).and_then(|i| items.get(i).cloned()))
    }

    /// Replaces the element at `index`. Returns whether the index was in range.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn list_set(&self, key: &[u8], index: i64, value: &[u8]) -> KvResult<bool> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(false);
        };
        let items = as_list(entry)?;
        match normalize(index, items.len()).and_then(|i| items.get_mut(i)) {
            Some(slot) => {
                *slot = value.to_vec();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Keeps only the elements between `start` and `stop`, dropping the key if that
    /// leaves it empty.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn list_trim(&self, key: &[u8], start: i64, stop: i64) -> KvResult<()> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(());
        };
        let items = as_list(entry)?;

        match range_bounds(start, stop, items.len()) {
            Some((from, to)) => {
                items.drain(to + 1..);
                items.drain(..from);
            }
            None => items.clear(),
        }
        drop_if_empty(&mut map, key);
        Ok(())
    }

    /// Removes elements equal to `value`, following Redis `LREM` count semantics:
    /// positive counts from the head, negative from the tail, zero removes all.
    /// Returns how many were removed.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a list.
    pub fn list_remove(&self, key: &[u8], count: i64, value: &[u8]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(0);
        };
        let items = as_list(entry)?;

        let limit = if count == 0 { usize::MAX } else { count.unsigned_abs() as usize };
        let mut removed = 0;

        if count >= 0 {
            let mut index = 0;
            while index < items.len() && removed < limit {
                if items[index] == value {
                    items.remove(index);
                    removed += 1;
                } else {
                    index += 1;
                }
            }
        } else {
            let mut index = items.len();
            while index > 0 && removed < limit {
                index -= 1;
                if items[index] == value {
                    items.remove(index);
                    removed += 1;
                }
            }
        }

        drop_if_empty(&mut map, key);
        Ok(removed as u64)
    }

    // ---- hashes -----------------------------------------------------------

    /// Sets a field. Returns whether the field is new rather than overwritten.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a hash.
    pub fn hash_set(&self, key: &[u8], field: &[u8], value: &[u8]) -> KvResult<bool> {
        let now = self.now();
        let mut map = self.guard();

        let entry = fresh_or_existing(&mut map, key, now, || Value::Hash(HashMap::new()));

        let Value::Hash(fields) = &mut entry.value else {
            return Err(KvError::WrongType);
        };
        Ok(fields.insert(field.to_vec(), value.to_vec()).is_none())
    }

    /// Reads a field.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a hash.
    pub fn hash_get(&self, key: &[u8], field: &[u8]) -> KvResult<Option<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(None),
            Some(entry) => Ok(as_hash(entry)?.get(field).cloned()),
        }
    }

    /// Removes a field. Returns whether it existed. Drops the key if it empties.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a hash.
    pub fn hash_remove(&self, key: &[u8], field: &[u8]) -> KvResult<bool> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(false);
        };
        let existed = as_hash(entry)?.remove(field).is_some();
        drop_if_empty(&mut map, key);
        Ok(existed)
    }

    /// Whether the field exists.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a hash.
    pub fn hash_contains(&self, key: &[u8], field: &[u8]) -> KvResult<bool> {
        Ok(self.hash_get(key, field)?.is_some())
    }

    /// Number of fields, or 0 when the key is missing.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a hash.
    pub fn hash_len(&self, key: &[u8]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(0),
            Some(entry) => Ok(as_hash(entry)?.len() as u64),
        }
    }

    /// Every field and value.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a hash.
    pub fn hash_entries(&self, key: &[u8]) -> KvResult<Vec<(Vec<u8>, Vec<u8>)>> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(Vec::new()),
            Some(entry) => Ok(as_hash(entry)?
                .iter()
                .map(|(field, value)| (field.clone(), value.clone()))
                .collect()),
        }
    }

    /// Adds `delta` to the integer in a field and returns the result.
    ///
    /// # Errors
    /// [`KvError::NotAnInteger`], [`KvError::OutOfRange`] or [`KvError::WrongType`],
    /// on the same conditions as [`KvKeyspace::incr_by`].
    pub fn hash_incr_by(&self, key: &[u8], field: &[u8], delta: i64) -> KvResult<i64> {
        let now = self.now();
        let mut map = self.guard();

        let entry = fresh_or_existing(&mut map, key, now, || Value::Hash(HashMap::new()));

        let Value::Hash(fields) = &mut entry.value else {
            return Err(KvError::WrongType);
        };

        let current = match fields.get(field) {
            None => 0,
            Some(text) => parse_int(text).ok_or(KvError::NotAnInteger)?,
        };
        let next = current.checked_add(delta).ok_or(KvError::OutOfRange)?;
        fields.insert(field.to_vec(), format_int(next));
        Ok(next)
    }

    // ---- sets -------------------------------------------------------------

    /// Adds members, returning how many were not already present.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a set.
    pub fn set_add(&self, key: &[u8], members: &[&[u8]]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();

        let entry = fresh_or_existing(&mut map, key, now, || Value::Set(HashSet::new()));
        let members_at_key = match &mut entry.value {
            Value::Set(existing) => existing,
            _ => return Err(KvError::WrongType),
        };

        let added =
            members.iter().filter(|member| members_at_key.insert((*member).to_vec())).count();

        // SADD with no members would otherwise leave an empty set behind.
        let emptied = members_at_key.is_empty();
        if emptied {
            map.remove(key);
        }
        Ok(added as u64)
    }

    /// Removes members, returning how many were present. Drops the key if it empties.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a set.
    pub fn set_remove(&self, key: &[u8], members: &[&[u8]]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(0);
        };
        let existing = as_set(entry)?;
        let removed = members.iter().filter(|member| existing.remove(**member)).count();

        drop_if_empty(&mut map, key);
        Ok(removed as u64)
    }

    /// Whether the member is in the set.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a set.
    pub fn set_contains(&self, key: &[u8], member: &[u8]) -> KvResult<bool> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(false),
            Some(entry) => Ok(as_set(entry)?.contains(member)),
        }
    }

    /// Number of members, or 0 when the key is missing.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a set.
    pub fn set_len(&self, key: &[u8]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(0),
            Some(entry) => Ok(as_set(entry)?.len() as u64),
        }
    }

    /// Every member, in no particular order — as Redis also declines to promise.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a set.
    pub fn set_members(&self, key: &[u8]) -> KvResult<Vec<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(Vec::new()),
            Some(entry) => Ok(as_set(entry)?.iter().cloned().collect()),
        }
    }

    /// Removes and returns up to `count` members.
    ///
    /// Which members is arbitrary rather than uniformly random: they come off the
    /// hash table in its own order. Redis makes the same practical trade for small
    /// sets. Do not build a lottery on this.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a set.
    pub fn set_pop(&self, key: &[u8], count: usize) -> KvResult<Vec<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(Vec::new());
        };
        let existing = as_set(entry)?;

        let taken: Vec<Vec<u8>> = existing.iter().take(count).cloned().collect();
        for member in &taken {
            existing.remove(member);
        }

        drop_if_empty(&mut map, key);
        Ok(taken)
    }

    /// Members chosen without removing them.
    ///
    /// `count` follows Redis: a positive count returns up to that many distinct
    /// members, a negative count returns exactly that many and may repeat.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a set.
    pub fn set_random_members(&self, key: &[u8], count: i64) -> KvResult<Vec<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(Vec::new());
        };
        let existing = as_set(entry)?;
        if existing.is_empty() {
            return Ok(Vec::new());
        }

        if count >= 0 {
            return Ok(existing.iter().take(count as usize).cloned().collect());
        }

        // A negative count means "exactly this many, repeats allowed".
        let pool: Vec<&Vec<u8>> = existing.iter().collect();
        let wanted = count.unsigned_abs() as usize;
        let offset = (now as usize).wrapping_add(pool.len());
        Ok((0..wanted).map(|i| pool[offset.wrapping_add(i) % pool.len()].clone()).collect())
    }

    /// Moves a member from one set to another. Returns whether it was there to move.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when either key holds something that is not a set.
    pub fn set_move(&self, source: &[u8], destination: &[u8], member: &[u8]) -> KvResult<bool> {
        let now = self.now();
        let mut map = self.guard();

        // Check the destination's type before mutating the source, so a type error
        // cannot leave the member removed from one set and in neither.
        if let Some(entry) = live(&mut map, destination, now) {
            as_set(entry)?;
        }

        // Moving a member to the set it is already in is a no-op that still reports
        // success. Doing it the long way would drop and recreate a single-member key,
        // losing its expiry.
        if source == destination {
            let present = match live(&mut map, source, now) {
                None => false,
                Some(entry) => as_set(entry)?.contains(member),
            };
            return Ok(present);
        }

        let Some(entry) = live(&mut map, source, now) else {
            return Ok(false);
        };
        if !as_set(entry)?.remove(member) {
            return Ok(false);
        }
        drop_if_empty(&mut map, source);

        let entry = fresh_or_existing(&mut map, destination, now, || Value::Set(HashSet::new()));
        match &mut entry.value {
            Value::Set(members) => members.insert(member.to_vec()),
            _ => unreachable!("the destination type was checked above"),
        };
        Ok(true)
    }

    /// Members present in any of the keys.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when any key holds something that is not a set.
    pub fn set_union(&self, keys: &[&[u8]]) -> KvResult<Vec<Vec<u8>>> {
        self.combine(keys, SetOp::Union).map(|members| members.into_iter().collect())
    }

    /// Members present in every one of the keys.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when any key holds something that is not a set.
    pub fn set_intersect(&self, keys: &[&[u8]]) -> KvResult<Vec<Vec<u8>>> {
        self.combine(keys, SetOp::Intersect).map(|members| members.into_iter().collect())
    }

    /// Members of the first key that are in none of the rest.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when any key holds something that is not a set.
    pub fn set_difference(&self, keys: &[&[u8]]) -> KvResult<Vec<Vec<u8>>> {
        self.combine(keys, SetOp::Difference).map(|members| members.into_iter().collect())
    }

    /// [`KvKeyspace::set_union`], storing the result at `destination`.
    ///
    /// Reads and writes under one lock, so no other caller can observe a half-done
    /// combination — which is what makes this different from doing it in two steps.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when any source key holds something that is not a set.
    pub fn set_union_store(&self, destination: &[u8], keys: &[&[u8]]) -> KvResult<u64> {
        self.combine_store(destination, keys, SetOp::Union)
    }

    /// [`KvKeyspace::set_intersect`], storing the result at `destination`.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when any source key holds something that is not a set.
    pub fn set_intersect_store(&self, destination: &[u8], keys: &[&[u8]]) -> KvResult<u64> {
        self.combine_store(destination, keys, SetOp::Intersect)
    }

    /// [`KvKeyspace::set_difference`], storing the result at `destination`.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when any source key holds something that is not a set.
    pub fn set_difference_store(&self, destination: &[u8], keys: &[&[u8]]) -> KvResult<u64> {
        self.combine_store(destination, keys, SetOp::Difference)
    }

    fn combine(&self, keys: &[&[u8]], op: SetOp) -> KvResult<HashSet<Vec<u8>>> {
        let now = self.now();
        let mut map = self.guard();
        combine_locked(&mut map, keys, op, now)
    }

    fn combine_store(&self, destination: &[u8], keys: &[&[u8]], op: SetOp) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();

        let result = combine_locked(&mut map, keys, op, now)?;
        let count = result.len() as u64;

        // An empty result deletes the destination, as Redis does — it does not leave
        // an empty set behind.
        if result.is_empty() {
            map.remove(destination);
        } else {
            map.insert(destination.to_vec(), Entry { value: Value::Set(result), expires_at: None });
        }
        Ok(count)
    }

    // ---- sorted sets ------------------------------------------------------

    /// Adds or re-scores members. Returns `(added, changed)`.
    ///
    /// `added` counts members that were not there before; `changed` also counts
    /// members whose score moved, which is what Redis `ZADD ... CH` reports.
    ///
    /// `condition` maps to `NX` and `XX`. `only_greater` and `only_less` map to `GT`
    /// and `LT`: an existing member's score is updated only if the new score moves it
    /// in that direction.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set,
    /// [`KvError::NotAFloat`] when a score is NaN.
    pub fn sorted_set_add(
        &self,
        key: &[u8],
        entries: &[(f64, &[u8])],
        condition: KvWriteCondition,
        only_greater: bool,
        only_less: bool,
    ) -> KvResult<(u64, u64)> {
        if entries.iter().any(|(score, _)| score.is_nan()) {
            return Err(KvError::NotAFloat);
        }

        let now = self.now();
        let mut map = self.guard();

        let entry =
            fresh_or_existing(&mut map, key, now, || Value::SortedSet(SortedSet::default()));
        let sorted = match &mut entry.value {
            Value::SortedSet(sorted) => sorted,
            _ => return Err(KvError::WrongType),
        };

        let (mut added, mut changed) = (0u64, 0u64);
        for (score, member) in entries {
            match sorted.score(member) {
                Some(previous) => {
                    if condition == KvWriteCondition::NotExists
                        || (only_greater && *score <= previous)
                        || (only_less && *score >= previous)
                    {
                        continue;
                    }
                    if previous != *score {
                        sorted.insert(member, *score);
                        changed += 1;
                    }
                }
                None => {
                    // GT and LT never create a member in Redis either — there is no
                    // previous score for the comparison to be against.
                    if condition == KvWriteCondition::Exists {
                        continue;
                    }
                    sorted.insert(member, *score);
                    added += 1;
                    changed += 1;
                }
            }
        }

        // ZADD NX/XX/GT/LT can decline every entry, which must not leave an empty
        // sorted set behind where there was no key before.
        let emptied = sorted.is_empty();
        if emptied {
            map.remove(key);
        }
        Ok((added, changed))
    }

    /// Removes members, returning how many were present.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_remove(&self, key: &[u8], members: &[&[u8]]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(0);
        };
        let sorted = as_sorted_set(entry)?;
        let removed = members.iter().filter(|member| sorted.remove(member).is_some()).count();

        drop_if_empty(&mut map, key);
        Ok(removed as u64)
    }

    /// A member's score.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_score(&self, key: &[u8], member: &[u8]) -> KvResult<Option<f64>> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(None),
            Some(entry) => Ok(as_sorted_set(entry)?.score(member)),
        }
    }

    /// Number of members, or 0 when the key is missing.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_len(&self, key: &[u8]) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(0),
            Some(entry) => Ok(as_sorted_set(entry)?.len() as u64),
        }
    }

    /// A member's zero-based position, counting from the lowest score or the highest.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_rank(
        &self,
        key: &[u8],
        member: &[u8],
        reverse: bool,
    ) -> KvResult<Option<u64>> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(None);
        };
        let sorted = as_sorted_set(entry)?;
        let Some(rank) = sorted.rank(member) else {
            return Ok(None);
        };
        Ok(Some(if reverse { (sorted.len() - 1 - rank) as u64 } else { rank as u64 }))
    }

    /// Members in an inclusive rank range, with Redis's negative-index rules.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_range(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
        reverse: bool,
    ) -> KvResult<Vec<(Vec<u8>, f64)>> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(Vec::new()),
            Some(entry) => Ok(as_sorted_set(entry)?.range_by_rank(start, stop, reverse)),
        }
    }

    /// Members whose score falls between the bounds, in score order.
    ///
    /// `reverse` reverses the result; the bounds are still given low to high, which
    /// is *not* how `ZREVRANGEBYSCORE` takes them on the wire. The server swaps them.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_range_by_score(
        &self,
        key: &[u8],
        min: Bound<f64>,
        max: Bound<f64>,
        reverse: bool,
    ) -> KvResult<Vec<(Vec<u8>, f64)>> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(Vec::new()),
            Some(entry) => Ok(as_sorted_set(entry)?.range_by_score(min, max, reverse)),
        }
    }

    /// How many members fall between the bounds.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_count(&self, key: &[u8], min: Bound<f64>, max: Bound<f64>) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();
        match live(&mut map, key, now) {
            None => Ok(0),
            Some(entry) => Ok(as_sorted_set(entry)?.count_by_score(min, max) as u64),
        }
    }

    /// Adds `delta` to a member's score and returns the result. Creates it at `delta`
    /// if it is not there.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set,
    /// [`KvError::NotAFloat`] when `delta` is NaN,
    /// [`KvError::NaNResult`] when the sum would be NaN, as `+inf` plus `-inf` is.
    pub fn sorted_set_incr_by(&self, key: &[u8], member: &[u8], delta: f64) -> KvResult<f64> {
        if delta.is_nan() {
            return Err(KvError::NotAFloat);
        }

        let now = self.now();
        let mut map = self.guard();

        let entry =
            fresh_or_existing(&mut map, key, now, || Value::SortedSet(SortedSet::default()));
        let sorted = match &mut entry.value {
            Value::SortedSet(sorted) => sorted,
            _ => return Err(KvError::WrongType),
        };

        let next = sorted.score(member).unwrap_or(0.0) + delta;
        if next.is_nan() {
            // Reached by adding opposite infinities. Leave the member as it was, and
            // do not leave behind a sorted set this call brought into existence.
            let emptied = sorted.is_empty();
            if emptied {
                map.remove(key);
            }
            return Err(KvError::NaNResult);
        }

        sorted.insert(member, next);
        Ok(next)
    }

    /// Removes and returns the lowest- or highest-scoring members.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_pop(
        &self,
        key: &[u8],
        count: usize,
        from_max: bool,
    ) -> KvResult<Vec<(Vec<u8>, f64)>> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(Vec::new());
        };
        let popped = as_sorted_set(entry)?.pop(count, from_max);

        drop_if_empty(&mut map, key);
        Ok(popped)
    }

    /// Removes everything in an inclusive rank range. Returns how many went.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_remove_range_by_rank(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(0);
        };
        let removed = as_sorted_set(entry)?.remove_range_by_rank(start, stop);

        drop_if_empty(&mut map, key);
        Ok(removed as u64)
    }

    /// Removes everything in a score range. Returns how many went.
    ///
    /// # Errors
    /// [`KvError::WrongType`] when the key holds something that is not a sorted set.
    pub fn sorted_set_remove_range_by_score(
        &self,
        key: &[u8],
        min: Bound<f64>,
        max: Bound<f64>,
    ) -> KvResult<u64> {
        let now = self.now();
        let mut map = self.guard();

        let Some(entry) = live(&mut map, key, now) else {
            return Ok(0);
        };
        let removed = as_sorted_set(entry)?.remove_range_by_score(min, max);

        drop_if_empty(&mut map, key);
        Ok(removed as u64)
    }
}

/// Which way to combine several sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetOp {
    Union,
    Intersect,
    Difference,
}

/// Combines sets while the caller holds the lock, so a store variant can write the
/// result without ever releasing it.
fn combine_locked(
    map: &mut Entries,
    keys: &[&[u8]],
    op: SetOp,
    now: u64,
) -> KvResult<HashSet<Vec<u8>>> {
    let mut result: Option<HashSet<Vec<u8>>> = None;

    for key in keys {
        // A missing key is the empty set, which is what Redis treats it as.
        let members: HashSet<Vec<u8>> = match live(map, key, now) {
            None => HashSet::new(),
            Some(entry) => as_set(entry)?.clone(),
        };

        result = Some(match result {
            None => members,
            Some(accumulated) => match op {
                SetOp::Union => accumulated.union(&members).cloned().collect(),
                SetOp::Intersect => accumulated.intersection(&members).cloned().collect(),
                SetOp::Difference => accumulated.difference(&members).cloned().collect(),
            },
        });

        // Nothing later can put members back into an intersection or a difference.
        if op != SetOp::Union && result.as_ref().is_some_and(HashSet::is_empty) {
            break;
        }
    }

    Ok(result.unwrap_or_default())
}

// ---- free helpers ---------------------------------------------------------

fn is_expired(entry: &Entry, now: u64) -> bool {
    entry.expires_at.is_some_and(|at| at <= now)
}

/// Returns the entry at `key` if it is live, dropping it first if it has expired.
///
/// Every read path goes through here, which is what makes lazy expiry correct: an
/// expired key is never observable, whether or not a sweep has reached it.
fn live<'a>(map: &'a mut Entries, key: &[u8], now: u64) -> Option<&'a mut Entry> {
    if map.get(key).is_some_and(|entry| is_expired(entry, now)) {
        map.remove(key);
        return None;
    }
    map.get_mut(key)
}

/// The entry at `key`, replacing its value with a fresh one first if it has expired.
///
/// This is what makes "push to an expired list starts a new list" true everywhere
/// rather than in whichever methods remembered to check. `fresh` is called at most
/// once, but must be `Fn` because the compiler cannot see which branch runs.
fn fresh_or_existing<'a>(
    map: &'a mut Entries,
    key: &[u8],
    now: u64,
    fresh: impl Fn() -> Value,
) -> &'a mut Entry {
    map.entry(key.to_vec())
        .and_modify(|entry| {
            if is_expired(entry, now) {
                entry.value = fresh();
                entry.expires_at = None;
            }
        })
        .or_insert_with(|| Entry { value: fresh(), expires_at: None })
}

/// Redis does not keep a list, hash, set or sorted set that has lost its last element.
fn drop_if_empty(map: &mut Entries, key: &[u8]) {
    if map.get(key).is_some_and(|entry| entry.value.is_empty_aggregate()) {
        map.remove(key);
    }
}

fn deadline(now: u64, time_to_live: Duration) -> u64 {
    now.saturating_add(u64::try_from(time_to_live.as_millis()).unwrap_or(u64::MAX))
}

fn as_string(entry: &mut Entry) -> KvResult<&mut Vec<u8>> {
    match &mut entry.value {
        Value::String(text) => Ok(text),
        _ => Err(KvError::WrongType),
    }
}

fn as_list(entry: &mut Entry) -> KvResult<&mut VecDeque<Vec<u8>>> {
    match &mut entry.value {
        Value::List(items) => Ok(items),
        _ => Err(KvError::WrongType),
    }
}

fn as_hash(entry: &mut Entry) -> KvResult<&mut HashMap<Vec<u8>, Vec<u8>>> {
    match &mut entry.value {
        Value::Hash(fields) => Ok(fields),
        _ => Err(KvError::WrongType),
    }
}

fn as_set(entry: &mut Entry) -> KvResult<&mut HashSet<Vec<u8>>> {
    match &mut entry.value {
        Value::Set(members) => Ok(members),
        _ => Err(KvError::WrongType),
    }
}

fn as_sorted_set(entry: &mut Entry) -> KvResult<&mut SortedSet> {
    match &mut entry.value {
        Value::SortedSet(members) => Ok(members),
        _ => Err(KvError::WrongType),
    }
}

/// Resolves a possibly-negative index, or `None` when it falls outside the list.
fn normalize(index: i64, len: usize) -> Option<usize> {
    let len = i64::try_from(len).ok()?;
    let resolved = if index < 0 { len.checked_add(index)? } else { index };
    if resolved < 0 || resolved >= len { None } else { usize::try_from(resolved).ok() }
}

/// Resolves an inclusive range with Redis's clamping, or `None` when it is empty.
pub(crate) fn range_bounds(start: i64, stop: i64, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let len = i64::try_from(len).ok()?;

    let from = if start < 0 { (len + start).max(0) } else { start };
    let to = if stop < 0 { len + stop } else { stop.min(len - 1) };

    if from > to || from >= len || to < 0 {
        return None;
    }
    Some((usize::try_from(from).ok()?, usize::try_from(to).ok()?))
}

// ---- the abstraction ------------------------------------------------------

impl kvlite_api::Keyspace for KvKeyspace {
    fn index(&self) -> usize {
        Self::index(self)
    }
    fn len(&self) -> u64 {
        Self::len(self)
    }
    fn contains(&self, key: &[u8]) -> bool {
        Self::contains(self, key)
    }
    fn remove(&self, key: &[u8]) -> bool {
        Self::remove(self, key)
    }
    fn expire(&self, key: &[u8], time_to_live: Option<Duration>) -> bool {
        Self::expire(self, key, time_to_live)
    }
    fn time_to_live(&self, key: &[u8]) -> Option<Duration> {
        Self::time_to_live(self, key)
    }
    fn kind(&self, key: &[u8]) -> KvValueType {
        Self::kind(self, key)
    }
    fn clear(&self) {
        Self::clear(self);
    }
    fn set(
        &self,
        key: &[u8],
        value: &[u8],
        time_to_live: Option<Duration>,
        condition: KvWriteCondition,
        keep_time_to_live: bool,
    ) -> bool {
        Self::set(self, key, value, time_to_live, condition, keep_time_to_live)
    }
    fn get(&self, key: &[u8]) -> KvResult<Option<Vec<u8>>> {
        Self::get(self, key)
    }
    fn incr_by(&self, key: &[u8], delta: i64) -> KvResult<i64> {
        Self::incr_by(self, key, delta)
    }
    fn strlen(&self, key: &[u8]) -> KvResult<u64> {
        Self::strlen(self, key)
    }
}

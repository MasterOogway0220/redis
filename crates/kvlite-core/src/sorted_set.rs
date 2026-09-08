use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;

use crate::score::Score;

/// A set of unique members, each with a score, ordered by score and then by member.
///
/// Two structures over the same data, which is what Redis does too:
///
/// - `scores` answers "what is this member's score" in O(1).
/// - `index` answers "what is in this score range, in order" in O(log n + k).
///
/// The invariant is that they always hold the same members, and that `index` holds
/// exactly `(Score(scores[member]), member)` for each. Every mutation goes through
/// [`SortedSet::insert`] or [`SortedSet::remove`] so there is one place to get that
/// right.
#[derive(Debug, Default, Clone)]
pub(crate) struct SortedSet {
    scores: HashMap<Vec<u8>, f64>,
    index: BTreeSet<(Score, Vec<u8>)>,
}

impl SortedSet {
    pub(crate) fn len(&self) -> usize {
        self.scores.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }

    pub(crate) fn score(&self, member: &[u8]) -> Option<f64> {
        self.scores.get(member).copied()
    }

    /// Sets a member's score, returning the score it had before.
    ///
    /// # Panics
    /// In debug builds, if `score` is NaN. Callers reject that at the boundary.
    pub(crate) fn insert(&mut self, member: &[u8], score: f64) -> Option<f64> {
        let score = Score::new(score);

        if let Some(previous) = self.scores.insert(member.to_vec(), score.get()) {
            // The index is keyed by score, so a re-score is a move, not an update.
            self.index.remove(&(Score::new(previous), member.to_vec()));
            self.index.insert((score, member.to_vec()));
            return Some(previous);
        }
        self.index.insert((score, member.to_vec()));
        None
    }

    pub(crate) fn remove(&mut self, member: &[u8]) -> Option<f64> {
        let score = self.scores.remove(member)?;
        self.index.remove(&(Score::new(score), member.to_vec()));
        Some(score)
    }

    /// The member's zero-based position in score order.
    pub(crate) fn rank(&self, member: &[u8]) -> Option<usize> {
        let score = self.score(member)?;
        // ponytail: O(n). Redis uses a skiplist carrying span counts, which makes
        // this O(log n). Swap in an order-statistic tree when ranking large sorted
        // sets shows up in a benchmark; nothing else here depends on the shape.
        Some(self.index.range(..(Score::new(score), member.to_vec())).count())
    }

    /// An inclusive rank range, with Redis's negative-index and clamping rules.
    pub(crate) fn range_by_rank(
        &self,
        start: i64,
        stop: i64,
        reverse: bool,
    ) -> Vec<(Vec<u8>, f64)> {
        let Some((from, to)) = crate::keyspace::range_bounds(start, stop, self.len()) else {
            return Vec::new();
        };
        let take = to - from + 1;

        if reverse {
            self.index
                .iter()
                .rev()
                .skip(from)
                .take(take)
                .map(|(score, member)| (member.clone(), score.get()))
                .collect()
        } else {
            self.index
                .iter()
                .skip(from)
                .take(take)
                .map(|(score, member)| (member.clone(), score.get()))
                .collect()
        }
    }

    /// Members whose score falls between the bounds, in score order.
    ///
    /// Seeks straight to the lower bound rather than scanning from the start, so the
    /// cost is the size of the answer, not the size of the set.
    pub(crate) fn range_by_score(
        &self,
        min: Bound<f64>,
        max: Bound<f64>,
        reverse: bool,
    ) -> Vec<(Vec<u8>, f64)> {
        // The index is keyed by (score, member), so seeking by score alone means
        // starting at the smallest possible member for that score and letting the
        // predicate discard the boundary cases. That is at most the number of
        // members sharing one score, not a scan.
        let start = match min {
            Bound::Unbounded => Bound::Unbounded,
            Bound::Included(value) | Bound::Excluded(value) => {
                Bound::Included((Score::new(value), Vec::new()))
            }
        };

        let mut found: Vec<(Vec<u8>, f64)> = self
            .index
            .range((start, Bound::Unbounded))
            .take_while(|(score, _)| within_max(score.get(), max))
            .filter(|(score, _)| within_min(score.get(), min))
            .map(|(score, member)| (member.clone(), score.get()))
            .collect();

        if reverse {
            found.reverse();
        }
        found
    }

    pub(crate) fn count_by_score(&self, min: Bound<f64>, max: Bound<f64>) -> usize {
        self.range_by_score(min, max, false).len()
    }

    /// Removes and returns the lowest- or highest-scoring members.
    pub(crate) fn pop(&mut self, count: usize, from_max: bool) -> Vec<(Vec<u8>, f64)> {
        let doomed: Vec<(Vec<u8>, f64)> = if from_max {
            self.index.iter().rev().take(count).map(|(s, m)| (m.clone(), s.get())).collect()
        } else {
            self.index.iter().take(count).map(|(s, m)| (m.clone(), s.get())).collect()
        };

        for (member, _) in &doomed {
            self.remove(member);
        }
        doomed
    }

    /// Removes everything the given rank range covers. Returns how many went.
    pub(crate) fn remove_range_by_rank(&mut self, start: i64, stop: i64) -> usize {
        let doomed = self.range_by_rank(start, stop, false);
        for (member, _) in &doomed {
            self.remove(member);
        }
        doomed.len()
    }

    /// Removes everything the given score range covers. Returns how many went.
    pub(crate) fn remove_range_by_score(&mut self, min: Bound<f64>, max: Bound<f64>) -> usize {
        let doomed = self.range_by_score(min, max, false);
        for (member, _) in &doomed {
            self.remove(member);
        }
        doomed.len()
    }
}

fn within_min(score: f64, min: Bound<f64>) -> bool {
    match min {
        Bound::Unbounded => true,
        Bound::Included(bound) => score >= bound,
        Bound::Excluded(bound) => score > bound,
    }
}

fn within_max(score: f64, max: Bound<f64>) -> bool {
    match max {
        Bound::Unbounded => true,
        Bound::Included(bound) => score <= bound,
        Bound::Excluded(bound) => score < bound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SortedSet {
        let mut set = SortedSet::default();
        for (member, score) in [(&b"a"[..], 1.0), (b"b", 2.0), (b"c", 2.0), (b"d", 3.0)] {
            set.insert(member, score);
        }
        set
    }

    /// Members as text, so a failing assertion reads as names rather than bytes.
    fn members(entries: &[(Vec<u8>, f64)]) -> Vec<String> {
        entries.iter().map(|(member, _)| String::from_utf8_lossy(member).into_owned()).collect()
    }

    #[test]
    fn ties_break_on_member_order() {
        let set = sample();
        assert_eq!(members(&set.range_by_rank(0, -1, false)), ["a", "b", "c", "d"]);
    }

    #[test]
    fn rescoring_moves_a_member_rather_than_duplicating_it() {
        let mut set = sample();
        assert_eq!(set.insert(b"a", 10.0), Some(1.0));

        assert_eq!(set.len(), 4, "the index must not have gained an entry");
        assert_eq!(set.score(b"a"), Some(10.0));
        assert_eq!(members(&set.range_by_rank(0, -1, false)), ["b", "c", "d", "a"]);
    }

    #[test]
    fn reports_rank_from_both_ends() {
        let set = sample();
        assert_eq!(set.rank(b"a"), Some(0));
        assert_eq!(set.rank(b"c"), Some(2));
        assert_eq!(set.rank(b"missing"), None);
    }

    #[test]
    fn score_ranges_honour_exclusive_bounds() {
        let set = sample();
        let range = |min, max| members(&set.range_by_score(min, max, false));

        assert_eq!(range(Bound::Unbounded, Bound::Unbounded), ["a", "b", "c", "d"]);
        assert_eq!(range(Bound::Included(2.0), Bound::Included(2.0)), ["b", "c"]);
        assert_eq!(
            range(Bound::Excluded(2.0), Bound::Unbounded),
            ["d"],
            "an exclusive lower bound must skip every member sharing that score"
        );
        assert_eq!(range(Bound::Unbounded, Bound::Excluded(2.0)), ["a"]);
        assert!(range(Bound::Excluded(3.0), Bound::Unbounded).is_empty());
    }

    #[test]
    fn infinite_bounds_cover_everything() {
        let set = sample();
        let all = set.range_by_score(
            Bound::Included(f64::NEG_INFINITY),
            Bound::Included(f64::INFINITY),
            false,
        );
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn popping_takes_from_the_end_it_was_asked_for() {
        let mut set = sample();
        assert_eq!(set.pop(1, false), vec![(b"a".to_vec(), 1.0)]);
        assert_eq!(set.pop(1, true), vec![(b"d".to_vec(), 3.0)]);
        assert_eq!(set.len(), 2);

        // Asking for more than there is takes what there is.
        assert_eq!(set.pop(10, false).len(), 2);
        assert!(set.is_empty());
    }

    #[test]
    fn removing_keeps_both_structures_in_step() {
        let mut set = sample();
        assert_eq!(set.remove(b"b"), Some(2.0));
        assert_eq!(set.remove(b"b"), None);

        assert_eq!(set.len(), 3);
        assert_eq!(set.index.len(), 3, "the index must not keep a ghost entry");
        assert_eq!(set.rank(b"c"), Some(1));
    }

    #[test]
    fn negative_zero_cannot_hide_from_a_lookup_for_zero() {
        let mut set = SortedSet::default();
        set.insert(b"m", -0.0);

        assert_eq!(set.rank(b"m"), Some(0));
        assert_eq!(
            set.range_by_score(Bound::Included(0.0), Bound::Included(0.0), false).len(),
            1,
            "-0.0 must be found by a range over 0.0"
        );
    }
}

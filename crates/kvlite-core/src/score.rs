use std::cmp::Ordering;

/// Parses a sorted-set score exactly as Redis does.
///
/// Accepts a decimal number, or `inf` / `+inf` / `infinity` / `-inf` in any case.
/// Rejects NaN, an empty string, and surrounding whitespace — a score has to have a
/// place in a total order, and NaN does not.
///
/// ```
/// use kvlite_core::parse_score;
///
/// assert_eq!(parse_score(b"1.5"), Some(1.5));
/// assert_eq!(parse_score(b"-inf"), Some(f64::NEG_INFINITY));
/// assert_eq!(parse_score(b"nan"), None);
/// assert_eq!(parse_score(b" 1"), None);
///
/// // Negative zero is normalised away, so it can never sort below positive zero.
/// assert_eq!(parse_score(b"-0").map(f64::is_sign_negative), Some(false));
/// ```
#[must_use]
pub fn parse_score(bytes: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(bytes).ok()?;

    // Rust's parser accepts leading and trailing whitespace nowhere, which matches
    // Redis, but it does accept "NaN" — which we must not.
    let value: f64 = text.parse().ok()?;
    if value.is_nan() {
        return None;
    }
    Some(normalize(value))
}

/// Formats a score the way Redis prints one: no trailing zeros, `inf` for infinity.
///
/// ```
/// use kvlite_core::format_score;
///
/// assert_eq!(format_score(1.0), b"1".to_vec());
/// assert_eq!(format_score(1.5), b"1.5".to_vec());
/// assert_eq!(format_score(f64::NEG_INFINITY), b"-inf".to_vec());
/// ```
#[must_use]
pub fn format_score(value: f64) -> Vec<u8> {
    if value.is_infinite() {
        return if value.is_sign_negative() { b"-inf".to_vec() } else { b"inf".to_vec() };
    }
    // Rust's Display for f64 prints the shortest string that round-trips, which is
    // what we want and is a slightly better guarantee than Redis's "%.17g". Very
    // large magnitudes are spelled out in full here where Redis would use
    // exponential notation; both re-parse to the same double.
    format!("{value}").into_bytes()
}

/// Maps `-0.0` to `0.0`.
///
/// Redis compares scores with `<` and `==`, under which `-0.0 == 0.0`. Our index is
/// a `BTreeSet` ordered by `total_cmp`, under which `-0.0 < 0.0` — so without this,
/// storing `-0` would put a member in a position no lookup for `0` would find.
fn normalize(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

/// A score with a total order, for use as part of a `BTreeSet` key.
///
/// Only constructible through [`Score::new`], which normalises, and never from NaN —
/// [`parse_score`] and the engine reject that before it gets here. Given those two
/// invariants, `total_cmp` agrees exactly with `<` on the values we actually store.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Score(f64);

impl Score {
    pub(crate) fn new(value: f64) -> Self {
        debug_assert!(!value.is_nan(), "NaN must be rejected before it reaches the index");
        Self(normalize(value))
    }

    pub(crate) fn get(self) -> f64 {
        self.0
    }
}

impl Eq for Score {}

impl Ord for Score {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl PartialOrd for Score {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_redis_parses() {
        assert_eq!(parse_score(b"0"), Some(0.0));
        assert_eq!(parse_score(b"-1.25"), Some(-1.25));
        assert_eq!(parse_score(b"1e3"), Some(1000.0));
        assert_eq!(parse_score(b"inf"), Some(f64::INFINITY));
        assert_eq!(parse_score(b"+inf"), Some(f64::INFINITY));
        assert_eq!(parse_score(b"INFINITY"), Some(f64::INFINITY));
        assert_eq!(parse_score(b"-inf"), Some(f64::NEG_INFINITY));
    }

    #[test]
    fn rejects_what_has_no_place_in_an_order() {
        for input in [&b""[..], b"nan", b"NaN", b"-nan", b" 1", b"1 ", b"abc", b"1.2.3"] {
            assert_eq!(parse_score(input), None, "should have rejected {input:?}");
        }
    }

    #[test]
    fn scores_order_the_way_redis_compares_them() {
        let ordered = [f64::NEG_INFINITY, -1.5, -0.0, 0.0, 1.5, f64::INFINITY];
        for pair in ordered.windows(2) {
            let (low, high) = (Score::new(pair[0]), Score::new(pair[1]));
            assert!(low <= high, "{:?} should not sort after {:?}", pair[0], pair[1]);
        }
        // The one that would go wrong without normalisation.
        assert_eq!(Score::new(-0.0), Score::new(0.0));
    }

    #[test]
    fn formatting_round_trips_through_parsing() {
        for value in [0.0, 1.0, -1.0, 1.5, 0.1, 1e18, -2.5e-8, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(parse_score(&format_score(value)), Some(value), "failed for {value}");
        }
    }
}

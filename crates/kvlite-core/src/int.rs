/// Parses a 64-bit integer exactly as Redis does.
///
/// Redis is stricter than [`str::parse`], and the difference is observable: it
/// rejects a leading `+`, rejects leading zeros, rejects surrounding whitespace, and
/// rejects an empty string. `INCR` on a key holding `"042"` is an error in Redis and
/// must be an error here, or a client that relies on that behaviour breaks quietly.
///
/// ```
/// assert_eq!(kvlite_core::parse_int(b"42"), Some(42));
/// assert_eq!(kvlite_core::parse_int(b"-9223372036854775808"), Some(i64::MIN));
/// assert_eq!(kvlite_core::parse_int(b"042"), None);
/// assert_eq!(kvlite_core::parse_int(b"+42"), None);
/// assert_eq!(kvlite_core::parse_int(b" 42"), None);
/// ```
#[must_use]
pub fn parse_int(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes.first()? {
        b'-' => (true, &bytes[1..]),
        _ => (false, bytes),
    };

    match digits {
        [] => return None,
        // "0" is the only value allowed to start with a zero, and "-0" is not a value.
        [b'0'] => return if negative { None } else { Some(0) },
        [b'0', ..] => return None,
        _ => {}
    }

    // Accumulate negatively so i64::MIN needs no special case.
    let mut accumulator: i64 = 0;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        accumulator = accumulator.checked_mul(10)?;
        accumulator = accumulator.checked_sub(i64::from(byte - b'0'))?;
    }

    if negative { Some(accumulator) } else { accumulator.checked_neg() }
}

/// Formats a 64-bit integer as the bytes Redis would store.
#[must_use]
pub fn format_int(value: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    if value == 0 {
        out.push(b'0');
        return out;
    }

    let negative = value < 0;
    let mut remaining = if negative { value } else { -value };

    let mut digits = [0u8; 20];
    let mut index = digits.len();
    while remaining != 0 {
        index -= 1;
        digits[index] = b'0' + (-(remaining % 10)) as u8;
        remaining /= 10;
    }

    if negative {
        out.push(b'-');
    }
    out.extend_from_slice(&digits[index..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_boundary() {
        for value in [0, 1, -1, 9, -9, 10, -10, i64::MAX, i64::MIN, 1234567890] {
            assert_eq!(parse_int(&format_int(value)), Some(value), "failed for {value}");
        }
    }

    #[test]
    fn rejects_what_redis_rejects() {
        for input in [
            &b""[..],
            b"-",
            b"+1",
            b"042",
            b"-042",
            b"-0",
            b" 1",
            b"1 ",
            b"1.0",
            b"abc",
            b"9223372036854775808",  // i64::MAX + 1
            b"-9223372036854775809", // i64::MIN - 1
        ] {
            assert_eq!(parse_int(input), None, "should have rejected {input:?}");
        }
    }
}

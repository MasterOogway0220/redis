//! Redis glob matching, used by `KEYS` and by pattern subscriptions.
//!
//! Supports `*`, `?`, character classes with ranges and negation, and `\` escapes —
//! the same grammar as Redis `stringmatchlen`.

/// Whether `subject` matches `pattern`.
pub(crate) fn matches(pattern: &[u8], subject: &[u8]) -> bool {
    let (mut p, mut s) = (0, 0);
    // Position to backtrack to when a `*` needs to swallow one more byte.
    let mut star: Option<(usize, usize)> = None;

    while s < subject.len() {
        // Each arm either consumes and `continue`s, or falls through to backtracking.
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, s));
                p += 1;
                continue;
            }
            Some(b'?') => {
                p += 1;
                s += 1;
                continue;
            }
            // A non-matching class, and an unterminated one, both fall through.
            Some(b'[') => {
                if let Some((true, next)) = class(pattern, p, subject[s]) {
                    p = next;
                    s += 1;
                    continue;
                }
            }
            Some(b'\\') if p + 1 < pattern.len() => {
                if pattern[p + 1] == subject[s] {
                    p += 2;
                    s += 1;
                    continue;
                }
            }
            Some(&literal) if literal == subject[s] => {
                p += 1;
                s += 1;
                continue;
            }
            _ => {}
        }

        match star {
            Some((star_p, star_s)) => {
                // Give the star one more byte and try again from just after it.
                star = Some((star_p, star_s + 1));
                s = star_s + 1;
                p = star_p + 1;
            }
            None => return false,
        }
    }

    // Trailing stars may match nothing at all.
    while pattern.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pattern.len()
}

/// Matches one character class starting at `[`, returning whether it matched and
/// the index just past the closing `]`. `None` when the class is unterminated.
fn class(pattern: &[u8], start: usize, byte: u8) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negated = pattern.get(i) == Some(&b'^');
    if negated {
        i += 1;
    }

    let mut matched = false;
    while let Some(&current) = pattern.get(i) {
        match current {
            b']' => return Some((matched != negated, i + 1)),
            b'\\' if i + 1 < pattern.len() => {
                if pattern[i + 1] == byte {
                    matched = true;
                }
                i += 2;
            }
            _ if pattern.get(i + 1) == Some(&b'-')
                && pattern.get(i + 2).is_some_and(|&end| end != b']') =>
            {
                let end = pattern[i + 2];
                let (low, high) = (current.min(end), current.max(end));
                if (low..=high).contains(&byte) {
                    matched = true;
                }
                i += 3;
            }
            _ => {
                if current == byte {
                    matched = true;
                }
                i += 1;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn literals_and_wildcards() {
        assert!(matches(b"", b""));
        assert!(matches(b"abc", b"abc"));
        assert!(!matches(b"abc", b"abd"));
        assert!(matches(b"*", b""));
        assert!(matches(b"*", b"anything"));
        assert!(matches(b"a*", b"abc"));
        assert!(matches(b"*c", b"abc"));
        assert!(matches(b"a*c", b"abbbc"));
        assert!(!matches(b"a*c", b"abbbd"));
        assert!(matches(b"a**c", b"ac"));
        assert!(matches(b"?", b"a"));
        assert!(!matches(b"?", b"ab"));
        assert!(matches(b"h?llo", b"hello"));
    }

    #[test]
    fn character_classes() {
        assert!(matches(b"h[ae]llo", b"hello"));
        assert!(matches(b"h[ae]llo", b"hallo"));
        assert!(!matches(b"h[ae]llo", b"hillo"));
        assert!(matches(b"h[a-z]llo", b"hqllo"));
        assert!(!matches(b"h[a-z]llo", b"hQllo"));
        assert!(matches(b"h[^e]llo", b"hallo"));
        assert!(!matches(b"h[^e]llo", b"hello"));
        // An unterminated class matches nothing rather than panicking.
        assert!(!matches(b"h[allo", b"hallo"));
    }

    #[test]
    fn escapes_are_literal() {
        assert!(matches(br"a\*c", b"a*c"));
        assert!(!matches(br"a\*c", b"abc"));
        assert!(matches(br"a\?c", b"a?c"));
    }

    #[test]
    fn backtracking_terminates_on_adversarial_patterns() {
        // The classic catastrophic-backtracking shape. The two-pointer walk is
        // linear in the subject, so this returns rather than hanging.
        assert!(!matches(b"a*a*a*a*a*a*b", &[b'a'; 64]));
    }

    #[test]
    fn matching_is_byte_wise_not_utf8() {
        assert!(matches(b"*", &[0xff, 0x00, 0xfe]));
        assert!(matches(b"?", &[0xff]));
    }
}

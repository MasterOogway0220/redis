use alloc::vec::Vec;

use crate::{DecodeError, Frame, Limits};

/// One decoded command and the number of bytes it consumed.
///
/// `Ok(None)` means the buffer holds a valid but incomplete command: read more and
/// call again with the same bytes plus whatever arrived.
pub type DecodedCommand = Result<Option<(Vec<Vec<u8>>, usize)>, DecodeError>;

/// Incremental RESP2/RESP3 decoder.
///
/// The decoder is stateless and cheap to clone: you hand it the bytes you have so
/// far, and it either produces a frame and the number of bytes it consumed, or
/// `Ok(None)` meaning "valid so far, send more". It never holds a borrow of your
/// buffer past the call, so you are free to compact or grow it between calls.
#[derive(Debug, Clone, Default)]
pub struct Decoder {
    limits: Limits,
}

impl Decoder {
    /// A decoder with the default [`Limits`].
    pub fn new() -> Self {
        Self::default()
    }

    /// A decoder with custom limits.
    pub fn with_limits(limits: Limits) -> Self {
        Self { limits }
    }

    /// The limits this decoder enforces.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Decodes one frame.
    ///
    /// Returns `Ok(None)` when `src` holds a valid but incomplete frame.
    ///
    /// # Errors
    /// [`DecodeError`] when the bytes are not valid RESP, or exceed a limit.
    pub fn decode(&self, src: &[u8]) -> Result<Option<(Frame, usize)>, DecodeError> {
        let mut pos = 0;
        match self.parse(src, &mut pos, 0)? {
            Some(frame) => Ok(Some((frame, pos))),
            None => Ok(None),
        }
    }

    /// Decodes one client command: an array of bulk strings, or an inline command.
    ///
    /// Inline commands are split on ASCII whitespace and do not support quoting.
    /// Real clients send the array form; inline exists for `telnet` and for health
    /// checks that write `PING\r\n` at a socket.
    ///
    /// An empty inline line yields an empty argument list, which callers should
    /// ignore rather than treat as an error — that is what Redis does.
    ///
    /// # Errors
    /// [`DecodeError`] when the bytes are not valid RESP, exceed a limit, or are a
    /// well-formed frame that is not a command.
    pub fn decode_command(&self, src: &[u8]) -> DecodedCommand {
        match src.first() {
            None => Ok(None),
            Some(b'*') => {
                let Some((frame, consumed)) = self.decode(src)? else {
                    return Ok(None);
                };
                let Frame::Array(items) = frame else {
                    // `*-1` decodes to Null. A null is not a command.
                    return Err(DecodeError::NotACommand);
                };
                let mut args = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Frame::Bulk(bytes) => args.push(bytes),
                        _ => return Err(DecodeError::NotACommand),
                    }
                }
                Ok(Some((args, consumed)))
            }
            Some(_) => self.decode_inline(src),
        }
    }

    fn decode_inline(&self, src: &[u8]) -> DecodedCommand {
        let Some(newline) = src.iter().position(|&b| b == b'\n') else {
            // Not a complete line yet. Refuse to buffer an unbounded one.
            if src.len() > self.limits.max_line_len {
                return Err(DecodeError::LineTooLong);
            }
            return Ok(None);
        };
        if newline > self.limits.max_line_len {
            return Err(DecodeError::LineTooLong);
        }

        let mut line = &src[..newline];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }

        let args = line
            .split(|b: &u8| b.is_ascii_whitespace())
            .filter(|part| !part.is_empty())
            .map(<[u8]>::to_vec)
            .collect();

        Ok(Some((args, newline + 1)))
    }

    fn parse(
        &self,
        src: &[u8],
        pos: &mut usize,
        depth: usize,
    ) -> Result<Option<Frame>, DecodeError> {
        if depth > self.limits.max_depth {
            return Err(DecodeError::DepthExceeded);
        }
        let Some(&tag) = src.get(*pos) else {
            return Ok(None);
        };

        // Work on a local cursor and only commit it once the whole frame is present,
        // so a partial frame leaves the caller's position exactly where it was.
        let mut p = *pos + 1;

        let frame = match tag {
            b'+' | b'-' => {
                let Some(line) = self.read_line(src, &mut p)? else {
                    return Ok(None);
                };
                // A lone CR or LF cannot appear in a line-terminated frame. Rejecting
                // it here is what lets the encoder promise a lossless round trip.
                if line.iter().any(|&byte| byte == b'\r' || byte == b'\n') {
                    return Err(DecodeError::StrayLineBreak);
                }
                if tag == b'+' { Frame::Simple(line.to_vec()) } else { Frame::Error(line.to_vec()) }
            }
            b':' => {
                let Some(line) = self.read_line(src, &mut p)? else {
                    return Ok(None);
                };
                Frame::Integer(parse_i64(line).ok_or(DecodeError::InvalidInteger)?)
            }
            b'_' => {
                let Some(line) = self.read_line(src, &mut p)? else {
                    return Ok(None);
                };
                if !line.is_empty() {
                    return Err(DecodeError::InvalidLength);
                }
                Frame::Null
            }
            b'#' => {
                let Some(line) = self.read_line(src, &mut p)? else {
                    return Ok(None);
                };
                match line {
                    b"t" => Frame::Boolean(true),
                    b"f" => Frame::Boolean(false),
                    _ => return Err(DecodeError::InvalidBoolean),
                }
            }
            b',' => {
                let Some(line) = self.read_line(src, &mut p)? else {
                    return Ok(None);
                };
                let text = core::str::from_utf8(line).map_err(|_| DecodeError::InvalidDouble)?;
                Frame::Double(text.parse().map_err(|_| DecodeError::InvalidDouble)?)
            }
            b'(' => {
                let Some(line) = self.read_line(src, &mut p)? else {
                    return Ok(None);
                };
                if !is_decimal_integer(line) {
                    return Err(DecodeError::InvalidInteger);
                }
                Frame::BigNumber(line.to_vec())
            }
            b'$' | b'!' | b'=' => {
                let Some(frame) = self.parse_bulk(src, &mut p, tag)? else {
                    return Ok(None);
                };
                frame
            }
            b'*' | b'~' | b'>' | b'%' => {
                let Some(frame) = self.parse_aggregate(src, &mut p, tag, depth)? else {
                    return Ok(None);
                };
                frame
            }
            other => return Err(DecodeError::UnexpectedByte(other)),
        };

        *pos = p;
        Ok(Some(frame))
    }

    fn parse_bulk(&self, src: &[u8], p: &mut usize, tag: u8) -> Result<Option<Frame>, DecodeError> {
        let Some(len) = self.read_count(src, p)? else {
            return Ok(None);
        };

        if len == -1 && tag == b'$' {
            // RESP2 null bulk string.
            return Ok(Some(Frame::Null));
        }
        if len < 0 {
            return Err(DecodeError::InvalidLength);
        }
        let len = len as usize;
        if len > self.limits.max_bulk_len {
            return Err(DecodeError::BulkTooLong);
        }

        let end = p.checked_add(len).ok_or(DecodeError::InvalidLength)?;
        let terminator = end.checked_add(2).ok_or(DecodeError::InvalidLength)?;
        if terminator > src.len() {
            return Ok(None);
        }
        if &src[end..terminator] != b"\r\n" {
            return Err(DecodeError::MissingTerminator);
        }

        let data = &src[*p..end];
        *p = terminator;

        Ok(Some(match tag {
            b'$' => Frame::Bulk(data.to_vec()),
            b'!' => Frame::Error(data.to_vec()),
            _ => {
                // `=` verbatim: three format bytes, a colon, then the payload.
                if data.len() < 4 || data[3] != b':' {
                    return Err(DecodeError::InvalidVerbatim);
                }
                Frame::Verbatim { format: [data[0], data[1], data[2]], data: data[4..].to_vec() }
            }
        }))
    }

    fn parse_aggregate(
        &self,
        src: &[u8],
        p: &mut usize,
        tag: u8,
        depth: usize,
    ) -> Result<Option<Frame>, DecodeError> {
        let Some(count) = self.read_count(src, p)? else {
            return Ok(None);
        };

        if count == -1 && tag == b'*' {
            // RESP2 null array.
            return Ok(Some(Frame::Null));
        }
        if count < 0 {
            return Err(DecodeError::InvalidLength);
        }
        let count = count as usize;
        if count > self.limits.max_array_len {
            return Err(DecodeError::ArrayTooLong);
        }

        let elements = if tag == b'%' {
            count.checked_mul(2).ok_or(DecodeError::ArrayTooLong)?
        } else {
            count
        };

        // Cap the eager allocation. A peer may legally announce a million elements
        // before sending any of them, and we should not reserve that on their say-so.
        let mut items = Vec::with_capacity(elements.min(1024));
        for _ in 0..elements {
            let Some(frame) = self.parse(src, p, depth + 1)? else {
                return Ok(None);
            };
            items.push(frame);
        }

        Ok(Some(match tag {
            b'*' => Frame::Array(items),
            b'~' => Frame::Set(items),
            b'>' => Frame::Push(items),
            _ => {
                let mut pairs = Vec::with_capacity(count);
                let mut drain = items.into_iter();
                while let (Some(k), Some(v)) = (drain.next(), drain.next()) {
                    pairs.push((k, v));
                }
                Frame::Map(pairs)
            }
        }))
    }

    fn read_line<'a>(&self, src: &'a [u8], p: &mut usize) -> Result<Option<&'a [u8]>, DecodeError> {
        let start = *p;
        let hay = src.get(start..).unwrap_or_default();

        match hay.windows(2).position(|w| w == b"\r\n") {
            Some(i) => {
                if i > self.limits.max_line_len {
                    return Err(DecodeError::LineTooLong);
                }
                *p = start + i + 2;
                Ok(Some(&hay[..i]))
            }
            None => {
                if hay.len() > self.limits.max_line_len {
                    return Err(DecodeError::LineTooLong);
                }
                Ok(None)
            }
        }
    }

    fn read_count(&self, src: &[u8], p: &mut usize) -> Result<Option<i64>, DecodeError> {
        let Some(line) = self.read_line(src, p)? else {
            return Ok(None);
        };
        if line == b"?" {
            return Err(DecodeError::StreamedNotSupported);
        }
        parse_i64(line).map(Some).ok_or(DecodeError::InvalidLength)
    }
}

/// Parses a decimal `i64`, accumulating negatively so `i64::MIN` is reachable.
fn parse_i64(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes.first()? {
        b'-' => (true, &bytes[1..]),
        b'+' => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return None;
    }

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

fn is_decimal_integer(bytes: &[u8]) -> bool {
    let digits = match bytes.first() {
        Some(b'-' | b'+') => &bytes[1..],
        Some(_) => bytes,
        None => return false,
    };
    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn decode(src: &[u8]) -> Option<(Frame, usize)> {
        Decoder::new().decode(src).expect("valid RESP")
    }

    #[test]
    fn decodes_the_scalar_types() {
        assert_eq!(decode(b"+OK\r\n"), Some((Frame::Simple(b"OK".to_vec()), 5)));
        assert_eq!(decode(b"-ERR nope\r\n"), Some((Frame::Error(b"ERR nope".to_vec()), 11)));
        assert_eq!(decode(b":42\r\n"), Some((Frame::Integer(42), 5)));
        assert_eq!(decode(b"$3\r\nabc\r\n"), Some((Frame::Bulk(b"abc".to_vec()), 9)));
        assert_eq!(decode(b"$0\r\n\r\n"), Some((Frame::Bulk(Vec::new()), 6)));
    }

    #[test]
    fn decodes_both_spellings_of_null() {
        assert_eq!(decode(b"$-1\r\n"), Some((Frame::Null, 5)));
        assert_eq!(decode(b"*-1\r\n"), Some((Frame::Null, 5)));
        assert_eq!(decode(b"_\r\n"), Some((Frame::Null, 3)));
    }

    #[test]
    fn decodes_negative_and_extreme_integers() {
        assert_eq!(decode(b":-1\r\n"), Some((Frame::Integer(-1), 5)));
        // Accumulating negatively is the only way to reach i64::MIN.
        let min = b":-9223372036854775808\r\n";
        assert_eq!(decode(min), Some((Frame::Integer(i64::MIN), min.len())));
        let max = b":9223372036854775807\r\n";
        assert_eq!(decode(max), Some((Frame::Integer(i64::MAX), max.len())));
    }

    #[test]
    fn rejects_integer_overflow_rather_than_wrapping() {
        assert_eq!(
            Decoder::new().decode(b":9223372036854775808\r\n"),
            Err(DecodeError::InvalidInteger)
        );
    }

    #[test]
    fn decodes_nested_aggregates() {
        let (frame, consumed) = decode(b"*2\r\n:1\r\n*1\r\n$2\r\nhi\r\n").expect("complete");
        assert_eq!(
            frame,
            Frame::Array(vec![Frame::Integer(1), Frame::Array(vec![Frame::Bulk(b"hi".to_vec())]),])
        );
        assert_eq!(consumed, 20);
    }

    #[test]
    fn decodes_resp3_types() {
        assert_eq!(decode(b"#t\r\n"), Some((Frame::Boolean(true), 4)));
        assert_eq!(decode(b",3.5\r\n"), Some((Frame::Double(3.5), 6)));
        assert_eq!(decode(b",inf\r\n"), Some((Frame::Double(f64::INFINITY), 6)));
        assert_eq!(
            decode(b"(12345678901234567890123\r\n"),
            Some((Frame::BigNumber(b"12345678901234567890123".to_vec()), 26))
        );
        assert_eq!(
            decode(b"=9\r\ntxt:hello\r\n"),
            Some((Frame::Verbatim { format: *b"txt", data: b"hello".to_vec() }, 15))
        );
        assert_eq!(
            decode(b"%1\r\n+k\r\n:1\r\n"),
            Some((Frame::Map(vec![(Frame::Simple(b"k".to_vec()), Frame::Integer(1))]), 12))
        );
        assert_eq!(
            decode(b">2\r\n$7\r\nmessage\r\n$2\r\nhi\r\n"),
            Some((
                Frame::Push(vec![Frame::Bulk(b"message".to_vec()), Frame::Bulk(b"hi".to_vec())]),
                25
            ))
        );
    }

    #[test]
    fn a_truncated_frame_is_not_an_error() {
        // Every prefix of a valid frame must ask for more rather than fail.
        let full = b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n";
        for cut in 0..full.len() {
            assert_eq!(
                Decoder::new().decode(&full[..cut]),
                Ok(None),
                "prefix of length {cut} should be incomplete"
            );
        }
        assert!(Decoder::new().decode(full).expect("valid").is_some());
    }

    #[test]
    fn reports_only_the_bytes_it_consumed() {
        let (frame, consumed) = decode(b"+OK\r\n+SECOND\r\n").expect("complete");
        assert_eq!(frame, Frame::Simple(b"OK".to_vec()));
        assert_eq!(consumed, 5, "must not swallow the pipelined frame behind it");
    }

    #[test]
    fn enforces_limits_against_a_hostile_peer() {
        let limits =
            Limits { max_depth: 2, max_array_len: 4, max_bulk_len: 8, ..Limits::default() };
        let decoder = Decoder::with_limits(limits);

        // Announcing a huge aggregate must fail before anything is allocated.
        assert_eq!(decoder.decode(b"*5\r\n"), Err(DecodeError::ArrayTooLong));
        assert_eq!(decoder.decode(b"$9\r\n"), Err(DecodeError::BulkTooLong));
        assert_eq!(
            decoder.decode(b"*1\r\n*1\r\n*1\r\n*1\r\n:1\r\n"),
            Err(DecodeError::DepthExceeded)
        );
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(Decoder::new().decode(b"@nope\r\n"), Err(DecodeError::UnexpectedByte(b'@')));
        assert_eq!(Decoder::new().decode(b":abc\r\n"), Err(DecodeError::InvalidInteger));
        assert_eq!(Decoder::new().decode(b"#x\r\n"), Err(DecodeError::InvalidBoolean));
        assert_eq!(Decoder::new().decode(b"$3\r\nabcXX"), Err(DecodeError::MissingTerminator));
        assert_eq!(Decoder::new().decode(b"$-2\r\n"), Err(DecodeError::InvalidLength));
        assert_eq!(Decoder::new().decode(b"*?\r\n"), Err(DecodeError::StreamedNotSupported));
        assert_eq!(Decoder::new().decode(b"=3\r\nabc\r\n"), Err(DecodeError::InvalidVerbatim));
    }

    #[test]
    fn rejects_a_stray_line_break_inside_a_line() {
        // `+a\rb\r\n` would decode, then re-encode as `+a b\r\n` once the encoder
        // sanitised it — a proxy built on that would silently rewrite traffic.
        assert_eq!(Decoder::new().decode(b"+a\rb\r\n"), Err(DecodeError::StrayLineBreak));
        assert_eq!(Decoder::new().decode(b"-ERR a\nb\r\n"), Err(DecodeError::StrayLineBreak));
        // A bulk string is length-prefixed, so it may carry anything at all.
        assert_eq!(
            decode(b"$4\r\na\r\nb\r\n"),
            Some((Frame::Bulk(b"a\r\nb".to_vec()), 10)),
            "only line-terminated frames are restricted"
        );
    }

    #[test]
    fn every_decodable_frame_survives_a_resp3_round_trip() {
        // The property the roundtrip fuzz target asserts, pinned as a unit test so a
        // regression shows up without running the fuzzer.
        use crate::{Encoder, RespProtocol};

        let inputs: &[&[u8]] = &[
            b"+OK\r\n",
            b"-ERR nope\r\n",
            b":-9223372036854775808\r\n",
            b"$4\r\na\r\nb\r\n",
            b"_\r\n",
            b"#t\r\n",
            b"(123456789012345678901234567890\r\n",
            b"=9\r\ntxt:hello\r\n",
            b"%1\r\n$1\r\nk\r\n:1\r\n",
            b"~2\r\n:1\r\n:2\r\n",
            b">2\r\n$7\r\nmessage\r\n$2\r\nhi\r\n",
            b"*2\r\n:1\r\n*1\r\n$2\r\nhi\r\n",
        ];

        let decoder = Decoder::new();
        for input in inputs {
            let (frame, _) = decoder.decode(input).expect("valid").expect("complete");

            let mut encoded = Vec::new();
            Encoder::new(RespProtocol::Resp3).encode(&frame, &mut encoded);

            let (reparsed, consumed) =
                decoder.decode(&encoded).expect("re-decodable").expect("complete");
            assert_eq!(consumed, encoded.len());
            assert_eq!(frame, reparsed, "round trip changed {input:?}");
        }
    }

    #[test]
    fn decodes_commands_in_both_forms() {
        let decoder = Decoder::new();
        assert_eq!(
            decoder.decode_command(b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n").expect("valid"),
            Some((vec![b"GET".to_vec(), b"key".to_vec()], 22))
        );
        assert_eq!(
            decoder.decode_command(b"PING\r\n").expect("valid"),
            Some((vec![b"PING".to_vec()], 6))
        );
        // A bare newline is what a telnet session and some health checks send.
        assert_eq!(
            decoder.decode_command(b"SET a b\n").expect("valid"),
            Some((vec![b"SET".to_vec(), b"a".to_vec(), b"b".to_vec()], 8))
        );
        // An empty line is a no-op, not an error.
        assert_eq!(decoder.decode_command(b"\r\n").expect("valid"), Some((Vec::new(), 2)));
    }

    #[test]
    fn rejects_a_well_formed_frame_that_is_not_a_command() {
        assert_eq!(Decoder::new().decode_command(b"*1\r\n:1\r\n"), Err(DecodeError::NotACommand));
        assert_eq!(Decoder::new().decode_command(b"*-1\r\n"), Err(DecodeError::NotACommand));
    }
}

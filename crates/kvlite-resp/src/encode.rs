use alloc::format;
use alloc::vec::Vec;

use crate::Frame;

/// The RESP version negotiated on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum RespProtocol {
    /// RESP2. Every reply is a string, integer, error or array.
    #[default]
    Resp2,
    /// RESP3. Adds typed nulls, booleans, doubles, maps, sets and out-of-band pushes.
    Resp3,
}

impl RespProtocol {
    /// The version number a client sends in `HELLO`.
    pub const fn version(self) -> u8 {
        match self {
            Self::Resp2 => 2,
            Self::Resp3 => 3,
        }
    }

    /// Maps a `HELLO` argument to a protocol version.
    pub const fn from_version(version: u8) -> Option<Self> {
        match version {
            2 => Some(Self::Resp2),
            3 => Some(Self::Resp3),
            _ => None,
        }
    }
}

/// Writes frames as RESP bytes.
///
/// One encoder serves both protocol versions. Given a RESP3-only frame on a RESP2
/// connection it downgrades rather than failing, because a server should be able to
/// build one reply and send it to any client:
///
/// | Frame | RESP3 | RESP2 |
/// |---|---|---|
/// | [`Frame::Null`] | `_` | `$-1` |
/// | [`Frame::Boolean`] | `#t` / `#f` | `:1` / `:0` |
/// | [`Frame::Double`] | `,` | bulk string |
/// | [`Frame::BigNumber`] | `(` | bulk string |
/// | [`Frame::Verbatim`] | `=` | bulk string, format dropped |
/// | [`Frame::Map`] | `%` | flat array of key, value, key, value |
/// | [`Frame::Set`] | `~` | array |
/// | [`Frame::Push`] | `>` | array |
#[derive(Debug, Clone, Copy, Default)]
pub struct Encoder {
    protocol: RespProtocol,
}

impl Encoder {
    /// An encoder for the given protocol version.
    pub const fn new(protocol: RespProtocol) -> Self {
        Self { protocol }
    }

    /// The protocol version this encoder writes.
    pub const fn protocol(self) -> RespProtocol {
        self.protocol
    }

    /// Appends the encoding of `frame` to `out`.
    pub fn encode(&self, frame: &Frame, out: &mut Vec<u8>) {
        let resp3 = self.protocol == RespProtocol::Resp3;

        match frame {
            Frame::Simple(text) => self.write_line(b'+', text, out),
            // RESP3 has a length-prefixed error type, which is the only way to carry
            // an error message containing a line break without mangling it. RESP2 has
            // no such frame, so there the line is sanitised and the break is lost.
            Frame::Error(text) if resp3 && contains_line_break(text) => {
                write_bulk(b'!', text, out);
            }
            Frame::Error(text) => self.write_line(b'-', text, out),
            Frame::Integer(value) => {
                out.push(b':');
                write_i64(*value, out);
                out.extend_from_slice(b"\r\n");
            }
            Frame::Bulk(bytes) => write_bulk(b'$', bytes, out),
            Frame::Null => {
                if resp3 {
                    out.extend_from_slice(b"_\r\n");
                } else {
                    out.extend_from_slice(b"$-1\r\n");
                }
            }
            Frame::Boolean(value) => {
                if resp3 {
                    out.extend_from_slice(if *value { b"#t\r\n" } else { b"#f\r\n" });
                } else {
                    out.extend_from_slice(if *value { b":1\r\n" } else { b":0\r\n" });
                }
            }
            Frame::Double(value) => {
                let text = format_double(*value);
                if resp3 {
                    out.push(b',');
                    out.extend_from_slice(text.as_bytes());
                    out.extend_from_slice(b"\r\n");
                } else {
                    write_bulk(b'$', text.as_bytes(), out);
                }
            }
            Frame::BigNumber(digits) => {
                if resp3 {
                    out.push(b'(');
                    out.extend_from_slice(digits);
                    out.extend_from_slice(b"\r\n");
                } else {
                    write_bulk(b'$', digits, out);
                }
            }
            Frame::Verbatim { format, data } => {
                if resp3 {
                    let mut payload = Vec::with_capacity(data.len() + 4);
                    payload.extend_from_slice(format);
                    payload.push(b':');
                    payload.extend_from_slice(data);
                    write_bulk(b'=', &payload, out);
                } else {
                    write_bulk(b'$', data, out);
                }
            }
            Frame::Array(items) => self.write_aggregate(b'*', items, out),
            Frame::Set(items) => self.write_aggregate(if resp3 { b'~' } else { b'*' }, items, out),
            Frame::Push(items) => self.write_aggregate(if resp3 { b'>' } else { b'*' }, items, out),
            Frame::Map(pairs) => {
                if resp3 {
                    out.push(b'%');
                    write_i64(pairs.len() as i64, out);
                } else {
                    // A RESP2 client expects a flat array of twice the length.
                    out.push(b'*');
                    write_i64((pairs.len() as i64).saturating_mul(2), out);
                }
                out.extend_from_slice(b"\r\n");
                for (key, value) in pairs {
                    self.encode(key, out);
                    self.encode(value, out);
                }
            }
        }
    }

    fn write_aggregate(&self, tag: u8, items: &[Frame], out: &mut Vec<u8>) {
        out.push(tag);
        write_i64(items.len() as i64, out);
        out.extend_from_slice(b"\r\n");
        for item in items {
            self.encode(item, out);
        }
    }

    /// Writes a `+` or `-` line, stripping CR and LF from the payload.
    ///
    /// This is not cosmetic. A simple string is terminated by CRLF, so an error
    /// message built from user-supplied bytes would otherwise let a client inject
    /// a second, forged frame into the reply stream.
    fn write_line(&self, tag: u8, text: &[u8], out: &mut Vec<u8>) {
        out.push(tag);
        out.reserve(text.len() + 2);
        for &byte in text {
            out.push(if byte == b'\r' || byte == b'\n' { b' ' } else { byte });
        }
        out.extend_from_slice(b"\r\n");
    }
}

fn contains_line_break(text: &[u8]) -> bool {
    text.iter().any(|&byte| byte == b'\r' || byte == b'\n')
}

fn write_bulk(tag: u8, bytes: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    write_i64(bytes.len() as i64, out);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

/// Appends the decimal text of `value`. Avoids `format!` on the hottest path there is.
fn write_i64(value: i64, out: &mut Vec<u8>) {
    if value == 0 {
        out.push(b'0');
        return;
    }

    // Accumulate negatively so i64::MIN has no special case.
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
}

fn format_double(value: f64) -> alloc::string::String {
    if value.is_nan() {
        // Rust prints "NaN"; RESP3 says "nan".
        return alloc::string::String::from("nan");
    }
    // Rust already prints "inf" and "-inf", which is what RESP3 wants.
    format!("{value}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decoder;
    use alloc::vec;

    fn encode(protocol: RespProtocol, frame: &Frame) -> Vec<u8> {
        let mut out = Vec::new();
        Encoder::new(protocol).encode(frame, &mut out);
        out
    }

    #[test]
    fn encodes_the_scalar_types() {
        assert_eq!(encode(RespProtocol::Resp2, &Frame::ok()), b"+OK\r\n");
        assert_eq!(encode(RespProtocol::Resp2, &Frame::bulk("value")), b"$5\r\nvalue\r\n");
        assert_eq!(encode(RespProtocol::Resp2, &Frame::Integer(-7)), b":-7\r\n");
        assert_eq!(encode(RespProtocol::Resp2, &Frame::Bulk(Vec::new())), b"$0\r\n\r\n");
    }

    #[test]
    fn writes_extreme_integers_without_a_special_case() {
        assert_eq!(
            encode(RespProtocol::Resp2, &Frame::Integer(i64::MIN)),
            b":-9223372036854775808\r\n"
        );
        assert_eq!(
            encode(RespProtocol::Resp2, &Frame::Integer(i64::MAX)),
            b":9223372036854775807\r\n"
        );
        assert_eq!(encode(RespProtocol::Resp2, &Frame::Integer(0)), b":0\r\n");
    }

    #[test]
    fn downgrades_resp3_frames_for_a_resp2_client() {
        assert_eq!(encode(RespProtocol::Resp2, &Frame::Null), b"$-1\r\n");
        assert_eq!(encode(RespProtocol::Resp2, &Frame::Boolean(true)), b":1\r\n");
        assert_eq!(encode(RespProtocol::Resp2, &Frame::Double(3.5)), b"$3\r\n3.5\r\n");
        assert_eq!(
            encode(RespProtocol::Resp2, &Frame::Map(vec![(Frame::bulk("k"), Frame::Integer(1))])),
            b"*2\r\n$1\r\nk\r\n:1\r\n"
        );
        assert_eq!(
            encode(RespProtocol::Resp2, &Frame::Set(vec![Frame::Integer(1)])),
            b"*1\r\n:1\r\n"
        );
        assert_eq!(
            encode(RespProtocol::Resp2, &Frame::Verbatim { format: *b"txt", data: b"hi".to_vec() }),
            b"$2\r\nhi\r\n"
        );
    }

    #[test]
    fn encodes_resp3_frames_natively() {
        assert_eq!(encode(RespProtocol::Resp3, &Frame::Null), b"_\r\n");
        assert_eq!(encode(RespProtocol::Resp3, &Frame::Boolean(false)), b"#f\r\n");
        assert_eq!(encode(RespProtocol::Resp3, &Frame::Double(3.5)), b",3.5\r\n");
        assert_eq!(encode(RespProtocol::Resp3, &Frame::Double(f64::NAN)), b",nan\r\n");
        assert_eq!(encode(RespProtocol::Resp3, &Frame::Double(f64::INFINITY)), b",inf\r\n");
        assert_eq!(
            encode(RespProtocol::Resp3, &Frame::Map(vec![(Frame::bulk("k"), Frame::Integer(1))])),
            b"%1\r\n$1\r\nk\r\n:1\r\n"
        );
    }

    #[test]
    fn strips_crlf_from_status_and_error_lines() {
        // Without this a client-supplied name could forge a second reply frame.
        let injected = Frame::error("ERR bad\r\n+INJECTED");
        assert_eq!(encode(RespProtocol::Resp2, &injected), b"-ERR bad  +INJECTED\r\n");

        // And the result must still decode as exactly one frame.
        let bytes = encode(RespProtocol::Resp2, &injected);
        let (_, consumed) = Decoder::new().decode(&bytes).expect("valid").expect("complete");
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn a_multiline_error_becomes_a_bulk_error_on_resp3() {
        let multiline = Frame::error("ERR line one\r\nline two");

        assert_eq!(
            encode(RespProtocol::Resp3, &multiline),
            b"!22\r\nERR line one\r\nline two\r\n",
            "RESP3 has a length-prefixed error type; use it rather than mangling the text"
        );
        assert_eq!(
            encode(RespProtocol::Resp2, &multiline),
            b"-ERR line one  line two\r\n",
            "RESP2 has no such frame, so the break is sanitised away"
        );

        // And the RESP3 form comes back as the same value.
        let bytes = encode(RespProtocol::Resp3, &multiline);
        let (decoded, _) = Decoder::new().decode(&bytes).expect("valid").expect("complete");
        assert_eq!(decoded, multiline);
    }

    #[test]
    fn round_trips_through_the_decoder() {
        let frames = vec![
            Frame::ok(),
            Frame::error("ERR nope"),
            Frame::Integer(i64::MIN),
            Frame::bulk("hello"),
            Frame::Null,
            Frame::Array(vec![Frame::Integer(1), Frame::bulk("two")]),
            Frame::Boolean(true),
            Frame::Double(-2.5),
            Frame::BigNumber(b"123456789012345678901234567890".to_vec()),
            Frame::Verbatim { format: *b"mkd", data: b"# hi".to_vec() },
            Frame::Map(vec![(Frame::bulk("k"), Frame::Integer(9))]),
            Frame::Set(vec![Frame::bulk("s")]),
            Frame::Push(vec![Frame::bulk("message"), Frame::bulk("body")]),
        ];

        let decoder = Decoder::new();
        for frame in &frames {
            let bytes = encode(RespProtocol::Resp3, frame);
            let (decoded, consumed) =
                decoder.decode(&bytes).expect("re-decodable").expect("complete");
            assert_eq!(&decoded, frame, "round trip failed for {frame:?}");
            assert_eq!(consumed, bytes.len());
        }
    }

    #[test]
    fn round_trips_a_pipeline_of_frames() {
        let mut buffer = Vec::new();
        let encoder = Encoder::new(RespProtocol::Resp2);
        for i in 0..8 {
            encoder.encode(&Frame::Integer(i), &mut buffer);
        }

        let decoder = Decoder::new();
        let mut offset = 0;
        for i in 0..8 {
            let (frame, consumed) =
                decoder.decode(&buffer[offset..]).expect("valid").expect("complete");
            assert_eq!(frame, Frame::Integer(i));
            offset += consumed;
        }
        assert_eq!(offset, buffer.len());
    }
}

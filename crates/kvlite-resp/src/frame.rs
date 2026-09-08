use alloc::string::String;
use alloc::vec::Vec;

/// One RESP value.
///
/// RESP2 and RESP3 share this type. A RESP2 peer simply never produces the
/// RESP3-only variants, and [`crate::Encoder`] downgrades them on the way out when
/// the connection negotiated RESP2.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Frame {
    /// `+` — a single-line status string.
    Simple(Vec<u8>),
    /// `-` — an error. Also produced for RESP3 `!` bulk errors.
    Error(Vec<u8>),
    /// `:` — a 64-bit signed integer.
    Integer(i64),
    /// `$` — a length-prefixed binary-safe string.
    Bulk(Vec<u8>),
    /// A null. RESP3 `_`, or a RESP2 null bulk string or null array.
    Null,
    /// `*` — an array.
    Array(Vec<Frame>),
    /// `#` — a RESP3 boolean.
    Boolean(bool),
    /// `,` — a RESP3 double.
    Double(f64),
    /// `(` — a RESP3 big number, kept as its decimal text.
    BigNumber(Vec<u8>),
    /// `=` — a RESP3 verbatim string with a three-byte format such as `txt` or `mkd`.
    Verbatim {
        /// The three-byte format tag.
        format: [u8; 3],
        /// The payload, without the format prefix.
        data: Vec<u8>,
    },
    /// `%` — a RESP3 map, held as ordered pairs so encoding round-trips exactly.
    Map(Vec<(Frame, Frame)>),
    /// `~` — a RESP3 set.
    Set(Vec<Frame>),
    /// `>` — a RESP3 out-of-band push, used for pub/sub.
    Push(Vec<Frame>),
}

impl Frame {
    /// A `+OK` status, the most common reply there is.
    pub fn ok() -> Self {
        Self::Simple(b"OK".to_vec())
    }

    /// A simple status string.
    pub fn simple(text: impl AsRef<[u8]>) -> Self {
        Self::Simple(text.as_ref().to_vec())
    }

    /// An error reply. The text should start with an error code, as Redis does.
    pub fn error(text: impl AsRef<[u8]>) -> Self {
        Self::Error(text.as_ref().to_vec())
    }

    /// A bulk string.
    pub fn bulk(bytes: impl AsRef<[u8]>) -> Self {
        Self::Bulk(bytes.as_ref().to_vec())
    }

    /// Whether this frame is a null in either protocol version.
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// The payload of a string-like frame, or `None` for anything else.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Simple(b) | Self::Error(b) | Self::Bulk(b) | Self::BigNumber(b) => Some(b),
            Self::Verbatim { data, .. } => Some(data),
            _ => None,
        }
    }

    /// The payload of a string-like frame decoded as UTF-8, or `None`.
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.as_bytes()?).ok()
    }

    /// The payload of a string-like frame as an owned `String`, lossily decoded.
    pub fn to_string_lossy(&self) -> Option<String> {
        Some(String::from_utf8_lossy(self.as_bytes()?).into_owned())
    }
}

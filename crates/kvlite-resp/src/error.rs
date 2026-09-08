use core::fmt;

/// Why a byte stream could not be decoded as RESP.
///
/// A truncated but otherwise valid stream is *not* an error — [`crate::Decoder::decode`]
/// returns `Ok(None)` for that. Everything here means the peer is wrong, or hostile,
/// and the connection should be closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecodeError {
    /// A frame started with a byte that is not a RESP type marker.
    UnexpectedByte(u8),
    /// A length or count prefix was not a valid integer, or was negative where it may not be.
    InvalidLength,
    /// An integer frame did not hold a valid `i64`.
    InvalidInteger,
    /// A double frame did not hold a valid floating point value.
    InvalidDouble,
    /// A boolean frame was neither `t` nor `f`.
    InvalidBoolean,
    /// A verbatim string was missing its three-byte format prefix.
    InvalidVerbatim,
    /// A bulk string's trailing CRLF was missing or misplaced.
    MissingTerminator,
    /// A single-line frame contained a stray CR or LF.
    ///
    /// A `+` or `-` line is terminated by CRLF and so cannot carry one. Accepting
    /// it would mean the encoder has to sanitise on the way back out, which makes
    /// decode-then-encode lossy — and a proxy built on that quietly rewrites the
    /// traffic it forwards.
    StrayLineBreak,
    /// A single line exceeded [`crate::Limits::max_line_len`].
    LineTooLong,
    /// A bulk string exceeded [`crate::Limits::max_bulk_len`].
    BulkTooLong,
    /// An aggregate exceeded [`crate::Limits::max_array_len`].
    ArrayTooLong,
    /// Nesting exceeded [`crate::Limits::max_depth`].
    DepthExceeded,
    /// A streamed aggregate or bulk string (`$?`, `*?`) was received. Not supported.
    StreamedNotSupported,
    /// A command frame was not an array of bulk strings.
    NotACommand,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedByte(b) => {
                write!(f, "unexpected byte {b:#04x} where a RESP type was expected")
            }
            Self::InvalidLength => f.write_str("invalid length or count prefix"),
            Self::InvalidInteger => f.write_str("invalid integer"),
            Self::InvalidDouble => f.write_str("invalid double"),
            Self::InvalidBoolean => f.write_str("invalid boolean, expected 't' or 'f'"),
            Self::InvalidVerbatim => f.write_str("verbatim string is missing its format prefix"),
            Self::MissingTerminator => f.write_str("bulk string is not terminated by CRLF"),
            Self::StrayLineBreak => f.write_str("single-line frame contains a stray CR or LF"),
            Self::LineTooLong => f.write_str("line exceeds the configured limit"),
            Self::BulkTooLong => f.write_str("bulk string exceeds the configured limit"),
            Self::ArrayTooLong => f.write_str("aggregate exceeds the configured limit"),
            Self::DepthExceeded => f.write_str("nesting exceeds the configured limit"),
            Self::StreamedNotSupported => f.write_str("streamed aggregates are not supported"),
            Self::NotACommand => f.write_str("expected an array of bulk strings"),
        }
    }
}

impl core::error::Error for DecodeError {}

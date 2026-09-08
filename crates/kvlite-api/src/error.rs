use core::fmt;

/// Everything that can go wrong inside the engine.
///
/// The [`fmt::Display`] text of each variant is the exact string Redis puts on the
/// wire for the same condition, so a server can forward it without translating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KvError {
    /// An operation was applied to a key holding a different type,
    /// such as a list command against a string key.
    WrongType,
    /// The stored value cannot be read as the 64-bit integer the operation needs.
    NotAnInteger,
    /// A numeric argument or result is outside the range the operation allows.
    OutOfRange,
    /// An expiry was supplied that cannot be represented.
    InvalidExpiry,
    /// A value or argument could not be read as a floating-point number.
    ///
    /// A sorted-set score may be any finite double, or an infinity. It may not be
    /// NaN, because NaN has no position in a total order.
    NotAFloat,
    /// An arithmetic result would be NaN, such as adding `+inf` to `-inf`.
    NaNResult,
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WrongType => "WRONGTYPE Operation against a key holding the wrong kind of value",
            Self::NotAnInteger => "ERR value is not an integer or out of range",
            Self::OutOfRange => "ERR value is out of range",
            Self::InvalidExpiry => "ERR invalid expire time",
            Self::NotAFloat => "ERR value is not a valid float",
            Self::NaNResult => "ERR resulting score is not a number (NaN)",
        })
    }
}

impl core::error::Error for KvError {}

/// Result alias for engine operations.
pub type KvResult<T> = Result<T, KvError>;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn display_matches_the_redis_wire_text() {
        // The server forwards these verbatim, so a change here is a wire change.
        assert!(KvError::WrongType.to_string().starts_with("WRONGTYPE "));
        assert!(KvError::NotAnInteger.to_string().starts_with("ERR "));
    }
}

/// Decoder limits, enforced on every frame.
///
/// These exist because a decoder sits on a trust boundary. Without them, a peer
/// sending `*1000000000\r\n` makes you allocate a billion-element vector before
/// you have read a single element.
///
/// The defaults match Redis where Redis has an equivalent setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Longest single CRLF-terminated line, including inline commands. Default 64 KiB.
    pub max_line_len: usize,
    /// Longest bulk string. Default 512 MiB, matching Redis `proto-max-bulk-len`.
    pub max_bulk_len: usize,
    /// Most elements in one aggregate. Default 1,048,576, matching Redis.
    pub max_array_len: usize,
    /// Deepest aggregate nesting. Default 32.
    ///
    /// The decoder recurses, so this is what stands between a hostile peer and a
    /// stack overflow.
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_line_len: 64 * 1024,
            max_bulk_len: 512 * 1024 * 1024,
            max_array_len: 1024 * 1024,
            max_depth: 32,
        }
    }
}

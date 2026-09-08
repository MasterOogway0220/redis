/// The type of the value stored at a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KvValueType {
    /// The key does not exist.
    None,
    /// A binary-safe string.
    String,
    /// An ordered sequence of elements.
    List,
    /// A map of fields to values.
    Hash,
    /// An unordered collection of unique members.
    Set,
    /// A collection of unique members ordered by score.
    SortedSet,
}

impl KvValueType {
    /// The name Redis reports for this type from `TYPE`.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::String => "string",
            Self::List => "list",
            Self::Hash => "hash",
            Self::Set => "set",
            // Redis has always called this "zset" on the wire, whatever the docs
            // call it. Clients match on the string.
            Self::SortedSet => "zset",
        }
    }
}

/// When a write should be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum KvWriteCondition {
    /// Always write.
    #[default]
    Always,
    /// Write only when the key does not already exist (Redis `NX`).
    NotExists,
    /// Write only when the key already exists (Redis `XX`).
    Exists,
}

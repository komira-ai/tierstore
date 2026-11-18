//! The one error of every decode and of the encodes that check their input.

/// Why bytes do not decode as a format, or why a value cannot be encoded. Every variant names
/// the format (`what`) so a log line says which file or message was bad.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// The bytes end before the format does.
    #[error("{what}: {need} bytes needed, {have} present")]
    Short {
        /// The format being read.
        what: &'static str,
        /// The bytes the format needs at this point.
        need: u64,
        /// The bytes that were there.
        have: u64,
    },
    /// The magic number is not the format's.
    #[error("{what}: bad magic {found:#010x}")]
    BadMagic {
        /// The format being read.
        what: &'static str,
        /// The four bytes found, as a little-endian `u32`.
        found: u32,
    },
    /// A stored crc32c differs from the crc32c of the bytes it covers.
    #[error("{what}: crc32c {stored:#010x} stored, {computed:#010x} computed")]
    BadCrc {
        /// The format being read.
        what: &'static str,
        /// The crc the bytes carry.
        stored: u32,
        /// The crc of the bytes read.
        computed: u32,
    },
    /// A format version this release does not read.
    #[error("{what}: version {version} is not supported")]
    UnsupportedVersion {
        /// The format being read.
        what: &'static str,
        /// The version found.
        version: u32,
    },
    /// A field holds a value the format does not allow.
    #[error("{what}: {reason}")]
    Invalid {
        /// The format being read or written.
        what: &'static str,
        /// What is wrong, in a few words.
        reason: &'static str,
    },
}

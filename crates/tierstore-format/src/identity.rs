//! The identity files `NODE` (hot directory, 10.3) and `DRIVE` (each drive's cold directory,
//! 10.4), checked at startup against the membership document (10.8: the wrong-mount guard).
//!
//! Both are text an operator can read: one `key = value` line per field, in this order, and
//! nothing else.
//!
//! ```text
//! format = 1
//! cluster = <32 hex digits>
//! node = <decimal>
//! drive = <32 hex digits>          (DRIVE only)
//! filesystem = <32 hex digits>     (DRIVE only: the filesystem UUID)
//! ```

use std::str::FromStr;

use crate::FormatError;
use crate::ids::{ClusterId, DriveId, NodeId};

/// The format version of the identity files this release writes and reads.
pub const IDENTITY_VERSION: u32 = 1;

/// The content of a `NODE` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeIdentity {
    /// The cluster the node belongs to.
    pub cluster: ClusterId,
    /// The node's id.
    pub node: NodeId,
}

/// The content of a `DRIVE` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveIdentity {
    /// The cluster the drive belongs to.
    pub cluster: ClusterId,
    /// The node that owns the drive.
    pub node: NodeId,
    /// The drive's id.
    pub drive: DriveId,
    /// The UUID of the drive's filesystem, which must also match the mounted filesystem's.
    pub filesystem: DriveId,
}

impl NodeIdentity {
    /// The file's text.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "format = {IDENTITY_VERSION}\ncluster = {}\nnode = {}\n",
            self.cluster, self.node
        )
    }

    /// The identity in a file's text.
    pub fn decode(text: &str) -> Result<Self, FormatError> {
        let mut lines = Lines::new(text, "NODE file")?;
        let cluster = lines.field("cluster")?;
        let node = NodeId(lines.field("node")?);
        lines.end()?;
        canonical(Self { cluster, node }, text, Self::encode)
    }
}

impl DriveIdentity {
    /// The file's text.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "format = {IDENTITY_VERSION}\ncluster = {}\nnode = {}\ndrive = {}\nfilesystem = {}\n",
            self.cluster, self.node, self.drive, self.filesystem
        )
    }

    /// The identity in a file's text.
    pub fn decode(text: &str) -> Result<Self, FormatError> {
        let mut lines = Lines::new(text, "DRIVE file")?;
        let cluster = lines.field("cluster")?;
        let node = NodeId(lines.field("node")?);
        let drive = lines.field("drive")?;
        let filesystem = lines.field("filesystem")?;
        lines.end()?;
        canonical(
            Self {
                cluster,
                node,
                drive,
                filesystem,
            },
            text,
            Self::encode,
        )
    }
}

/// `id` if `text` is exactly the text `encode` writes for it (no leading zeros, signs, CRLF or
/// missing final newline), the same rule the file-name parsers follow.
fn canonical<T>(id: T, text: &str, encode: fn(&T) -> String) -> Result<T, FormatError> {
    if encode(&id) == text {
        Ok(id)
    } else {
        Err(FormatError::Invalid {
            what: "identity file",
            reason: "not in the canonical form this release writes",
        })
    }
}

struct Lines<'a> {
    lines: std::str::Lines<'a>,
    what: &'static str,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str, what: &'static str) -> Result<Self, FormatError> {
        let mut me = Self {
            lines: text.lines(),
            what,
        };
        let version: u32 = me.field("format")?;
        if version != IDENTITY_VERSION {
            return Err(FormatError::UnsupportedVersion { what, version });
        }
        Ok(me)
    }

    fn field<T: FromStr>(&mut self, key: &str) -> Result<T, FormatError> {
        let line = self.lines.next().ok_or(FormatError::Invalid {
            what: self.what,
            reason: "a field is missing",
        })?;
        line.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(" = "))
            .and_then(|value| value.parse().ok())
            .ok_or(FormatError::Invalid {
                what: self.what,
                reason: "a line is not the expected `key = value`",
            })
    }

    fn end(mut self) -> Result<(), FormatError> {
        match self.lines.next() {
            None => Ok(()),
            Some(_) => Err(FormatError::Invalid {
                what: self.what,
                reason: "a line after the last field",
            }),
        }
    }
}

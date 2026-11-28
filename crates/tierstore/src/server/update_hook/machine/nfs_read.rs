//! The read of a tree from the NFS front of the node's `fs` interface, over loopback TCP, as the
//! kernel's client of the mount reads it: the export's root handle (MOUNT's `MNT` of `/`), a
//! `LOOKUP` of each name down to the tree's directory, then every directory listed (`READDIRPLUS`),
//! every file read whole (`READ`) and the link's target read (`READLINK`), compared by the check as
//! a read through the mount is. NFS version 3 (RFC 1813) in the RPC of RFC 5531, framed by hand,
//! with the credential `AUTH_SYS` of root, which the front serves as it serves every user.
//!
//! On a Mac the hook reads the check's trees this way and never through the mount: macOS refuses a
//! process the privacy grant of a network volume (Full Disk Access) does not cover, root included,
//! every read of the volume (`Operation not permitted`), and the process hostkeeper runs the hook in
//! has no such grant. The front is the part of the node the kernel's client asks: the same calls,
//! answered by the same core from the same placements and the same cache.
//!
//! What the read gives up on, as the read through the mount does: past
//! [`MAX_PATHS`](super::provider::MAX_PATHS) paths or a file over
//! [`MAX_FILE_BYTES`](super::provider::MAX_FILE_BYTES), a name or a link target that is not UTF-8,
//! and an entry of no kind of a tree ([`ReadFault::NotTheTree`]); a status of the front, an answer
//! that is not one of the protocol, and a connection that fails ([`ReadFault::Failed`]); and a front
//! that does not answer within the bound, or answers `NFS3ERR_JUKEBOX` (a fill that has not ended),
//! which is slowness ([`ReadFault::Unread`]).

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Component, Path};
use std::time::Duration;

use super::super::ports::{ReadFault, TreeNode};
use super::provider::{MAX_FILE_BYTES, MAX_PATHS};

/// The program of the MOUNT protocol, and its procedure `MNT`.
const MOUNT_PROGRAM: u32 = 100_005;
const MNT: u32 = 1;
/// The program of NFS, and the procedures the read makes.
const NFS_PROGRAM: u32 = 100_003;
const LOOKUP: u32 = 3;
const READLINK: u32 = 5;
const READ: u32 = 6;
const READDIRPLUS: u32 = 17;
/// The version of both programs.
const VERSION: u32 = 3;
/// The status that asks the client to try again later: a fill of the file has not ended.
const JUKEBOX: u32 = 10_008;
/// The kinds of entry (`ftype3`) of a tree.
const REGULAR: u32 = 1;
const DIRECTORY: u32 = 2;
const LINK: u32 = 5;
/// The bytes of a `fattr3`.
const FATTR3: usize = 84;
/// The longest answer taken, in bytes: a `READ` of [`READ_COUNT`] or a page of a listing is far
/// smaller.
const MAX_ANSWER: usize = 1 << 20;
/// The bytes one `READ` asks for.
const READ_COUNT: u32 = 64 * 1024;
/// The bytes of names and of the whole answer one page of a listing asks for.
const DIR_COUNT: u32 = 8 * 1024;
const MAX_COUNT: u32 = 64 * 1024;
/// The export the node's file system has: its root.
const EXPORT: &str = "/";
/// The names of the statuses a read may meet, for a detail; another is said by its number.
const STATUSES: [(u32, &str); 9] = [
    (1, "NFS3ERR_PERM"),
    (2, "NFS3ERR_NOENT"),
    (5, "NFS3ERR_IO"),
    (13, "NFS3ERR_ACCES"),
    (20, "NFS3ERR_NOTDIR"),
    (70, "NFS3ERR_STALE"),
    (10_001, "NFS3ERR_BADHANDLE"),
    (10_006, "NFS3ERR_SERVERFAULT"),
    (JUKEBOX, "NFS3ERR_JUKEBOX"),
];

/// Every path below `below` (a path under the export's root, its names alone) with what it is, read
/// from the NFS front at `front`, each call answered within `bound`.
///
/// # Errors
/// As the module says.
pub(super) fn read_tree(
    front: SocketAddr,
    below: &Path,
    bound: Duration,
) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
    // The path is checked whole before anything is asked of the front.
    let names = below
        .components()
        .map(|part| match part {
            Component::Normal(name) => name
                .to_str()
                .ok_or_else(|| ReadFault::NotTheTree(format!("{} is not UTF-8", below.display()))),
            _ => Err(ReadFault::NotTheTree(format!(
                "{} is no path below the mount",
                below.display()
            ))),
        })
        .collect::<Result<Vec<&str>, ReadFault>>()?;
    let path = below.display().to_string();
    let mut client = Front::connect(front, bound)?;
    let mut dir = client.root()?;
    for name in names {
        dir = client.lookup(&dir, name, &path)?;
    }
    client.walk(dir)
}

/// An entry of a listing: its name, its kind, and its handle.
struct Listed {
    name: String,
    kind: u32,
    handle: Vec<u8>,
}

/// A connection to the front, and the last call's id.
struct Front {
    stream: TcpStream,
    front: SocketAddr,
    xid: u32,
}

impl Front {
    /// A connection to `front`, whose every read and write waits at most `bound`.
    fn connect(front: SocketAddr, bound: Duration) -> Result<Front, ReadFault> {
        let what = format!("the NFS front at {front} could not be reached");
        let stream = TcpStream::connect_timeout(&front, bound)
            .and_then(|stream| {
                stream.set_read_timeout(Some(bound))?;
                stream.set_write_timeout(Some(bound))?;
                Ok(stream)
            })
            .map_err(|error| io_fault(&what, &error))?;
        Ok(Front {
            stream,
            front,
            xid: 0,
        })
    }

    /// Calls `procedure` of `program` with `args`: the results after the RPC header. `what` names
    /// the call for a detail.
    fn call(
        &mut self,
        program: u32,
        procedure: u32,
        args: &Enc,
        what: &str,
    ) -> Result<Vec<u8>, ReadFault> {
        self.xid = self.xid.wrapping_add(1);
        // `AUTH_SYS`: a stamp, no machine name, the user and the group of root, no other group.
        let credential = Enc::default().u32(0).u32(0).u32(0).u32(0).u32(0);
        let mut message = Enc::default()
            .u32(self.xid)
            .u32(0)
            .u32(2)
            .u32(program)
            .u32(VERSION)
            .u32(procedure)
            .u32(1)
            .opaque(&credential.0)
            .u32(0)
            .u32(0);
        message.0.extend_from_slice(&args.0);
        let length = u32::try_from(message.0.len()).unwrap_or(u32::MAX) & !LAST;
        let mut record = (LAST | length).to_be_bytes().to_vec();
        record.extend_from_slice(&message.0);
        let front = format!("the NFS front at {}", self.front);
        self.stream
            .write_all(&record)
            .map_err(|error| io_fault(&format!("{front} could not be asked {what}"), &error))?;
        let answer = self.answer(what)?;
        let mut header = Dec::new(&answer, &front, what);
        let (xid, kind, state) = (header.u32()?, header.u32()?, header.u32()?);
        if (xid, kind, state) != (self.xid, 1, 0) {
            return Err(ReadFault::Failed(format!(
                "{front} answered {what} with no accepted reply to it (id {xid}, message {kind}, \
                 state {state})"
            )));
        }
        let _verifier = (header.u32()?, header.opaque()?);
        let accepted = header.u32()?;
        if accepted != 0 {
            return Err(ReadFault::Failed(format!(
                "{front} did not take {what} (accept state {accepted})"
            )));
        }
        Ok(answer[header.at..].to_vec())
    }

    /// The record of an answer to `what`: its fragments, each after its mark, joined.
    fn answer(&mut self, what: &str) -> Result<Vec<u8>, ReadFault> {
        let front = format!("the NFS front at {}", self.front);
        let unanswered = format!("{front} did not answer {what}");
        let mut answer = Vec::new();
        loop {
            let mut mark = [0u8; 4];
            self.stream
                .read_exact(&mut mark)
                .map_err(|error| io_fault(&unanswered, &error))?;
            let mark = u32::from_be_bytes(mark);
            let length = (mark & !LAST) as usize;
            if answer.len() + length > MAX_ANSWER {
                return Err(ReadFault::Failed(format!(
                    "{front} answered {what} with more than {MAX_ANSWER} bytes"
                )));
            }
            let start = answer.len();
            answer.resize(start + length, 0);
            self.stream
                .read_exact(&mut answer[start..])
                .map_err(|error| io_fault(&unanswered, &error))?;
            if mark & LAST != 0 {
                return Ok(answer);
            }
        }
    }

    /// The handle of the export's root.
    fn root(&mut self) -> Result<Vec<u8>, ReadFault> {
        let what = format!("MNT of {EXPORT}");
        let answer = self.call(
            MOUNT_PROGRAM,
            MNT,
            &Enc::default().opaque(EXPORT.as_bytes()),
            &what,
        )?;
        let front = format!("the NFS front at {}", self.front);
        let mut results = Dec::new(&answer, &front, &what);
        let status = results.u32()?;
        if status != 0 {
            return Err(ReadFault::Failed(format!(
                "{front} refused {what}: {}",
                status_name(status)
            )));
        }
        results.opaque()
    }

    /// The handle of `name` in the directory `dir`, whose path is `path`.
    fn lookup(&mut self, dir: &[u8], name: &str, path: &str) -> Result<Vec<u8>, ReadFault> {
        let what = format!("LOOKUP of {name}");
        let args = Enc::default().opaque(dir).opaque(name.as_bytes());
        let answer = self.call(NFS_PROGRAM, LOOKUP, &args, &what)?;
        let front = format!("the NFS front at {}", self.front);
        let mut results = Dec::new(&answer, &front, &what);
        status(&mut results, path, "looked up")?;
        results.opaque()
    }

    /// Every path below the directory `root`, by its path from it with `/` between names.
    fn walk(&mut self, root: Vec<u8>) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
        let mut nodes = BTreeMap::new();
        // Each directory to list: its path from `root`, what a failure calls it, and its handle.
        let mut dirs = vec![(String::new(), "the directory".to_owned(), root)];
        while let Some((name, label, handle)) = dirs.pop() {
            for entry in self.list(&handle, &label)? {
                let path = if name.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{name}/{}", entry.name)
                };
                if nodes.len() == MAX_PATHS {
                    return Err(ReadFault::NotTheTree(format!(
                        "more than {MAX_PATHS} paths"
                    )));
                }
                let node = match entry.kind {
                    DIRECTORY => {
                        dirs.push((path.clone(), path.clone(), entry.handle));
                        TreeNode::Dir
                    }
                    LINK => TreeNode::Link(self.readlink(&entry.handle, &path)?),
                    REGULAR => TreeNode::File(self.read(&entry.handle, &path)?),
                    _ => {
                        return Err(ReadFault::NotTheTree(format!(
                            "{path} is neither a file, a directory nor a link"
                        )));
                    }
                };
                nodes.insert(path, node);
            }
        }
        Ok(nodes)
    }

    /// The entries of the directory `dir`, called `label`, but `.` and `..`, page by page.
    fn list(&mut self, dir: &[u8], label: &str) -> Result<Vec<Listed>, ReadFault> {
        let what = format!("READDIRPLUS of {label}");
        let front = format!("the NFS front at {}", self.front);
        let (mut cookie, mut verifier) = (0u64, [0u8; 8]);
        let mut listed = Vec::new();
        loop {
            let args = Enc::default()
                .opaque(dir)
                .u64(cookie)
                .fixed(&verifier)
                .u32(DIR_COUNT)
                .u32(MAX_COUNT);
            let answer = self.call(NFS_PROGRAM, READDIRPLUS, &args, &what)?;
            let mut results = Dec::new(&answer, &front, &what);
            status(&mut results, label, "listed")?;
            results.post_op_attr()?;
            verifier = results.fixed8()?;
            let before = listed.len();
            while results.bool()? {
                let _fileid = results.u64()?;
                let name = results.opaque()?;
                cookie = results.u64()?;
                let kind = results.post_op_kind()?;
                let handle = results.bool()?.then(|| results.opaque()).transpose()?;
                let name = String::from_utf8(name).map_err(|_| {
                    ReadFault::NotTheTree(format!("{label} holds a name that is not UTF-8"))
                })?;
                if name == "." || name == ".." {
                    continue;
                }
                let (Some(kind), Some(handle)) = (kind, handle) else {
                    return Err(ReadFault::Failed(format!(
                        "{front} listed {name} in {label} without its attributes and handle"
                    )));
                };
                if listed.len() == MAX_PATHS {
                    return Err(ReadFault::NotTheTree(format!(
                        "more than {MAX_PATHS} paths"
                    )));
                }
                listed.push(Listed { name, kind, handle });
            }
            if results.bool()? {
                return Ok(listed);
            }
            if listed.len() == before {
                return Err(ReadFault::Failed(format!(
                    "{front} answered {what} with a page of no entry that is not the last"
                )));
            }
        }
    }

    /// The bytes of the file `file`, whose path is `path`, up to [`MAX_FILE_BYTES`].
    fn read(&mut self, file: &[u8], path: &str) -> Result<Vec<u8>, ReadFault> {
        let what = format!("READ of {path}");
        let front = format!("the NFS front at {}", self.front);
        let mut bytes = Vec::new();
        loop {
            let offset = bytes.len() as u64;
            let args = Enc::default().opaque(file).u64(offset).u32(READ_COUNT);
            let answer = self.call(NFS_PROGRAM, READ, &args, &what)?;
            let mut results = Dec::new(&answer, &front, &what);
            status(&mut results, path, "read")?;
            results.post_op_attr()?;
            let _count = results.u32()?;
            let end = results.bool()?;
            let data = results.opaque()?;
            bytes.extend_from_slice(&data);
            if bytes.len() as u64 > MAX_FILE_BYTES {
                return Err(ReadFault::NotTheTree(format!(
                    "{path} is larger than {MAX_FILE_BYTES} bytes"
                )));
            }
            if end {
                return Ok(bytes);
            }
            if data.is_empty() {
                return Err(ReadFault::Failed(format!(
                    "{front} answered {what} with no bytes before the end of the file"
                )));
            }
        }
    }

    /// The target of the link `link`, whose path is `path`.
    fn readlink(&mut self, link: &[u8], path: &str) -> Result<String, ReadFault> {
        let what = format!("READLINK of {path}");
        let answer = self.call(NFS_PROGRAM, READLINK, &Enc::default().opaque(link), &what)?;
        let front = format!("the NFS front at {}", self.front);
        let mut results = Dec::new(&answer, &front, &what);
        status(&mut results, path, "read")?;
        results.post_op_attr()?;
        String::from_utf8(results.opaque()?).map_err(|_| {
            ReadFault::NotTheTree(format!("{path} links to a target that is not UTF-8"))
        })
    }
}

/// The bit of a record mark that says the fragment is the record's last.
const LAST: u32 = 1 << 31;

/// The status at the head of `results`: nothing for success; the path could not be `done`
/// otherwise, slowness for `NFS3ERR_JUKEBOX`.
fn status(results: &mut Dec<'_>, path: &str, done: &str) -> Result<(), ReadFault> {
    let code = results.u32()?;
    if code == 0 {
        return Ok(());
    }
    let said = format!(
        "{path} could not be {done}: {} answered {}",
        results.front,
        status_name(code)
    );
    if code == JUKEBOX {
        return Err(ReadFault::Unread(format!(
            "{said} (a fill of the file has not ended)"
        )));
    }
    Err(ReadFault::Failed(said))
}

/// The name of the status `code`, with its number.
fn status_name(code: u32) -> String {
    STATUSES
        .iter()
        .find(|(known, _)| *known == code)
        .map_or_else(
            || format!("status {code}"),
            |(_, name)| format!("{name} ({code})"),
        )
}

/// What a failed read or write of the connection, which `what` says, makes of the read: slowness
/// for a wait past the bound, a failure for anything else.
fn io_fault(what: &str, error: &std::io::Error) -> ReadFault {
    let said = format!("{what}: {error}");
    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
        ReadFault::Unread(said)
    } else {
        ReadFault::Failed(said)
    }
}

/// Words of XDR, big-endian and padded to four bytes.
#[derive(Default)]
pub(super) struct Enc(pub(super) Vec<u8>);

impl Enc {
    pub(super) fn u32(mut self, value: u32) -> Enc {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(super) fn u64(mut self, value: u64) -> Enc {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Variable-length opaque data: its length, the bytes, and the padding.
    pub(super) fn opaque(self, bytes: &[u8]) -> Enc {
        let mut enc = self.u32(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        enc.0.extend_from_slice(bytes);
        enc.0.resize(enc.0.len().next_multiple_of(4), 0);
        enc
    }

    /// Fixed-length opaque data (a cookie verifier).
    pub(super) fn fixed(mut self, bytes: &[u8]) -> Enc {
        self.0.extend_from_slice(bytes);
        self
    }
}

/// The cursor over an answer to a call, which says the call and the front in what it refuses.
struct Dec<'a> {
    data: &'a [u8],
    at: usize,
    front: &'a str,
    what: &'a str,
}

impl<'a> Dec<'a> {
    fn new(data: &'a [u8], front: &'a str, what: &'a str) -> Dec<'a> {
        Dec {
            data,
            at: 0,
            front,
            what,
        }
    }

    /// The next `count` bytes.
    fn take(&mut self, count: usize) -> Result<&'a [u8], ReadFault> {
        let bytes = self
            .at
            .checked_add(count)
            .and_then(|end| self.data.get(self.at..end))
            .ok_or_else(|| {
                ReadFault::Failed(format!(
                    "{} answered {} with an answer that ends early",
                    self.front, self.what
                ))
            })?;
        self.at += count;
        Ok(bytes)
    }

    fn fixed8(&mut self) -> Result<[u8; 8], ReadFault> {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(self.take(8)?);
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32, ReadFault> {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(self.take(4)?);
        Ok(u32::from_be_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, ReadFault> {
        Ok(u64::from_be_bytes(self.fixed8()?))
    }

    /// A `bool`: any word but 0 is true.
    fn bool(&mut self) -> Result<bool, ReadFault> {
        Ok(self.u32()? != 0)
    }

    /// Variable-length opaque data.
    fn opaque(&mut self) -> Result<Vec<u8>, ReadFault> {
        let length = self.u32()? as usize;
        let bytes = self.take(length)?.to_vec();
        self.take(length.next_multiple_of(4) - length)?;
        Ok(bytes)
    }

    /// An optional `fattr3`, skipped.
    fn post_op_attr(&mut self) -> Result<(), ReadFault> {
        self.post_op_kind().map(|_| ())
    }

    /// The kind of an optional `fattr3`, the rest of it skipped.
    fn post_op_kind(&mut self) -> Result<Option<u32>, ReadFault> {
        if !self.bool()? {
            return Ok(None);
        }
        let kind = self.u32()?;
        self.take(FATTR3 - 4)?;
        Ok(Some(kind))
    }
}

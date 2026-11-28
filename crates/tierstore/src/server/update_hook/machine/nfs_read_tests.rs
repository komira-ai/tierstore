//! The read of a tree from an NFS front (`nfs_read.rs`) against a front that answers from a script,
//! on a loopback socket of its own: every answer that ends the read, and how each is named. The
//! read against the release's own front is in `treefs/nfs/hook_read_tests.rs`.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

use super::nfs_read::{Enc, read_tree};
use super::provider::{MAX_FILE_BYTES, MAX_PATHS};
use crate::server::update_hook::{ReadFault, TreeNode};

/// The bound of every read here: a front of the script answers at once or never.
const BOUND: Duration = Duration::from_secs(30);

/// The bound of the read of a front that never answers.
const SHORT: Duration = Duration::from_millis(50);

/// The last-fragment bit of a record mark.
const LAST: u32 = 1 << 31;

/// What the front of a script answers to one call.
enum Says {
    /// An accepted reply to the call with these results.
    Results(Vec<u8>),
    /// A reply whose header is these three words (id, message type, state), and nothing more.
    Header(u32, u32, u32),
    /// An accepted reply whose accept state is this, and no results.
    NotTaken(u32),
    /// These bytes as they are, marks included.
    Raw(Vec<u8>),
    /// Nothing: the call is never answered.
    Nothing,
}

/// The id of the call in `record`.
fn xid_of(record: &[u8]) -> u32 {
    u32::from_be_bytes(record[..4].try_into().unwrap())
}

/// Reads the next call of `stream`, whole.
fn call_of(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut mark = [0u8; 4];
    stream.read_exact(&mut mark).ok()?;
    let mut record = vec![0u8; (u32::from_be_bytes(mark) & !LAST) as usize];
    stream.read_exact(&mut record).ok()?;
    Some(record)
}

/// `body` as one record.
fn record(body: &[u8]) -> Vec<u8> {
    let mut bytes = (LAST | u32::try_from(body.len()).unwrap())
        .to_be_bytes()
        .to_vec();
    bytes.extend_from_slice(body);
    bytes
}

/// A front that answers its first connection's calls as `script` says, in order; after the
/// script, or at [`Says::Nothing`], it reads what the connection sends until it closes.
fn front(script: Vec<Says>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        for says in script {
            let Some(call) = call_of(&mut stream) else {
                return;
            };
            let xid = xid_of(&call);
            let reply = match says {
                Says::Results(results) => {
                    let mut body = Enc::default().u32(xid).u32(1).u32(0).u32(0).u32(0).u32(0);
                    body.0.extend_from_slice(&results);
                    record(&body.0)
                }
                Says::Header(id, kind, state) => {
                    record(&Enc::default().u32(id).u32(kind).u32(state).0)
                }
                Says::NotTaken(state) => record(
                    &Enc::default()
                        .u32(xid)
                        .u32(1)
                        .u32(0)
                        .u32(0)
                        .u32(0)
                        .u32(state)
                        .0,
                ),
                Says::Raw(bytes) => bytes,
                Says::Nothing => break,
            };
            if stream.write_all(&reply).is_err() {
                return;
            }
        }
        while call_of(&mut stream).is_some() {}
    });
    address
}

/// `MNT` answered with the root's handle.
fn mounted() -> Says {
    Says::Results(Enc::default().u32(0).opaque(b"root").u32(0).0)
}

/// `LOOKUP` answered with `handle`.
fn found(handle: &[u8]) -> Says {
    Says::Results(Enc::default().u32(0).opaque(handle).u32(0).u32(0).0)
}

/// A status alone.
fn status(code: u32) -> Says {
    Says::Results(Enc::default().u32(code).0)
}

/// An entry of a listing: its name, its kind (none: no attributes), its handle (none: none given).
struct Entry<'a>(&'a [u8], Option<u32>, Option<&'a [u8]>);

/// A page of a listing of `entries`, the last when `end`.
fn page(entries: &[Entry<'_>], end: bool) -> Says {
    let mut enc = Enc::default().u32(0).u32(0).fixed(&[7; 8]);
    for (cookie, Entry(name, kind, handle)) in entries.iter().enumerate() {
        enc = enc.u32(1).u64(9).opaque(name).u64(cookie as u64 + 1);
        enc = match kind {
            Some(kind) => enc.u32(1).u32(*kind).fixed(&[0; 80]),
            None => enc.u32(0),
        };
        enc = match handle {
            Some(handle) => enc.u32(1).opaque(handle),
            None => enc.u32(0),
        };
    }
    Says::Results(enc.u32(0).u32(u32::from(end)).0)
}

/// The results of a `READ` answered with `data`, the end of the file when `end`; with attributes.
fn read_results(data: &[u8], end: bool) -> Vec<u8> {
    let enc = Enc::default().u32(0).u32(1).u32(1).fixed(&[0; 80]);
    let len = u32::try_from(data.len()).unwrap();
    enc.u32(len).u32(u32::from(end)).opaque(data).0
}

/// A `READ` answered with `data`, the end of the file when `end`.
fn bytes(data: &[u8], end: bool) -> Says {
    Says::Results(read_results(data, end))
}

/// A `READLINK` answered with `target`.
fn target(target: &[u8]) -> Says {
    Says::Results(Enc::default().u32(0).u32(0).opaque(target).0)
}

/// The read of `below` from `front`, within [`BOUND`].
fn read(front: SocketAddr, below: &str) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
    read_tree(front, Path::new(below), BOUND)
}

/// Asserts a read of a tree from a front that answers it in pages and pieces: the root's handle,
/// a LOOKUP of each name of the path, a listing in two pages (with `.` and `..`, which are not
/// entries), a file read in two READs and an empty one, a link's target, and a directory listed in
/// turn; the answer of the first READ in two fragments. Catches a read that stops at the first
/// page, the first READ or the first fragment, takes `.` for an entry, or follows the link.
#[test]
fn a_tree_is_read_from_the_front_in_pages_and_pieces() {
    // The sixth call, the first READ of `file`, is answered in two fragments.
    let fragmented = {
        let mut body = Enc::default().u32(6).u32(1).u32(0).u32(0).u32(0).u32(0).0;
        body.extend_from_slice(&read_results(b"ab", false));
        let (first, second) = body.split_at(10);
        let mut raw = u32::try_from(first.len()).unwrap().to_be_bytes().to_vec();
        raw.extend_from_slice(first);
        raw.extend_from_slice(&record(second));
        raw
    };
    let script = vec![
        mounted(),
        found(b"a"),
        found(b"b"),
        page(
            &[
                Entry(b".", None, None),
                Entry(b"..", Some(2), None),
                Entry(b"file", Some(1), Some(b"f")),
            ],
            false,
        ),
        page(
            &[
                Entry(b"link", Some(5), Some(b"l")),
                Entry(b"dir", Some(2), Some(b"d")),
                Entry(b"empty", Some(1), Some(b"e")),
            ],
            true,
        ),
        Says::Raw(fragmented),
        bytes(b"cd", true),
        target(b"file"),
        bytes(b"", true),
        page(&[Entry(b"inner", Some(1), Some(b"i"))], true),
        bytes(b"in", true),
    ];
    let read = read(front(script), "a/b").unwrap();
    assert_eq!(
        read,
        BTreeMap::from([
            ("dir".to_owned(), TreeNode::Dir),
            ("dir/inner".to_owned(), TreeNode::File(b"in".to_vec())),
            ("empty".to_owned(), TreeNode::File(Vec::new())),
            ("file".to_owned(), TreeNode::File(b"abcd".to_vec())),
            ("link".to_owned(), TreeNode::Link("file".to_owned())),
        ])
    );
}

/// Asserts that every status, refusal and broken answer of the front ends the read as a failure
/// that names the call, the path and the front: a refused MNT, a LOOKUP's status (by name, or by
/// number for a status of no name), a reply to another call, a call the front did not take, an
/// answer that ends early, one larger than the read takes, an entry listed without its attributes
/// or handle, a page of no entry that is not the last, and a READ of no bytes before the end of the
/// file. Catches a read that takes a broken answer for a tree, or defers on one, which would hold
/// the update until hostkeeper's deadline.
#[test]
fn every_refusal_and_broken_answer_of_the_front_is_a_failure_named() {
    let cases: Vec<(Vec<Says>, &str, String)> = vec![
        (
            vec![status(13)],
            "a",
            "refused MNT of /: NFS3ERR_ACCES (13)".to_owned(),
        ),
        (
            vec![mounted(), status(70)],
            "a",
            "a could not be looked up: the NFS front at {front} answered NFS3ERR_STALE (70)"
                .to_owned(),
        ),
        (
            vec![mounted(), status(99)],
            "a",
            "a could not be looked up: the NFS front at {front} answered status 99".to_owned(),
        ),
        (
            vec![Says::Header(77, 1, 0)],
            "a",
            "answered MNT of / with no accepted reply to it (id 77, message 1, state 0)".to_owned(),
        ),
        (
            vec![Says::Header(1, 1, 1)],
            "a",
            "answered MNT of / with no accepted reply to it (id 1, message 1, state 1)".to_owned(),
        ),
        (
            vec![Says::NotTaken(3)],
            "a",
            "did not take MNT of / (accept state 3)".to_owned(),
        ),
        (
            vec![Says::Header(1, 1, 0)],
            "a",
            "answered MNT of / with an answer that ends early".to_owned(),
        ),
        (
            vec![Says::Raw((LAST | (2 << 20)).to_be_bytes().to_vec())],
            "a",
            "answered MNT of / with more than 1048576 bytes".to_owned(),
        ),
        (
            vec![mounted(), page(&[Entry(b"x", None, Some(b"x"))], true)],
            "",
            "listed x in the directory without its attributes and handle".to_owned(),
        ),
        (
            vec![mounted(), page(&[Entry(b"x", Some(1), None)], true)],
            "",
            "listed x in the directory without its attributes and handle".to_owned(),
        ),
        (
            vec![mounted(), page(&[Entry(b".", Some(2), Some(b"."))], false)],
            "",
            "answered READDIRPLUS of the directory with a page of no entry that is not the last"
                .to_owned(),
        ),
        (
            vec![
                mounted(),
                page(&[Entry(b"f", Some(1), Some(b"f"))], true),
                bytes(b"", false),
            ],
            "",
            "answered READ of f with no bytes before the end of the file".to_owned(),
        ),
    ];
    for (script, below, said) in cases {
        let address = front(script);
        let said = said.replace("{front}", &address.to_string());
        let read = read(address, below);
        let Err(ReadFault::Failed(why)) = &read else {
            panic!("{said}: {read:?}");
        };
        assert!(why.ends_with(&said), "{why} does not end with {said}");
        assert!(why.contains(&address.to_string()), "{why}");
    }
}

/// Asserts that what the front serves is refused as no tree of the check, by name: a path that is
/// not names alone or not UTF-8, a name or a link target that is not UTF-8, an entry of no kind of
/// a tree (a block device), more than [`MAX_PATHS`] entries in one listing or in the tree, and a
/// file over [`MAX_FILE_BYTES`]. Catches a read that walks a huge tree or loads a large file whole.
#[test]
fn what_is_no_tree_of_the_check_is_refused_by_name() {
    let many: Vec<Vec<u8>> = (0..=MAX_PATHS)
        .map(|i| format!("n{i}").into_bytes())
        .collect();
    let crowd: Vec<Entry<'_>> = many
        .iter()
        .map(|name| Entry(name, Some(2), Some(b"d")))
        .collect();
    let half = MAX_PATHS / 2 + 1;
    let first: Vec<Entry<'_>> = many[..half]
        .iter()
        .map(|name| Entry(name, Some(2), Some(b"d")))
        .collect();
    let mut deep = vec![mounted(), page(&first, true)];
    // Each directory of the first page lists two more: the tree passes the bound before its end.
    for _ in 0..half {
        deep.push(page(
            &[
                Entry(b"x", Some(1), Some(b"x")),
                Entry(b"y", Some(1), Some(b"y")),
            ],
            true,
        ));
        deep.push(bytes(b"1", true));
        deep.push(bytes(b"2", true));
    }
    let large = vec![0u8; usize::try_from(MAX_FILE_BYTES).unwrap() / 2 + 1];
    let cases: Vec<(Vec<Says>, &str, String)> = vec![
        (
            vec![],
            "a/../b",
            "a/../b is no path below the mount".to_owned(),
        ),
        (
            vec![
                mounted(),
                page(&[Entry(b"\xff", Some(1), Some(b"f"))], true),
            ],
            "",
            "the directory holds a name that is not UTF-8".to_owned(),
        ),
        (
            vec![
                mounted(),
                page(&[Entry(b"l", Some(5), Some(b"l"))], true),
                target(b"\xff"),
            ],
            "",
            "l links to a target that is not UTF-8".to_owned(),
        ),
        (
            vec![mounted(), page(&[Entry(b"dev", Some(3), Some(b"v"))], true)],
            "",
            "dev is neither a file, a directory nor a link".to_owned(),
        ),
        (
            vec![mounted(), page(&crowd, true)],
            "",
            format!("more than {MAX_PATHS} paths"),
        ),
        (deep, "", format!("more than {MAX_PATHS} paths")),
        (
            vec![
                mounted(),
                page(&[Entry(b"big", Some(1), Some(b"b"))], true),
                bytes(&large, false),
                bytes(&large, false),
            ],
            "",
            format!("big is larger than {MAX_FILE_BYTES} bytes"),
        ),
    ];
    for (script, below, said) in cases {
        let read = read(front(script), below);
        assert_eq!(read, Err(ReadFault::NotTheTree(said)));
    }
}

/// Asserts that a path that is not UTF-8 is refused before anything is asked of the front.
#[test]
fn a_path_that_is_not_utf8_is_refused() {
    use std::os::unix::ffi::OsStrExt as _;
    let below = Path::new(std::ffi::OsStr::from_bytes(b"a/\xff"));
    let read = read_tree(front(vec![]), below, BOUND);
    assert_eq!(
        read,
        Err(ReadFault::NotTheTree(format!(
            "{} is not UTF-8",
            below.display()
        )))
    );
}

/// Asserts that slowness is slowness: a front that answers `NFS3ERR_JUKEBOX` (a fill that has not
/// ended) and a front that never answers within the bound end the read as unread, so that the
/// check defers; and that a front nobody listens on is a failure. Catches a check that fails, and
/// so rolls back, an update of a node whose front is only slow to fill a file.
#[test]
fn a_slow_front_is_unread_and_a_front_not_there_is_a_failure() {
    let jukebox = front(vec![mounted(), status(10_008)]);
    assert_eq!(
        read(jukebox, "a"),
        Err(ReadFault::Unread(format!(
            "a could not be looked up: the NFS front at {jukebox} answered NFS3ERR_JUKEBOX \
             (10008) (a fill of the file has not ended)"
        )))
    );

    let silent = front(vec![Says::Nothing]);
    let read_silent = read_tree(silent, Path::new("a"), SHORT);
    let Err(ReadFault::Unread(why)) = &read_silent else {
        panic!("{read_silent:?}");
    };
    assert!(
        why.starts_with(&format!(
            "the NFS front at {silent} did not answer MNT of /: "
        )),
        "{why}"
    );

    let gone = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let read_gone = read(gone, "a");
    let Err(ReadFault::Failed(why)) = &read_gone else {
        panic!("{read_gone:?}");
    };
    assert!(
        why.starts_with(&format!("the NFS front at {gone} could not be reached: ")),
        "{why}"
    );
}

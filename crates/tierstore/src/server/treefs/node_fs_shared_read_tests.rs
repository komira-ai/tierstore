//! Reads of handles that share one descriptor of a cache file (`core/blob.rs`), through
//! [`NodeFs::handle`] from one thread per handle, as the reader threads of the mount ask them:
//! each read gets the bytes at its own offset, whatever the other handles read at the same time.
//! The descriptor is shared, so a read that went through its file position, and not through the
//! offset of the request, would be given the bytes another handle moved it to.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use bytes::Bytes;

use super::*;
use crate::server::local_cache::trees::testing::{fs_at, open_image};
use crate::server::local_cache::trees::{ImageRequest, TreeStore, TreesConfig};
use crate::server::treefs::core::Core;
use crate::server::treefs::in_process::InProcess;
use crate::server::treefs::table::TreeTable;
use crate::{Backend as _, BlobSource, ListEntry, ListKind, Mode, TreeList};

/// How many handles read at once: the reader threads of the mount.
const READERS: usize = 16;

/// How many reads each thread makes.
const ROUNDS: usize = 400;

/// The byte at `at` of a file of the test, which tells where it is: a misplaced span of the file,
/// or a span of another file (`file`), differs from the right one.
fn pattern(file: u32, at: usize) -> u8 {
    let mixed = (at as u32).wrapping_add(file.wrapping_mul(0x85eb_ca6b));
    (mixed.wrapping_mul(0x9e37_79b1) >> 23) as u8
}

/// `len` bytes of the file `file`.
pub(super) fn content(file: u32, len: usize) -> Vec<u8> {
    (0..len).map(|at| pattern(file, at)).collect()
}

/// The next of a thread's numbers, from `state`: xorshift.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// A range to read in a file of `len` bytes: sizes from one byte to 128 KiB, mostly at an
/// offset inside the file, some across its end, and some past it.
fn range(state: &mut u64, len: usize) -> (u64, u32) {
    let size = match next(state) % 4 {
        0 => 1 + next(state) % 16,
        1 => 1 + next(state) % 4096,
        2 => 1 + next(state) % (128 * 1024),
        _ => 128 * 1024,
    } as usize;
    let offset = match next(state) % 8 {
        0 => len - (next(state) as usize % size.min(len)),
        1 => len + next(state) as usize % 64,
        _ => next(state) as usize % len,
    };
    (offset as u64, size as u32)
}

/// What a read of `size` bytes at `offset` gives of `bytes`: fewer at the end, none past it.
fn expected(bytes: &[u8], offset: u64, size: u32) -> &[u8] {
    let from = (offset as usize).min(bytes.len());
    &bytes[from..(from + size as usize).min(bytes.len())]
}

/// One reader: a handle `fh` of the file `node` whose bytes are `bytes`.
struct Reader<'a> {
    node: u64,
    fh: u64,
    bytes: &'a [u8],
}

/// Reads [`ROUNDS`] ranges through `fs` from one thread for each of `readers`, all started
/// together, and asserts that each reply is the bytes of its own range.
fn read_together(fs: &NodeFs, tokens: &Tokens, readers: &[Reader<'_>]) {
    let unique = AtomicU64::new(0);
    let start = Barrier::new(readers.len());
    std::thread::scope(|scope| {
        for (n, reader) in readers.iter().enumerate() {
            let (unique, start) = (&unique, &start);
            let _ = scope.spawn(move || {
                let device = Arc::new(Recorder::default());
                let mut state = 0x9e37_79b9_7f4a_7c15 ^ (n as u64 + 1).wrapping_mul(0xff51_afd7);
                start.wait();
                for round in 0..ROUNDS {
                    let (offset, size) = range(&mut state, reader.bytes.len());
                    let id = unique.fetch_add(1, Ordering::Relaxed) + 1;
                    let read = read_in(reader.fh, offset, size);
                    let bytes = request(op::READ, id, reader.node, &read);
                    let replier = tokens.register(id, device.clone());
                    let handled = fs.handle(Request::parse(&bytes).unwrap(), replier);
                    assert_eq!(handled, Handled::Done);
                    let replies = std::mem::take(&mut *device.0.lock().unwrap());
                    assert_eq!(replies.len(), 1);
                    assert_eq!(replies[0].error, 0);
                    assert!(
                        replies[0].body == expected(reader.bytes, offset, size),
                        "reader {n} round {round}: {size} bytes at {offset} are not the file's",
                    );
                }
            });
        }
    });
}

/// `count` opens of `node`, as handles.
fn open_all(wire: &Wire, node: u64, count: usize) -> Vec<u64> {
    (0..count)
        .map(|_| open_out(&wire.ask(op::OPEN, node, &open_in(0))).fh)
        .collect()
}

/// Asserts that two handles of one cache file of a few hundred KiB, and then sixteen, share one
/// descriptor of it, and that reads of them from one thread each, all at different offsets and
/// sizes at the same time (a byte to 128 KiB, across the end and past it), each give exactly the
/// bytes of the file at the request's own offset.
#[test]
fn handles_of_one_cache_file_read_their_own_offsets_at_once_on_the_shared_descriptor() {
    let bytes = content(1, 300_000 + 123);
    let built = Built::of(vec![("blob", file(&bytes))]);
    let wire = Wire::over("nodefs-shared-reads", built, false);
    let blob = node(1);
    for count in [2, READERS] {
        let handles = open_all(&wire, blob, count);
        assert_eq!(wire.rig.core.open_descriptors(), 1);
        let readers: Vec<Reader<'_>> = handles
            .iter()
            .map(|&fh| Reader {
                node: blob,
                fh,
                bytes: &bytes,
            })
            .collect();
        read_together(&wire.fs, &wire.tokens, &readers);
        for fh in handles {
            assert_eq!(wire.error(op::RELEASE, blob, &release_in(fh)), 0);
        }
        assert_eq!(wire.rig.core.open_descriptors(), 0);
    }
}

/// The key of the pack of [`pack_image`] in the node's upstream: an object the cache keeps.
const PACK_KEY: &str = "cas/pack/one";

/// The names of the files of the pack of [`pack_image`], their sizes, and where each begins in the
/// pack.
pub(super) const PACKED: [(&str, usize, u64); 4] = [
    ("a", 170_001, 4096),
    ("b", 90_000, 200_000),
    ("c", 262_144, 300_000),
    ("d", 5_000, 600_000),
];

/// The tree list of the files of [`PACKED`] as spans of the pack [`PACK_KEY`], and the pack's bytes:
/// each file's own content at its place and the bytes of no file between.
pub(super) fn pack_image() -> (Vec<u8>, Vec<u8>) {
    let mut pack = content(99, 700_000);
    let entries = PACKED
        .iter()
        .enumerate()
        .map(|(n, &(name, size, offset))| {
            let bytes = content(n as u32 + 10, size);
            pack[offset as usize..offset as usize + size].copy_from_slice(&bytes);
            ListEntry {
                path: name.to_owned(),
                kind: ListKind::File {
                    mode: Mode::new(0o644).unwrap(),
                    size: size as u64,
                    source: Some(BlobSource {
                        key: Arc::from(PACK_KEY),
                        offset,
                    }),
                    sha256: Some(TreeDigest::of(&bytes)),
                },
            }
        })
        .collect();
    (TreeList { entries }.encode_v2().unwrap(), pack)
}

/// A frontend whose core calls the node's tree store as well as its cache, with the tree list of
/// [`pack_image`] placed at `jobs` and its pack in the node's upstream: the files of the tree are
/// spans of the pack, which the cache fills when a file is first opened.
pub(super) fn placed_pack(name: &str) -> Wire {
    let (image, pack) = pack_image();
    let mut rig = Rig::new(name, Built::of(Vec::new()));
    let work = rig.work.handle();
    work.block_on(rig.upstream.put(PACK_KEY, Bytes::from(pack)))
        .unwrap();
    let _entered = work.enter();
    let trees = TreeStore::open(rig.node.clone(), TreesConfig::default()).unwrap();
    let bytes = Bytes::from(image);
    let request = ImageRequest {
        digest: TreeDigest::of(&bytes),
        image: open_image(bytes),
        at: vec![fs_at("jobs")],
        ttl: None,
        ready: false,
        staged: None,
    };
    let _ = trees.create_image(request).unwrap();
    let asks = Arc::new(InProcess::new(
        (rig.node.clone(), Some(trees)),
        rig.work.handle(),
        Duration::from_secs(30),
        Arc::new(|_| {}),
    ));
    let table = TreeTable::open(&rig.runtime).unwrap();
    let settings = rig.core.settings().clone();
    rig.core = Arc::new(Core::new(settings, asks, table, &rig.runtime));
    Wire::on(rig, name, false)
}

/// Asserts that the files of a placed tree that are spans of one pack, at different offsets of it,
/// share one descriptor of the pack, and that sixteen handles, four of each of four files, read
/// from one thread each at different offsets at the same time, each give the bytes of their own
/// file and no other.
#[test]
fn files_that_are_spans_of_one_pack_read_their_own_bytes_at_once_on_the_shared_descriptor() {
    let wire = placed_pack("nodefs-shared-pack");
    let root = entry_out(&wire.ask(op::LOOKUP, ino::ROOT, &cstr(b"jobs"))).nodeid;
    let files: Vec<(u64, Vec<u8>)> = PACKED
        .iter()
        .enumerate()
        .map(|(n, &(name, size, _))| {
            let found = entry_out(&wire.ask(op::LOOKUP, root, &cstr(name.as_bytes())));
            (found.nodeid, content(n as u32 + 10, size))
        })
        .collect();
    let opened: Vec<(u64, u64, &[u8])> = (0..READERS / files.len())
        .flat_map(|_| {
            files.iter().map(|(node, bytes)| {
                let fh = open_out(&wire.ask(op::OPEN, *node, &open_in(0))).fh;
                (*node, fh, bytes.as_slice())
            })
        })
        .collect();
    assert_eq!(wire.rig.core.open_descriptors(), 1);
    let readers: Vec<Reader<'_>> = opened
        .iter()
        .map(|&(node, fh, bytes)| Reader { node, fh, bytes })
        .collect();
    read_together(&wire.fs, &wire.tokens, &readers);
    for (node, fh, _) in opened {
        assert_eq!(wire.error(op::RELEASE, node, &release_in(fh)), 0);
    }
    assert_eq!(wire.rig.core.open_descriptors(), 0);
}

//! Kernel conformance, 5.2 items 1 (INIT), 2 (LOOKUP, GETATTR, READDIR, READDIRPLUS) and the live
//! half of 10 (the running kernel's protocol minor within what the pinned ABI supports; the
//! layout probe itself is `tests/abi_layout.rs` and `tests/abi_probe.rs`). Every mount is made by
//! the library's `Mounter`, initialised by `Session::init` with `InitConfig::tree()` or
//! `InitConfig::put()`, and served by the suite's `Model`.
//!
//! - INIT, item 1: the terms of a tree mount and of a put mount are exactly the flags asked for
//!   that the kernel offered (plus `INIT_EXT` if offered), `missing` is the rest, the minor is
//!   within `[MIN_KERNEL_MINOR, KERNEL_MINOR_VERSION]`, and the reply carries the configured
//!   limits and no request timeout (K22, K23). On a kernel of 6.14 or later the kernel offers every
//!   flag 5.2 requires. The kernel honours the reply: with `NO_OPEN_SUPPORT` an `ENOSYS` OPEN is
//!   the only OPEN (K18), with `NO_OPENDIR_SUPPORT` an `ENOSYS` OPENDIR is the only OPENDIR, and
//!   with `CACHE_SYMLINKS` a second readlink sends no READLINK.
//! - Item 2: a directory larger than one reply is listed whole, each name once with its inode and
//!   type, `.` and `..` included, over requests that resume inside the entries the one before sent
//!   (the kernel keeps only what fits the caller's buffer and asks again after the last kept
//!   entry, not after the last sent one) and end with an empty one at the end (the first a
//!   READDIRPLUS under `READDIRPLUS_AUTO`); the attributes the kernel reports are the
//!   tree's, and entries and attributes stay valid (a second stat sends no LOOKUP or GETATTR);
//!   after one READDIRPLUS listing a stat of each child sends no LOOKUP; a missing name is
//!   `ENOENT` after one LOOKUP, and names of 255 and 256 bytes both reach the server, which answers
//!   `ENOENT`: the kernel does not hold a FUSE name to the 255 bytes of `NAME_MAX`, so that limit,
//!   where a server wants it, is the server's to enforce.
//!
//! An integration target: it mounts through `mount(2)` as root (the farm) or `fusermount3` (CI),
//! and says on stderr why it does not run where neither can.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod conformance;
mod kernel;

use std::fs::File;
use std::io::{self, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::sync::Arc;

use conformance::{
    DirRead, Mounted, Open, Opendir, ROOT, Served, TreeSpec, answer_read, errno, is_root,
    kernel_at_least, memfd, op, pattern, pread, serve, tree_options,
};
use kernel::skip;
use rustix::fs::{Dir, FileType};
use tsfuse::abi::{
    EntryKind, InitConfig, InitFlags, KERNEL_MINOR_VERSION, MIN_KERNEL_MINOR, MountKind, Negotiated,
};
use tsfuse::mount::MountOptions;
use tsfuse::{Handled, ReaderEnd, ReplyConfig, ReplyPath, SPLICE_MIN};

fn say(line: &str) {
    let _ = writeln!(io::stderr(), "{line}");
}

/// A mount of `kind` (the tree's options and INIT terms, or the put directory's), its INIT terms
/// as the kernel and the library agreed them, and the file system serving `tree`.
fn start(
    name: &str,
    kind: MountKind,
    tree: Arc<TreeSpec>,
    open: Open,
    opendir: Opendir,
) -> Option<(Mounted, Negotiated, Served)> {
    let (options, config) = match kind {
        MountKind::Tree => (tree_options("tsfuse-conformance"), InitConfig::tree()),
        MountKind::Put => (
            MountOptions {
                allow_other: false,
                ..MountOptions::put("tsfuse-conformance")
            },
            InitConfig::put(),
        ),
    };
    let (mounted, session) = Mounted::with(name, &options, &config, 2, |m, mp, o| m.mount(mp, o))?;
    let negotiated = *session.negotiated();
    say(&format!(
        "{name}: the kernel offered {:?}",
        negotiated.offered
    ));
    let path = Arc::new(
        ReplyPath::new(ReplyConfig {
            splice_min: Some(SPLICE_MIN),
            max_data: 256 * 1024,
            pool: 1,
            move_pages: false,
        })
        .unwrap(),
    );
    let served = serve(
        session,
        tree,
        open,
        opendir,
        move |tree, read, replier, events| {
            answer_read(&path, tree, read, replier, events);
            Handled::Done
        },
    );
    Some((mounted, negotiated, served))
}

/// Unmounts and checks that every reader saw the unmount.
fn finish(mounted: Mounted, served: Served) {
    mounted.unmount();
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; 2]);
}

fn file_type(kind: EntryKind) -> FileType {
    match kind {
        EntryKind::Directory => FileType::Directory,
        EntryKind::File => FileType::RegularFile,
        EntryKind::Symlink => FileType::Symlink,
    }
}

/// The listing of `dir` by `getdents`: name, inode and type of every entry, in order.
fn list(dir: &Path) -> Vec<(Vec<u8>, u64, FileType)> {
    Dir::new(File::open(dir).unwrap())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_bytes().to_vec(),
                entry.ino(),
                entry.file_type(),
            )
        })
        .collect()
}

/// What the listing of directory `ino` of `tree` must be.
fn expected(tree: &TreeSpec, ino: u64) -> Vec<(Vec<u8>, u64, FileType)> {
    tree.entries(ino)
        .unwrap()
        .into_iter()
        .map(|(name, ino, kind)| (name, ino, file_type(kind)))
        .collect()
}

fn count(served: &Served, opcode: u32) -> usize {
    served.events.with(|s| s.count(opcode))
}

/// The requests that reached the file system for directory `ino`, in order.
fn dir_reads(served: &Served, ino: u64) -> Vec<DirRead> {
    served
        .events
        .with(|s| s.dirs.iter().filter(|d| d.ino == ino).copied().collect())
}

fn errno_of(result: io::Result<std::fs::Metadata>) -> i32 {
    result.unwrap_err().raw_os_error().unwrap()
}

#[test]
fn a_tree_mount_and_a_put_mount_are_granted_exactly_what_was_asked_and_offered_with_the_configured_limits()
 {
    for (kind, name, max_write) in [
        (MountKind::Tree, "tsfuse-init-tree", 4096),
        (MountKind::Put, "tsfuse-init-put", 1024 * 1024),
    ] {
        let Some((mounted, n, served)) = start(
            name,
            kind,
            Arc::new(TreeSpec::new()),
            Open::Enosys,
            Opendir::Enosys,
        ) else {
            return;
        };
        let required = kind.required();
        let mut granted = required.intersection(n.offered);
        if n.offered.contains(InitFlags::INIT_EXT) {
            granted = granted.union(InitFlags::INIT_EXT);
        }
        assert_eq!(
            n.granted, granted,
            "{name}: required and offered, and INIT_EXT"
        );
        assert_eq!(n.missing, required.difference(n.offered), "{name}: missing");
        assert!(
            (MIN_KERNEL_MINOR..=KERNEL_MINOR_VERSION).contains(&n.minor),
            "{name}: the kernel's minor {} is within the pinned ABI",
            n.minor
        );
        let r = n.reply;
        assert_eq!(
            (r.major, r.minor),
            (7, n.minor),
            "{name}: the version replied"
        );
        assert_eq!(
            u64::from(r.flags) | u64::from(r.flags2) << 32,
            n.granted.0,
            "{name}: the reply carries the flags granted"
        );
        assert!(
            r.max_readahead > 0 && r.max_readahead <= 128 * 1024,
            "{name}: readahead {} is within the 128 KiB asked",
            r.max_readahead
        );
        assert_eq!(r.max_write, max_write, "{name}: max_write");
        let pages = if n.granted.contains(InitFlags::MAX_PAGES) {
            256
        } else {
            0
        };
        assert_eq!(r.max_pages, pages, "{name}: max_pages");
        assert_eq!(
            (r.max_background, r.congestion_threshold),
            (1024, 768),
            "{name}: background limits"
        );
        assert_eq!(r.request_timeout, 0, "{name}: no request timeout is asked");
        finish(mounted, served);
    }
}

#[test]
fn a_kernel_of_6_14_or_later_offers_every_flag_the_tree_and_the_put_mount_require() {
    if !kernel_at_least(6, 14) {
        skip("the kernel is older than 6.14, which offers every required flag (5.2 item 1)");
        return;
    }
    for (kind, name) in [
        (MountKind::Tree, "tsfuse-init-offers-tree"),
        (MountKind::Put, "tsfuse-init-offers-put"),
    ] {
        let Some((mounted, n, served)) = start(
            name,
            kind,
            Arc::new(TreeSpec::new()),
            Open::Enosys,
            Opendir::Enosys,
        ) else {
            return;
        };
        assert_eq!(
            n.missing,
            InitFlags::EMPTY,
            "{name}: the kernel did not offer {:?}; it offered {:?}",
            n.missing,
            n.offered
        );
        assert!(n.granted.contains(kind.required()), "{name}: all granted");
        finish(mounted, served);
    }
}

#[test]
fn an_enosys_open_is_the_only_open_of_two_opened_and_read_files_and_no_release_is_sent() {
    let mut tree = TreeSpec::new();
    let data = pattern(3, 4096);
    tree.file(ROOT, "f", 4096, Some(memfd("open", &data)));
    let Some((mounted, n, served)) = start(
        "tsfuse-init-no-open",
        MountKind::Tree,
        Arc::new(tree),
        Open::Enosys,
        Opendir::Ok,
    ) else {
        return;
    };
    if !n.granted.contains(InitFlags::NO_OPEN_SUPPORT) {
        skip("the kernel did not offer NO_OPEN_SUPPORT");
        return;
    }
    for _ in 0..2 {
        let caller = pread(mounted.path("f"), 0, 16);
        assert_eq!(caller.bytes(), data[..16]);
        caller.join();
    }
    assert_eq!(
        count(&served, op::OPEN),
        1,
        "the first OPEN was answered ENOSYS"
    );
    assert_eq!(
        count(&served, op::RELEASE),
        0,
        "an open that was not sent is not released"
    );
    finish(mounted, served);
}

#[test]
fn an_enosys_opendir_is_the_only_opendir_of_two_listings_and_no_releasedir_is_sent() {
    let mut tree = TreeSpec::new();
    tree.file(ROOT, "f", 1, None);
    let tree = Arc::new(tree);
    let Some((mounted, n, served)) = start(
        "tsfuse-init-no-opendir",
        MountKind::Tree,
        tree.clone(),
        Open::Enosys,
        Opendir::Enosys,
    ) else {
        return;
    };
    if !n.granted.contains(InitFlags::NO_OPENDIR_SUPPORT) {
        skip("the kernel did not offer NO_OPENDIR_SUPPORT");
        return;
    }
    for _ in 0..2 {
        assert_eq!(list(&mounted.mp), expected(&tree, ROOT));
    }
    assert_eq!(
        count(&served, op::OPENDIR),
        1,
        "the first OPENDIR was answered ENOSYS"
    );
    assert_eq!(
        count(&served, op::RELEASEDIR),
        0,
        "an opendir that was not sent is not released"
    );
    finish(mounted, served);
}

#[test]
fn a_symlink_read_twice_sends_one_readlink_when_the_kernel_caches_symlinks() {
    let mut tree = TreeSpec::new();
    tree.symlink(ROOT, "l", "target/of/the/link");
    let Some((mounted, n, served)) = start(
        "tsfuse-init-symlinks",
        MountKind::Tree,
        Arc::new(tree),
        Open::Enosys,
        Opendir::Enosys,
    ) else {
        return;
    };
    if !n.granted.contains(InitFlags::CACHE_SYMLINKS) {
        skip("the kernel did not offer CACHE_SYMLINKS");
        return;
    }
    for _ in 0..2 {
        let target = std::fs::read_link(mounted.path("l")).unwrap();
        assert_eq!(target, Path::new("target/of/the/link"));
    }
    assert_eq!(
        count(&served, op::READLINK),
        1,
        "the second readlink came from the cache"
    );
    finish(mounted, served);
}

/// The name of the `i`th file of the big directory: 100 bytes.
fn long_name(i: usize) -> String {
    format!("{i:04}{}", "n".repeat(96))
}

#[test]
fn a_directory_larger_than_one_reply_is_listed_whole_over_requests_that_resume_after_the_entries_sent()
 {
    let mut tree = TreeSpec::new();
    for i in 0..300 {
        tree.file(ROOT, &long_name(i), i as u64, None);
    }
    for d in ["dir-a", "dir-b", "dir-c"] {
        tree.dir(ROOT, d);
    }
    for l in ["link-a", "link-b"] {
        tree.symlink(ROOT, l, "dir-a");
    }
    let tree = Arc::new(tree);
    let Some((mounted, n, served)) = start(
        "tsfuse-init-readdir",
        MountKind::Tree,
        tree.clone(),
        Open::Enosys,
        Opendir::Enosys,
    ) else {
        return;
    };
    let want = expected(&tree, ROOT);
    assert_eq!(want.len(), 2 + 300 + 3 + 2);
    assert_eq!(
        list(&mounted.mp),
        want,
        "every entry once, with its inode and type"
    );

    let reads = dir_reads(&served, ROOT);
    assert!(
        reads.len() > 1,
        "the listing needed more than one request: {reads:?}"
    );
    assert_eq!(
        reads[0].offset, 0,
        "the first request starts at the beginning"
    );
    assert_eq!(
        reads[0].plus,
        n.granted.contains(InitFlags::DO_READDIRPLUS),
        "the first request is a READDIRPLUS under READDIRPLUS_AUTO when it was granted"
    );
    // The kernel (K-fact found by this test, not in the design) keeps only the entries that fit
    // the caller's getdents buffer and asks again at the offset of the last one it kept, so a
    // request resumes after at least one and at most all of the entries the one before sent.
    for pair in reads.windows(2) {
        let (before, after) = (pair[0], pair[1]);
        assert!(
            before.offset < after.offset && after.offset <= before.offset + before.sent as u64,
            "a request resumes within the entries the last one sent: {reads:?}"
        );
    }
    let last = reads.last().unwrap();
    assert_eq!(
        (last.offset, last.sent),
        (want.len() as u64, 0),
        "the listing ends with a request at the end that sends nothing: {reads:?}"
    );
    finish(mounted, served);
}

#[test]
fn stat_of_the_root_a_directory_a_file_and_a_symlink_equals_the_tree_and_a_second_stat_asks_nothing()
 {
    let mut tree = TreeSpec::new();
    let sub = tree.dir(ROOT, "sub");
    tree.file(sub, "inner", 7, None);
    tree.dir(sub, "deeper");
    tree.file(ROOT, "f", 12_345, None);
    tree.symlink(ROOT, "l", "f");
    let tree = Arc::new(tree);
    let Some((mounted, _, served)) = start(
        "tsfuse-init-stat",
        MountKind::Tree,
        tree.clone(),
        Open::Enosys,
        Opendir::Enosys,
    ) else {
        return;
    };
    let nodes = [("", ROOT), ("sub", 2), ("f", 5), ("l", 6)];
    let stat_all = || {
        for (rel, ino) in nodes {
            let m = std::fs::symlink_metadata(mounted.path(rel)).unwrap();
            let a = tree.attr(ino).unwrap();
            let got = (
                m.mode(),
                m.size(),
                m.nlink(),
                m.ino(),
                m.uid(),
                m.gid(),
                m.mtime(),
            );
            let want = (
                a.mode,
                a.size,
                u64::from(a.nlink),
                a.ino,
                a.uid,
                a.gid,
                a.mtime as i64,
            );
            assert_eq!(got, want, "attributes of {rel:?}");
        }
    };
    stat_all();
    let asked = |served: &Served| (count(served, op::LOOKUP), count(served, op::GETATTR));
    let first = asked(&served);
    assert!(
        first.0 >= 3,
        "the first stats looked the names up: {first:?}"
    );
    stat_all();
    assert_eq!(
        asked(&served),
        first,
        "entries and attributes are still valid"
    );
    finish(mounted, served);
}

#[test]
fn after_a_readdirplus_listing_a_stat_of_each_child_sends_no_lookup() {
    let mut tree = TreeSpec::new();
    let sub = tree.dir(ROOT, "sub");
    let kids = [
        tree.file(sub, "a", 1, None),
        tree.file(sub, "b", 22, None),
        tree.dir(sub, "c"),
        tree.symlink(sub, "d", "a"),
    ];
    let tree = Arc::new(tree);
    let Some((mounted, n, served)) = start(
        "tsfuse-init-readdirplus",
        MountKind::Tree,
        tree.clone(),
        Open::Enosys,
        Opendir::Enosys,
    ) else {
        return;
    };
    if !n.granted.contains(InitFlags::DO_READDIRPLUS) {
        skip("the kernel did not offer DO_READDIRPLUS");
        return;
    }
    let dir = mounted.path("sub");
    assert_eq!(list(&dir), expected(&tree, sub));
    let reads = dir_reads(&served, sub);
    assert!(
        reads[0].plus,
        "the listing began with a READDIRPLUS: {reads:?}"
    );
    let lookups = count(&served, op::LOOKUP);
    for (name, ino) in ["a", "b", "c", "d"].into_iter().zip(kids) {
        let m = std::fs::symlink_metadata(dir.join(name)).unwrap();
        let a = tree.attr(ino).unwrap();
        assert_eq!(
            (m.ino(), m.mode(), m.size()),
            (a.ino, a.mode, a.size),
            "{name}"
        );
    }
    assert_eq!(
        count(&served, op::LOOKUP),
        lookups,
        "the entries came with the READDIRPLUS"
    );
    finish(mounted, served);
}

#[test]
fn a_missing_name_is_enoent_after_one_lookup_and_a_name_of_256_bytes_reaches_the_server_which_decides()
 {
    let mut tree = TreeSpec::new();
    let sub = tree.dir(ROOT, "sub");
    tree.file(sub, "inner", 1, None);
    let Some((mounted, _, served)) = start(
        "tsfuse-init-missing",
        MountKind::Tree,
        Arc::new(tree),
        Open::Enosys,
        Opendir::Enosys,
    ) else {
        return;
    };
    let stat = |rel: &str| std::fs::symlink_metadata(mounted.path(rel));
    let lookups = || count(&served, op::LOOKUP);

    let before = lookups();
    assert_eq!(errno_of(stat("absent")), errno::ENOENT);
    assert_eq!(
        lookups() - before,
        1,
        "one LOOKUP for a name missing in the root"
    );

    stat("sub").unwrap();
    let before = lookups();
    assert_eq!(errno_of(stat("sub/absent")), errno::ENOENT);
    assert_eq!(
        lookups() - before,
        1,
        "one LOOKUP for a name missing in a subdirectory"
    );

    let before = lookups();
    assert_eq!(errno_of(stat(&"x".repeat(255))), errno::ENOENT);
    assert_eq!(
        lookups() - before,
        1,
        "a name of 255 bytes reaches the server"
    );

    let before = lookups();
    assert_eq!(errno_of(stat(&"x".repeat(256))), errno::ENOENT);
    assert_eq!(
        lookups() - before,
        1,
        "a name of 256 bytes is not refused by the kernel: the server decides"
    );

    if !is_root() {
        say("tsfuse-init-missing: ran as an unprivileged user (fusermount3)");
    }
    finish(mounted, served);
}

//! `NodeFs` over a real mount: what a process sees through `std::fs`, and what reaches the core.
//! A mount needs `/dev/fuse` and `fusermount3` (the mounting user is not root); where either is
//! missing the test says so on stderr and ends.

use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use tsfuse::abi::InitFlags;

use super::RUN;
use crate::server::treefs::backing::{FILE, Saved};
use crate::server::treefs::core::FIXED_MTIME;
use crate::server::treefs::ino;
use crate::server::treefs::serving::Serving;
use crate::server::treefs::testing::{Built, FakeKernel, Rig, file, sample, sha, with_fuse};
use crate::tree_manifest::TreeDigest;

/// A rig, and its core served on a mount of its mountpoint.
struct Mounted {
    /// Ended first: the mount goes before the directories do.
    serving: Option<Serving>,
    rig: Rig,
    root: String,
}

/// The mount of `built`, attached for one run, with the owner of the entries the test's user.
fn mounted(name: &str, built: Built) -> Mounted {
    let me = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    let rig = Rig::owned(name, built, me);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    let serving = Serving::mount(&rig.core, &rig.settings()).unwrap();
    Mounted {
        serving: Some(serving),
        rig,
        root,
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(serving) = self.serving.take() {
            let _ = serving.unmount();
        }
    }
}

/// Runs `test` over the mount of the sample tree where a mount can be made.
fn with_mount(name: &str, test: impl FnOnce(&Mounted)) {
    with_fuse(|| test(&mounted(name, sample())));
}

impl Mounted {
    /// The path of `name` in the tree.
    fn at(&self, name: &str) -> std::path::PathBuf {
        self.rig
            .mountpoint
            .join("trees")
            .join(&self.root)
            .join(name)
    }

    /// What was served, by kind.
    fn served(&self, what: &str) -> usize {
        self.rig
            .core
            .served
            .lock()
            .unwrap()
            .iter()
            .filter(|got| **got == what)
            .count()
    }
}

fn errno_of(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap()
}

const EACCES: i32 = 13;
const ENOENT: i32 = 2;

/// Asserts that a tree reads as the manifests say through a real mount: the names, the contents of
/// a small and a large file, an empty file, a link, the modes, sizes, owner, link counts and the
/// fixed modification time.
#[test]
fn a_tree_reads_through_the_mount_as_its_manifests_say() {
    with_mount("tfs-fuse-read", |m| {
        let mut names: Vec<String> = std::fs::read_dir(m.at(""))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["big.bin", "bin", "empty.h", "include", "lib"]);
        let built = m.rig.upstream.built.lock().unwrap().blobs.clone();
        let big = std::fs::read(m.at("big.bin")).unwrap();
        assert_eq!(big, built[&sha(&big)]);
        assert_eq!(big.len(), 300_000);
        assert_eq!(std::fs::read(m.at("include/a.h")).unwrap(), b"int a;");
        assert_eq!(std::fs::read(m.at("bin/tool")).unwrap(), b"tool bytes");
        assert!(std::fs::read(m.at("empty.h")).unwrap().is_empty());
        assert_eq!(std::fs::read_link(m.at("lib")).unwrap(), Path::new("bin"));
        assert_eq!(std::fs::read(m.at("lib/tool")).unwrap(), b"tool bytes");
        let me = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        let cc = std::fs::metadata(m.at("bin/cc")).unwrap();
        assert_eq!(
            (cc.mode() & 0o7777, cc.size(), cc.nlink()),
            (0o755, 3000, 1)
        );
        assert_eq!((cc.uid(), cc.gid()), me);
        assert_eq!(cc.mtime(), FIXED_MTIME as i64);
        assert_eq!(
            std::fs::metadata(m.at("big.bin")).unwrap().mode() & 0o7777,
            0o644
        );
        let bin = std::fs::metadata(m.at("bin")).unwrap();
        assert!(bin.is_dir());
        assert_eq!((bin.mode() & 0o7777, bin.nlink()), (0o755, 2));
        assert_eq!(std::fs::metadata(m.at("")).unwrap().nlink(), 4);
        let link = std::fs::symlink_metadata(m.at("lib")).unwrap();
        assert!(link.file_type().is_symlink());
        assert_eq!((link.mode() & 0o777, link.size()), (0o777, 3));
        assert_eq!(
            std::fs::metadata(m.at("big.bin")).unwrap().ino(),
            ino::of(1, 1)
        );
    });
}

/// Asserts the namespace through the mount: the root and `trees` can be passed through and not
/// listed (`EACCES`), `views` and a digest the upstream has no tree of are not there, the answers
/// about the namespace are not kept by the kernel (a second miss is a second request), and a tree
/// the upstream gains after a miss is found by the next lookup, which attaches it with no run.
#[test]
fn the_namespace_can_be_passed_through_and_not_listed_and_a_tree_resolves_at_the_next_lookup() {
    with_mount("tfs-fuse-namespace", |m| {
        let mount = &m.rig.mountpoint;
        assert_eq!(errno_of(&std::fs::read_dir(mount).unwrap_err()), EACCES);
        assert_eq!(
            errno_of(&std::fs::read_dir(mount.join("trees")).unwrap_err()),
            EACCES
        );
        assert_eq!(
            std::fs::metadata(mount.join("trees")).unwrap().mode() & 0o7777,
            0o711
        );
        assert_eq!(std::fs::metadata(mount).unwrap().mode() & 0o7777, 0o711);
        assert_eq!(
            errno_of(&std::fs::metadata(mount.join("views")).unwrap_err()),
            ENOENT
        );
        let other = sha(b"a tree nobody has attached yet");
        let later = mount.join("trees").join(&other);
        let before = m.served("namespace");
        assert_eq!(errno_of(&std::fs::metadata(&later).unwrap_err()), ENOENT);
        let one = m.served("namespace") - before;
        assert!(one >= 1);
        assert_eq!(errno_of(&std::fs::metadata(&later).unwrap_err()), ENOENT);
        assert_eq!(m.served("namespace") - before, 2 * one);
        let tree = Built::of(vec![("only", file(b"later"))]);
        let digest = tree.root.clone();
        {
            let mut built = m.rig.upstream.built.lock().unwrap();
            built.manifests.extend(tree.manifests);
            built.blobs.extend(tree.blobs);
        }
        let there = mount.join("trees").join(&digest);
        assert_eq!(std::fs::read(there.join("only")).unwrap(), b"later");
        assert_eq!(m.rig.core.implicit().len(), 1);
    });
}

/// Asserts that what is below a tree's root is kept by the kernel: a name that is there, a name
/// that is not (a negative entry), a link's target and the bytes of a file read twice cost the
/// core one request each, and the second open of a file is a request and its read is not.
#[test]
fn what_is_below_a_tree_root_is_asked_of_the_core_once() {
    with_mount("tfs-fuse-cache", |m| {
        let _ = std::fs::metadata(m.at("bin/cc")).unwrap();
        let _ = std::fs::metadata(m.at("bin/nothing.h")).unwrap_err();
        let _ = std::fs::read_link(m.at("lib")).unwrap();
        let (lookups, links) = (m.served("lookup"), m.served("readlink"));
        assert_eq!(links, 1);
        for _ in 0..5 {
            let _ = std::fs::metadata(m.at("bin/cc")).unwrap();
            assert_eq!(
                errno_of(&std::fs::metadata(m.at("bin/nothing.h")).unwrap_err()),
                ENOENT
            );
            let _ = std::fs::read_link(m.at("lib")).unwrap();
        }
        assert_eq!(m.served("lookup"), lookups);
        assert_eq!(m.served("readlink"), links);
        let first = std::fs::read(m.at("bin/cc")).unwrap();
        let second = std::fs::read(m.at("bin/cc")).unwrap();
        assert_eq!(first, second);
        assert_eq!(m.served("open"), 2);
        assert_eq!(m.served("read"), 1);
    });
}

/// Asserts that a listing is made with entries (`READDIRPLUS`): the core counts a lookup for each
/// entry the kernel was sent, and the listing needs no lookup of the names after it.
#[test]
fn a_listing_is_made_with_entries() {
    with_mount("tfs-fuse-plus", |m| {
        let before = m.served("lookup");
        let count = std::fs::read_dir(m.at("")).unwrap().count();
        assert_eq!(count, 5);
        assert_eq!(m.served("count"), 5);
        assert_eq!(m.served("lookup"), before);
        let _ = std::fs::metadata(m.at("bin")).unwrap();
        assert_eq!(m.served("lookup"), before);
    });
}

/// Asserts that the mount is a FUSE file system named for tierstore with the kernel checking the
/// modes.
#[test]
fn the_mount_is_named_and_checked_by_the_kernel() {
    with_mount("tfs-fuse-info", |m| {
        let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        let line = info
            .lines()
            .find(|line| line.contains(m.rig.mountpoint.to_str().unwrap()))
            .unwrap();
        assert!(line.contains("fuse.tierstore-fs"), "{line}");
        assert!(line.contains("default_permissions"), "{line}");
        assert!(!line.contains("allow_other"), "{line}");
    });
}

/// Asserts that every request that would change a tree is `EROFS`, whether the file is opened for
/// writing, created, made, removed, renamed, linked, changed or given an attribute, and that the
/// extended attributes are not there (`EOPNOTSUPP`).
#[test]
fn every_request_that_would_change_a_tree_is_read_only() {
    with_mount("tfs-fuse-erofs", |m| {
        let rofs = rustix::io::Errno::ROFS.raw_os_error();
        let file = m.at("include/a.h");
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap_err();
        assert_eq!(errno_of(&opened), rofs);
        let appended = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap_err();
        assert_eq!(errno_of(&appended), rofs);
        let created = std::fs::File::create_new(m.at("include/new.h")).unwrap_err();
        assert_eq!(errno_of(&created), rofs);
        assert_eq!(
            errno_of(&std::fs::create_dir(m.at("include/d")).unwrap_err()),
            rofs
        );
        assert_eq!(errno_of(&std::fs::remove_file(&file).unwrap_err()), rofs);
        assert_eq!(
            errno_of(&std::fs::remove_dir(m.at("include")).unwrap_err()),
            rofs
        );
        assert_eq!(
            errno_of(&std::fs::rename(&file, m.at("include/b.h")).unwrap_err()),
            rofs
        );
        assert_eq!(
            errno_of(&std::fs::hard_link(&file, m.at("include/c.h")).unwrap_err()),
            rofs
        );
        let linked = std::os::unix::fs::symlink("x", m.at("include/l")).unwrap_err();
        assert_eq!(errno_of(&linked), rofs);
        let chmod = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        assert_eq!(errno_of(&chmod.unwrap_err()), rofs);
        let node = rustix::fs::mknodat(
            rustix::fs::CWD,
            m.at("include/n"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        );
        assert_eq!(node.unwrap_err().raw_os_error(), rofs);
        let flags = rustix::fs::XattrFlags::empty();
        let set = rustix::fs::setxattr(&file, "user.x", b"v", flags);
        assert_eq!(set.unwrap_err().raw_os_error(), rofs);
        let remove = rustix::fs::removexattr(&file, "user.x");
        assert_eq!(remove.unwrap_err().raw_os_error(), rofs);
        let unsupported = rustix::io::Errno::OPNOTSUPP.raw_os_error();
        let get = rustix::fs::getxattr(&file, "user.x", &mut [0u8; 8]);
        assert_eq!(get.unwrap_err().raw_os_error(), unsupported);
        let list = rustix::fs::listxattr(&file, &mut [0u8; 8]);
        assert_eq!(list.unwrap_err().raw_os_error(), unsupported);
        assert_eq!(std::fs::read(&file).unwrap(), b"int a;");
    });
}

/// Asserts that a file whose fill fails cannot be opened (`EIO`), that the failure is on the
/// attachment, and that the file opens once the blob can be had; and that a directory cannot be
/// read as a file.
#[test]
fn a_file_whose_fill_fails_is_an_io_error_until_the_blob_can_be_had() {
    with_mount("tfs-fuse-eio", |m| {
        let a = sha(b"int a;");
        let _ = m
            .rig
            .upstream
            .failing
            .lock()
            .unwrap()
            .insert(a, "upstream 503".to_owned());
        let error = std::fs::File::open(m.at("include/a.h")).unwrap_err();
        assert_eq!(errno_of(&error), rustix::io::Errno::IO.raw_os_error());
        assert_eq!(std::fs::read(m.at("bin/tool")).unwrap(), b"tool bytes");
        m.rig.upstream.failing.lock().unwrap().clear();
        let mut text = String::new();
        let _ = std::fs::File::open(m.at("include/a.h"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "int a;");
        let directory = std::fs::read(m.at("bin")).unwrap_err();
        assert_eq!(
            errno_of(&directory),
            rustix::io::Errno::ISDIR.raw_os_error()
        );
        let failures = m.rig.core.detach(RUN).unwrap().failures;
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].path, "/include/a.h");
        assert_eq!(failures[0].reason, "upstream 503");
    });
}

/// Asserts that a name that is not text is not there.
#[test]
fn a_name_that_is_not_text_is_not_there() {
    use std::os::unix::ffi::OsStrExt as _;
    with_mount("tfs-fuse-bytes", |m| {
        let odd = m.at("").join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
        assert_eq!(errno_of(&std::fs::metadata(odd).unwrap_err()), ENOENT);
    });
}

/// Asserts that a directory too large for one reply is listed whole: the first reply is made with
/// entries and the next ones are plain listings.
#[test]
fn a_directory_too_large_for_one_reply_is_listed_whole() {
    with_fuse(|| {
        let names: Vec<String> = (0..3000).map(|i| format!("f{i:05}")).collect();
        let built = Built::of(
            names
                .iter()
                .map(|name| (name.as_str(), file(b"")))
                .collect(),
        );
        let m = mounted("tfs-fuse-large", built);
        let mut listed: Vec<String> = std::fs::read_dir(m.at(""))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        listed.sort();
        assert_eq!(listed.len(), 3000);
        assert_eq!(listed[2999], "f02999");
        assert!(m.served("readdir") > 1);
    });
}

/// Asserts that a mount never asks the kernel for passthrough, whatever the kernel offers, and so
/// leaves the stack depth 0, on which the job's overlay stacks; and that every open is a cached
/// one: the reads of a file reach the core.
#[test]
fn a_mount_does_not_ask_for_passthrough_and_its_reads_reach_the_core() {
    with_mount("tfs-fuse-no-passthrough", |m| {
        let outcome = m.serving.as_ref().unwrap().outcome();
        assert!(!outcome.flags.contains(InitFlags::PASSTHROUGH));
        assert_eq!(outcome.max_stack_depth, 0);
        let blobs = m.rig.upstream.built.lock().unwrap().blobs.clone();
        let bytes = std::fs::read(m.at("bin/cc")).unwrap();
        assert_eq!(bytes, blobs[&sha(&bytes)]);
        assert!(m.served("read") > 0);
        assert!(!m.rig.runtime.join(FILE).exists());
    });
}

/// Asserts that an open of a blob the core holds an adopted id for, whose handle is not one the
/// frontend can answer with (here the test kernel's, not a [`BackingId`]), fails with `EIO` and
/// ends the open the core made, rather than answering a cached open on an inode the kernel serves
/// by passthrough; and that the opens of other files are cached opens as before.
#[test]
fn an_adopted_id_the_frontend_cannot_answer_with_fails_the_open() {
    with_fuse(|| {
        let m = mounted("tfs-fuse-adopted", sample());
        let cc: Vec<u8> = (0..3000u32).map(|i| (i % 241) as u8).collect();
        let saved = Saved {
            blob: TreeDigest::from_hex(&sha(&cc)).unwrap(),
            id: 9,
            opens: 1,
            exact: false,
        };
        let kernel = FakeKernel::new();
        m.rig
            .core
            .adopt_backing(&m.rig.runtime.join(FILE), &[saved], &|id| kernel.wrap(id));
        let refused = std::fs::File::open(m.at("bin/cc")).unwrap_err();
        assert_eq!(errno_of(&refused), rustix::io::Errno::IO.raw_os_error());
        assert_eq!(m.rig.core.open_files(), 0);
        assert_eq!(std::fs::read(m.at("bin/tool")).unwrap(), b"tool bytes");
    });
}

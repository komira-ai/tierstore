//! The mount, the unmount and the abort against the kernel (5.2 item 8, and the ported
//! `fuser-tests` cases "mount/unmount, kill-then-unmount, `allow_other`, `fusermount3` path, mount
//! table"), through the library: [`Mounter`], [`Session::init`], cloned readers,
//! [`Session::serve`] with the suite's `Model` as the `Fs`, and [`Mount`].
//!
//! - **Mount table.** A tree mount is in `/proc/self/mountinfo` with its type, its `fsname` as
//!   source, `ro,nosuid,nodev`, and `user_id`, `group_id` and `default_permissions` as super
//!   block options, no `allow_other`; a put mount is `rw`. After the unmount the entry is gone and
//!   every reader ended `Unmounted`.
//! - **Connection directory.** The mount's [`ConnectionId`](tsfuse::mount::ConnectionId) names a
//!   directory of the FUSE control file system while mounted and none after the unmount.
//! - **Unmount while busy.** With a file of the mount held open, an unmount fails and leaves the
//!   mount: `EBUSY` from `umount2` (`MountError::Unmount`), a failed helper
//!   (`MountError::Helper`) through `fusermount3`; a lazy unmount then detaches it, the held file
//!   still reads its bytes through the live session, and once it is closed every reader ends
//!   `Unmounted`.
//! - **Abort** (root: the `abort` file is root's). With a READ parked, `Mount::abort` fails the
//!   blocked caller with `ECONNABORTED` (K5: the kernel ends the requests of a closed or aborted
//!   connection with `ECONNABORTED`), ends every reader, leaves the mount in the table until it
//!   is unmounted, and a late completion of the parked replier is a device error. 5.2 item 8
//!   expects the readers to end `Aborted`; the farm kernel (7.0) ends a read after a write of the
//!   `abort` file with `ENODEV`, which the library reports as `Unmounted`, so the check asserts
//!   that every reader ended by the kernel's end of the connection (`Unmounted` or `Aborted`)
//!   and not by a stop.
//! - **Kill, then unmount.** A child process (this binary) mounts and serves a tree; killed with
//!   `SIGKILL`, every descriptor of the connection closes with it (K5), so a synchronous `statx`
//!   of the mount fails `ENOTCONN`, the mount stays in the table, and `Mount::adopt` of it
//!   unmounts it.
//! - **`allow_other`** (root: the check changes one thread's user). With it, another user
//!   (`nobody`, 65534) can `fstat` a descriptor of the mount's root that root opened beforehand
//!   (no path walk through a scratch directory that user may not search) and `openat` a file of
//!   it; without it both fail `EACCES` (`fuse_allow_current_process`).
//! - **`fusermount3` path.** Forced even as root: the method is `Fusermount`, the tree is served,
//!   an unmount through `fusermount3 -u` removes the entry, and a lazy unmount through
//!   `fusermount3 -u -z` detaches a busy mount.
//! - **A mount that is gone.** `Mount::adopt` on the unmounted directory: `abort` is an error that
//!   names the `abort` file and writes nowhere (it creates nothing); `connection` reads the device
//!   number of whatever is mounted there now, which is not the old connection's.
//!
//! Every wait is on an event (a request parked, a caller's system call returning, a reader
//! joining, a child ending or writing a line), never on a timer. An integration target: it mounts
//! through `mount(2)` as root (the farm) or `fusermount3` (CI), and says on stderr why it does not
//! run where neither can.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod conformance;
mod kernel;

use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;

use conformance::{
    Done, FOPEN_DIRECT_IO, Mounted, Open, Opendir, ROOT, TreeSpec, answer_read, child, errno,
    is_root, memfd, mount_entry, park, pattern, pread, serve, tree_options,
};
use kernel::skip;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, StatxFlags};
use tierstore_testkit::dir::TestDir;
use tsfuse::abi::InitConfig;
use tsfuse::mount::{
    CONNECTIONS, FUSERMOUNT, Mount, MountError, MountMethod, MountOptions, Mounter,
};
use tsfuse::{Handled, ReaderEnd, ReplyConfig, ReplyPath};

/// The size of the file `f` of the tree, and the readers every mount here has.
const SIZE: usize = 4096;
const READERS: usize = 2;

/// The root and a file `f` of [`SIZE`] bytes, and those bytes.
fn tree() -> (Arc<TreeSpec>, Vec<u8>) {
    let bytes = pattern(7, SIZE);
    let mut tree = TreeSpec::new();
    tree.file(ROOT, "f", SIZE as u64, Some(memfd("f", &bytes)));
    (Arc::new(tree), bytes)
}

/// Serves `session` with every READ answered inline through a reply path.
fn serve_inline(session: tsfuse::Session, tree: Arc<TreeSpec>) -> conformance::Served {
    let path = ReplyPath::new(ReplyConfig::for_session(session.negotiated(), READERS)).unwrap();
    serve(
        session,
        tree,
        Open::Flags(FOPEN_DIRECT_IO),
        Opendir::Ok,
        move |tree, read, replier, events| {
            answer_read(&path, tree, read, replier, events);
            Handled::Done
        },
    )
}

/// Whether every READ answered so far was accepted by the kernel.
fn all_written(events: &conformance::Events) -> bool {
    events.with(|s| {
        s.completions
            .iter()
            .all(|(_, done, _)| *done == Done::Written)
    })
}

/// Asserts that every reader ended because the kernel ended the connection. A mount detached
/// lazily goes when its last file closes, and the kernel's end of a read then is `ENODEV`
/// (`Unmounted`) on one reader and `ECONNABORTED` (`Aborted`) on another, from run to run on the
/// farm kernel (7.0): either is the connection ending, neither a stop or a hand-over.
fn ended_by_the_kernel(ends: &[ReaderEnd]) {
    assert_eq!(ends.len(), READERS);
    for end in ends {
        assert!(
            matches!(end, ReaderEnd::Unmounted | ReaderEnd::Aborted),
            "{ends:?}"
        );
    }
}

/// The bytes of `file` at 0, one `pread`.
fn read_all(file: &File) -> Vec<u8> {
    let mut buf = vec![0u8; SIZE];
    let n = file.read_at(&mut buf, 0).unwrap();
    buf.truncate(n);
    buf
}

#[test]
fn a_tree_mount_is_in_the_mount_table_with_its_options_and_its_unmount_removes_the_entry() {
    let Some((mounted, session)) = Mounted::new("t-f7-table", READERS) else {
        return;
    };
    let (tree, bytes) = tree();
    let served = serve_inline(session, tree);

    let options = tree_options("tsfuse-conformance");
    let entry = mounted
        .entry()
        .expect("the mount is in /proc/self/mountinfo");
    assert_eq!(entry.fstype, options.fstype());
    assert_eq!(entry.fstype, "fuse.tsfuse");
    assert_eq!(entry.source, "tsfuse-conformance");
    for flag in ["ro", "nosuid", "nodev"] {
        assert!(
            entry.options.iter().any(|o| o == flag),
            "{flag} in {entry:?}"
        );
    }
    let uid = format!("user_id={}", rustix::process::geteuid().as_raw());
    let gid = format!("group_id={}", rustix::process::getegid().as_raw());
    for option in [uid.as_str(), gid.as_str(), "default_permissions"] {
        assert!(
            entry.super_options.iter().any(|o| o == option),
            "{option} in {entry:?}"
        );
    }
    assert!(
        !entry.super_options.iter().any(|o| o == "allow_other"),
        "{entry:?}"
    );

    // The mount is the one that serves.
    assert_eq!(std::fs::read(mounted.path("f")).unwrap(), bytes);

    let real_mp = mounted.real_mp.clone();
    let events = served.events.clone();
    mounted.unmount();
    assert_eq!(
        mount_entry(&real_mp),
        None,
        "the entry is gone after the unmount"
    );
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; READERS]);
    assert!(all_written(&events));
}

#[test]
fn a_put_mount_is_writable_in_the_mount_table_and_its_unmount_removes_the_entry() {
    let options = MountOptions {
        allow_other: false,
        ..MountOptions::put("tsfuse-conformance-put")
    };
    let Some((mounted, session)) = Mounted::with(
        "t-f7-put",
        &options,
        &InitConfig::put(),
        READERS,
        |m, mp, o| m.mount(mp, o),
    ) else {
        return;
    };
    let (tree, _) = tree();
    let served = serve_inline(session, tree);

    let entry = mounted
        .entry()
        .expect("the mount is in /proc/self/mountinfo");
    assert_eq!(entry.fstype, options.fstype());
    assert_eq!(entry.source, "tsfuse-conformance-put");
    for flag in ["rw", "nosuid", "nodev"] {
        assert!(
            entry.options.iter().any(|o| o == flag),
            "{flag} in {entry:?}"
        );
    }
    assert!(!entry.options.iter().any(|o| o == "ro"), "{entry:?}");
    assert!(!entry.super_options.iter().any(|o| o == "ro"), "{entry:?}");
    assert!(
        entry
            .super_options
            .iter()
            .any(|o| o == "default_permissions"),
        "{entry:?}"
    );
    // The writable mount answers a stat of its root as the tree one does.
    assert!(std::fs::metadata(&mounted.mp).unwrap().is_dir());

    let real_mp = mounted.real_mp.clone();
    mounted.unmount();
    assert_eq!(mount_entry(&real_mp), None);
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; READERS]);
}

#[test]
fn the_connection_of_a_mount_is_a_directory_of_the_control_file_system_until_it_is_unmounted() {
    let Some((mounted, session)) = Mounted::new("t-f7-conn", READERS) else {
        return;
    };
    let (tree, _) = tree();
    let served = serve_inline(session, tree);
    let id = mounted.mount().connection().unwrap();
    let directory = Path::new(CONNECTIONS).join(id.0.to_string());
    if let Err(e) = std::fs::read_dir(CONNECTIONS) {
        skip(&format!("{CONNECTIONS} cannot be listed here: {e}"));
        mounted.unmount();
        served.join();
        return;
    }
    assert!(directory.is_dir(), "{} while mounted", directory.display());
    assert!(directory.join("abort").exists());

    mounted.unmount();
    assert!(
        !directory.exists(),
        "{} after the unmount",
        directory.display()
    );
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; READERS]);
}

/// A mount with a file of it held open: the unmount fails and leaves the mount; the lazy one
/// detaches it, the file still reads, and closing it ends every reader.
fn busy_mount_is_unmounted_lazily_only(mut mounted: Mounted, session: tsfuse::Session) {
    let (tree, bytes) = tree();
    let served = serve_inline(session, tree);
    let events = served.events.clone();
    let file = File::open(mounted.path("f")).unwrap();
    assert_eq!(read_all(&file), bytes);

    let mount = mounted.mount().clone();
    let err = mount.clone().unmount(false).unwrap_err();
    match mount.method() {
        MountMethod::Syscall => {
            let MountError::Unmount { mountpoint, source } = &err else {
                panic!("{err:?}");
            };
            assert_eq!(mountpoint, &mounted.mp);
            assert_eq!(source.raw_os_error(), Some(errno::EBUSY), "{err}");
        }
        MountMethod::Fusermount => {
            assert!(matches!(err, MountError::Helper { .. }), "{err:?}");
        }
    }
    assert!(
        mounted.entry().is_some(),
        "the busy mount is still in the table"
    );
    assert_eq!(read_all(&file), bytes, "and still served");

    mounted.take().unmount(true).unwrap();
    assert_eq!(mounted.entry(), None, "a lazy unmount detaches it");
    assert_eq!(
        read_all(&file),
        bytes,
        "the held file reads through the live session"
    );
    assert_eq!(events.with(|s| s.reads.len()), 3);

    drop(file);
    ended_by_the_kernel(&served.join());
    assert!(all_written(&events));
}

#[test]
fn an_unmount_of_a_busy_mount_fails_and_leaves_it_and_a_lazy_unmount_detaches_it() {
    let Some((mounted, session)) = Mounted::new("t-f7-busy", READERS) else {
        return;
    };
    busy_mount_is_unmounted_lazily_only(mounted, session);
}

#[test]
fn an_abort_with_a_read_parked_fails_the_caller_and_the_readers_and_leaves_the_mount_to_unmount() {
    if !is_root() {
        skip("the abort file of a connection is root's: this process is not root");
        return;
    }
    let Some((mounted, session)) = Mounted::new("t-f7-abort", READERS) else {
        return;
    };
    let (tree, _) = tree();
    let served = serve(
        session,
        tree,
        Open::Flags(FOPEN_DIRECT_IO),
        Opendir::Ok,
        |_, read, replier, events| park(read, replier, events),
    );
    let caller = pread(mounted.path("f"), 0, SIZE);
    served
        .events
        .wait_for(|s| (!s.parked.is_empty()).then_some(()));

    mounted.mount().abort().unwrap();

    let err = caller.result().unwrap_err();
    assert_eq!(err.raw_os_error(), Some(errno::ECONNABORTED), "{err}");
    caller.join();
    let parked = served.take_parked();
    assert_eq!(parked.len(), 1);
    for p in parked {
        let late = Done::from(&p.replier.ok(&[b"late".as_slice()]));
        assert!(matches!(late, Done::DeviceError(_)), "{late:?}");
    }
    let events = served.events.clone();
    ended_by_the_kernel(&served.join());
    assert_eq!(events.with(|s| s.reads.len()), 1);
    assert!(
        mounted.entry().is_some(),
        "an aborted mount stays in the table"
    );

    let real_mp = mounted.real_mp.clone();
    mounted.unmount();
    assert_eq!(
        mount_entry(&real_mp),
        None,
        "and its unmount removes the entry"
    );
}

/// The name of [`child_mounts_and_serves_a_tree_until_its_stdin_ends`], as a command line gives it.
const MOUNTING_CHILD: &str = "child_mounts_and_serves_a_tree_until_its_stdin_ends";

/// The server of the kill-then-unmount check: mounts at the path on its first line of stdin,
/// serves a tree with two readers, says `serving <method>` on stdout, and serves until stdin ends.
#[test]
#[ignore = "the child process of a_killed_server_leaves_a_mount_that_answers_enotconn_and_unmounts, which runs it with --ignored"]
fn child_mounts_and_serves_a_tree_until_its_stdin_ends() {
    let mut line = String::new();
    io::stdin().read_line(&mut line).unwrap();
    let mp = PathBuf::from(line.trim_end());
    let (device, mount) = Mounter::default()
        .mount(&mp, &tree_options("tsfuse-conformance-child"))
        .unwrap();
    let mut session = tsfuse::Session::init(device, &InitConfig::tree()).unwrap();
    session
        .clone_readers(NonZeroUsize::new(READERS).unwrap())
        .unwrap();
    let (tree, _) = tree();
    let served = serve(
        session,
        tree,
        Open::Flags(FOPEN_DIRECT_IO),
        Opendir::Ok,
        |_, read, replier, events| park(read, replier, events),
    );
    writeln!(io::stdout(), "serving {:?}", mount.method()).unwrap();
    io::copy(&mut io::stdin(), &mut io::sink()).unwrap();
    drop(served);
    let _ = mount.unmount(true);
}

/// The child server and the mount it made; dropped without [`Self::done`] (a failed check), it is
/// killed and its mount detached.
struct Orphan {
    child: Child,
    mp: PathBuf,
    live: bool,
}

impl Drop for Orphan {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.live {
            for method in [MountMethod::Syscall, MountMethod::Fusermount] {
                let _ = Mount::adopt(&self.mp, method, &Mounter::default()).unmount(true);
            }
        }
    }
}

/// A `statx` of `path` that asks the file system (`AT_STATX_FORCE_SYNC`), so that the attributes
/// the kernel cached do not answer it.
fn synchronous_statx(path: &Path) -> io::Result<rustix::fs::Statx> {
    Ok(rustix::fs::statx(
        CWD,
        path,
        AtFlags::STATX_FORCE_SYNC,
        StatxFlags::MODE,
    )?)
}

#[test]
fn a_killed_server_leaves_a_mount_that_answers_enotconn_and_unmounts() {
    if let Some(why) = kernel::unavailable() {
        skip(why);
        return;
    }
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "t-f7-kill");
    let mp = dir.path().join("mnt");
    std::fs::create_dir(&mp).unwrap();
    let real_mp = std::fs::canonicalize(&mp).unwrap();

    let mut command = child(MOUNTING_CHILD);
    let mut spawned = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = spawned.stdin.take().unwrap();
    let mut stdout = BufReader::new(spawned.stdout.take().unwrap());
    let mut orphan = Orphan {
        child: spawned,
        mp: mp.clone(),
        live: true,
    };
    writeln!(stdin, "{}", mp.display()).unwrap();
    stdin.flush().unwrap();
    let method = loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "the child ended before serving"
        );
        // The harness of the child has begun this line with its own `test ... ` progress.
        match line
            .trim_end()
            .split_once("serving ")
            .map(|(_, method)| method)
        {
            Some("Syscall") => break MountMethod::Syscall,
            Some("Fusermount") => break MountMethod::Fusermount,
            Some(other) => panic!("a method of mounting that is none: {other}"),
            None => {}
        }
    };

    let served = synchronous_statx(&mp).unwrap();
    assert_eq!(
        u32::from(served.stx_mode) & 0o170_000,
        0o040_000,
        "the root is a directory"
    );
    assert_eq!(served.stx_ino, ROOT);
    assert!(mount_entry(&real_mp).is_some());

    orphan.child.kill().unwrap();
    let status = orphan.child.wait().unwrap();
    assert_eq!(status.signal(), Some(9), "killed by SIGKILL: {status:?}");

    let err = synchronous_statx(&mp).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(errno::ENOTCONN), "{err}");
    assert!(
        mount_entry(&real_mp).is_some(),
        "the mount outlives the process that served it"
    );

    Mount::adopt(&mp, method, &Mounter::default())
        .unmount(false)
        .unwrap();
    assert_eq!(mount_entry(&real_mp), None);
    orphan.live = false;
    drop(stdin);
}

/// What another user (`nobody`) gets on the root of a mount, whose descriptor `root` was opened
/// by this process: `fstat` and `openat` of `f`, each as the error it ended with. `Err` when this
/// thread may not change its user.
fn as_nobody(root: &OwnedFd) -> io::Result<(io::Result<()>, io::Result<()>)> {
    thread::scope(|scope| {
        scope
            .spawn(|| {
                let nobody = rustix::process::Uid::from_raw(65534);
                rustix::thread::set_thread_res_uid(nobody, nobody, nobody)?;
                let stat = rustix::fs::fstat(root).map(drop).map_err(io::Error::from);
                let open =
                    rustix::fs::openat(root, "f", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
                        .map(drop)
                        .map_err(io::Error::from);
                Ok((stat, open))
            })
            .join()
            .unwrap()
    })
}

/// Mounts with `allow_other` as `allow` says, as root, and runs [`as_nobody`] on the root.
fn nobody_on_a_mount(
    name: &str,
    allow: bool,
) -> Option<(io::Result<()>, io::Result<()>, Vec<String>)> {
    if !is_root() {
        skip("changing the user of a thread, and mounting with allow_other, need root");
        return None;
    }
    let options = MountOptions {
        allow_other: allow,
        ..tree_options("tsfuse-conformance")
    };
    let (mounted, session) =
        Mounted::with(name, &options, &InitConfig::tree(), READERS, |m, mp, o| {
            m.mount(mp, o)
        })?;
    let (tree, _) = tree();
    let served = serve_inline(session, tree);
    let super_options = mounted.entry().unwrap().super_options;
    let root = OwnedFd::from(File::open(&mounted.mp).unwrap());
    let nobody = as_nobody(&root);
    drop(root);
    let real_mp = mounted.real_mp.clone();
    mounted.unmount();
    assert_eq!(mount_entry(&real_mp), None);
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; READERS]);
    match nobody {
        Ok((stat, open)) => Some((stat, open, super_options)),
        Err(e) => {
            skip(&format!(
                "this process may not change the user of a thread: {e}"
            ));
            None
        }
    }
}

#[test]
fn with_allow_other_another_user_stats_the_root_and_opens_a_file_of_the_mount() {
    let Some((stat, open, super_options)) = nobody_on_a_mount("t-f7-other-on", true) else {
        return;
    };
    assert!(
        super_options.iter().any(|o| o == "allow_other"),
        "{super_options:?}"
    );
    stat.unwrap();
    open.unwrap();
}

#[test]
fn without_allow_other_another_user_is_refused_with_eacces_on_the_root_and_on_a_file() {
    let Some((stat, open, super_options)) = nobody_on_a_mount("t-f7-other-off", false) else {
        return;
    };
    assert!(
        !super_options.iter().any(|o| o == "allow_other"),
        "{super_options:?}"
    );
    assert_eq!(stat.unwrap_err().raw_os_error(), Some(errno::EACCES));
    assert_eq!(open.unwrap_err().raw_os_error(), Some(errno::EACCES));
}

/// Why `fusermount3` cannot run here, if it cannot.
fn no_fusermount() -> Option<String> {
    Command::new(FUSERMOUNT)
        .arg("--version")
        .output()
        .err()
        .map(|e| format!("{FUSERMOUNT} cannot be run: {e}"))
}

#[test]
fn a_mount_forced_through_fusermount3_is_served_and_unmounted_through_it() {
    if let Some(why) = no_fusermount() {
        skip(&why);
        return;
    }
    let Some((mounted, session)) = Mounted::with(
        "t-f7-helper",
        &tree_options("tsfuse-conformance"),
        &InitConfig::tree(),
        READERS,
        |m, mp, o| m.mount_fusermount(mp, o),
    ) else {
        return;
    };
    assert_eq!(mounted.mount().method(), MountMethod::Fusermount);
    let (tree, bytes) = tree();
    let served = serve_inline(session, tree);
    let entry = mounted
        .entry()
        .expect("the mount is in /proc/self/mountinfo");
    assert_eq!(entry.fstype, "fuse.tsfuse");
    assert_eq!(entry.source, "tsfuse-conformance");
    assert_eq!(std::fs::read(mounted.path("f")).unwrap(), bytes);

    let real_mp = mounted.real_mp.clone();
    let events = served.events.clone();
    mounted.unmount();
    assert_eq!(mount_entry(&real_mp), None);
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; READERS]);
    assert!(all_written(&events));
}

#[test]
fn a_busy_mount_made_and_unmounted_through_fusermount3_fails_to_unmount_and_detaches_lazily() {
    if let Some(why) = no_fusermount() {
        skip(&why);
        return;
    }
    let Some((mounted, session)) = Mounted::with(
        "t-f7-helper-busy",
        &tree_options("tsfuse-conformance"),
        &InitConfig::tree(),
        READERS,
        |m, mp, o| m.mount_fusermount(mp, o),
    ) else {
        return;
    };
    assert_eq!(mounted.mount().method(), MountMethod::Fusermount);
    busy_mount_is_unmounted_lazily_only(mounted, session);
}

#[test]
fn the_abort_of_a_mount_that_is_gone_is_an_error_naming_the_abort_file_and_writes_nothing() {
    let Some((mut mounted, session)) = Mounted::new("t-f7-gone", READERS) else {
        return;
    };
    let (tree, _) = tree();
    let served = serve_inline(session, tree);
    let before = mounted.mount().connection().unwrap();
    let mp = mounted.mp.clone();
    mounted.take().unmount(false).unwrap();
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; READERS]);

    let gone = Mount::adopt(&mp, MountMethod::Syscall, &Mounter::default());
    // The directory is there again, on the file system under it: its device number is another
    // connection's number, not the old one's.
    let now = gone.connection().unwrap();
    assert_ne!(now, before);
    let abort = Path::new(CONNECTIONS).join(now.0.to_string()).join("abort");
    let err = gone.abort().unwrap_err();
    assert!(
        matches!(&err, MountError::Abort { path, .. } if path == &abort),
        "{err:?}"
    );
    assert!(
        !abort.exists(),
        "nothing was created at {}",
        abort.display()
    );
    assert!(!Path::new(CONNECTIONS).join(before.0.to_string()).exists());

    // And with the directory itself removed, the connection cannot be read at all.
    std::fs::remove_dir(&mp).unwrap();
    assert!(matches!(
        gone.connection().unwrap_err(),
        MountError::Connection { .. }
    ));
    assert!(matches!(
        gone.abort().unwrap_err(),
        MountError::Connection { .. }
    ));
}

//! The put directory through [`NodeFs`](super::NodeFs) on `tsfuse`, over a real mount: what a job
//! sees through the kernel, made by a whole start of the process (`run.rs`) with `fs.put`
//! enabled. A job writes, closes and commits; what the put directory does not do is refused by its
//! errno; a writer killed mid-write leaves its file staged and uploads nothing; concurrent commits
//! each upload their own bytes; and a restart that hands the mount over, or leaves it to the
//! service manager, keeps the job's files, its open file and the directory it holds. A mount needs
//! `/dev/fuse` and `fusermount3` (the mounting user is not root); where either is missing the test
//! says so on stderr and ends.

use std::fs::OpenOptions;
use std::io::{BufRead as _, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use rustix::fs::{CWD, FileType, Mode, OFlags, XattrFlags};
use tokio::sync::Semaphore;

use crate::server::local_cache::testing::{Rig as NodeRig, Tamper};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::fills::Calls;
use crate::server::treefs::put::PutSettings;
use crate::server::treefs::put::config::PutConfig;
use crate::server::treefs::put::space::{PutAttr, PutSpace};
use crate::server::treefs::put_fs::{absent, attr, entry};
use crate::server::treefs::run::{Kernel, Prepared, Running};
use crate::server::treefs::testing::{Rig, sample, until, with_fuse};
use crate::{Backend as _, Op};

/// The job's directory, below `put/`.
const JOB: &str = "cas/b/build/nonce";

/// The argument that tells the writer process where to write.
const WRITER: &str = "put-writer=";

/// The full name of the writer process's test.
const WRITER_TEST: &str = "server::treefs::node_fs::put_tests::writer_process";

/// Settings of a rig with the put directory enabled, its stage beside the cache.
fn settings(rig: &Rig) -> FsSettings {
    let mut settings = rig.settings();
    settings.put = Some(PutConfig {
        stage_dir: rig.dir.path().join("stage"),
        store: PutSettings {
            prefixes: vec!["cas/b/".to_owned()],
            max_object_bytes: None,
            max_dir_bytes: None,
            max_dir_files: None,
            min_free_bytes: None,
            stage_ttl: Duration::from_secs(43_200),
            committed_ttl: Duration::from_secs(600),
        },
        commit_deadline: Duration::from_secs(30),
    });
    settings
}

/// The puts the cache of a [`Served`] makes to its upstream, held there from the moment they
/// reach it until they are let go.
struct Held {
    gate: Arc<Semaphore>,
    tamper: Arc<Tamper>,
}

impl Held {
    /// Lets the waiting puts, and the next ones, go on.
    fn let_go(&self) {
        self.gate.add_permits(1 << 16);
    }

    /// Waits until `count` puts are waiting at the upstream.
    fn wait_for(&self, count: u64) {
        until(|| self.tamper.at_puts() >= count);
    }
}

/// A process serving the mount of a rig, with the put directory: its core calls a cache of its own
/// that writes through to an upstream the test reads, which a restart keeps.
struct Served {
    running: Option<Running>,
    settings: FsSettings,
    /// What the core calls.
    calls: Calls,
    /// The cache the core calls, and the upstream it puts to.
    store: NodeRig,
    rig: Rig,
}

impl Served {
    /// A start of the process over a new rig named after `name`.
    fn start(name: &str) -> Self {
        let rig = Rig::new(name, sample());
        let store = {
            let _entered = rig.work.handle().enter();
            NodeRig::writing(&format!("{name}-store"))
        };
        let calls = Calls {
            cache: store.cache.clone(),
            trees: None,
            runtime: rig.work.handle(),
        };
        let settings = settings(&rig);
        let running = Running::start((&settings, &calls), &Kernel::real()).unwrap();
        Self {
            running: Some(running),
            settings,
            calls,
            store,
            rig,
        }
    }

    /// Holds the puts of the cache at the upstream from now on.
    fn hold(&self) -> Arc<Held> {
        Arc::new(Held {
            gate: self.store.tamper.hold_puts(),
            tamper: Arc::clone(&self.store.tamper),
        })
    }

    /// The puts that reached the upstream.
    fn puts(&self) -> u64 {
        self.store.calls(Op::Put)
    }

    /// The path of `name` below `put/`.
    fn at(&self, name: &str) -> PathBuf {
        self.rig.mountpoint.join("put").join(name)
    }

    /// The put directory of the process serving now.
    fn space(&self) -> &PutSpace {
        self.running.as_ref().unwrap().core().put().unwrap()
    }

    /// The bytes uploaded as the object `key`.
    fn object(&self, key: &str) -> Option<Vec<u8>> {
        let got = self
            .rig
            .work
            .handle()
            .block_on(self.store.tamper.get(key, None));
        got.ok().map(|bytes| bytes.to_vec())
    }

    /// The hand-over of a running instance to a successor, as `handoff-fs-v1` does it: the copies
    /// of the descriptors offered, the running one quiesced, the successor adopting the connection
    /// and the control socket, the running one giving up and ending.
    fn hand_over(&mut self) {
        let first = self.running.take().unwrap();
        let (device, control) = first.share().unwrap();
        first.quiesce();
        self.adopt(device, Some(UnixListener::from(control)));
        first.give_up();
        drop(first);
    }

    /// A restart that leaves the mount to the service manager's store: the running instance stops
    /// reading and ends, and the next one adopts the connection.
    fn restart(&mut self) {
        let first = self.running.take().unwrap();
        let device = first.device().unwrap();
        first.leave();
        self.adopt(device, None);
    }

    /// Starts the successor over `device`.
    fn adopt(&mut self, device: OwnedFd, control: Option<UnixListener>) {
        let prepared = Prepared::load(&self.settings).unwrap();
        let next = Running::adopt(
            (&self.settings, &self.calls),
            prepared,
            device,
            (control, false),
        )
        .unwrap();
        self.running = Some(next);
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.stop();
        }
        let _ = Command::new("fusermount3")
            .arg("-u")
            .arg("-z")
            .arg(&self.rig.mountpoint)
            .status();
    }
}

/// The errno of `error`.
fn errno_of(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap()
}

/// Creates `path` (create-only), writes `bytes` and closes it.
fn write_file(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

/// Asserts the life of a job's output through the kernel: the job's directory exists before
/// anything is in it; a `.part` file is created, written in pieces and closed, and its commit
/// rename right after the close (whose release FUSE sends later) uploads the bytes; the committed
/// name is then a file of mode 0400 of their size, which cannot be read, and the `.part` name is
/// gone; the directory lists the committed name; `statfs` answers names of 255 bytes.
#[test]
fn a_job_writes_closes_and_commits_through_the_mount() {
    with_fuse(|| {
        let served = Served::start("put-mount-commit");
        let job = served.at(JOB);
        assert!(std::fs::metadata(&job).unwrap().is_dir());
        assert_eq!(std::fs::read_dir(&job).unwrap().count(), 0);
        let part = job.join("pack.part");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part)
                .unwrap();
            for piece in [&b"the "[..], b"pack ", b"bytes"] {
                file.write_all(piece).unwrap();
            }
            assert_eq!(file.metadata().unwrap().mode() & 0o7777, 0o600);
        }
        std::fs::rename(&part, job.join("pack")).unwrap();
        assert_eq!(
            served.object(&format!("{JOB}/pack")).unwrap(),
            b"the pack bytes"
        );
        let meta = std::fs::metadata(job.join("pack")).unwrap();
        assert_eq!((meta.len(), meta.mode() & 0o7777), (14, 0o400));
        assert_eq!(
            errno_of(&std::fs::metadata(&part).unwrap_err()),
            libc_errno::ENOENT
        );
        assert_eq!(
            errno_of(&std::fs::File::open(job.join("pack")).unwrap_err()),
            libc_errno::EACCES
        );
        let names: Vec<String> = std::fs::read_dir(&job)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["pack"]);
        assert_eq!(rustix::fs::statvfs(&job).unwrap().f_namemax, 255);
    });
}

/// The errnos the tests compare, as the kernel gives them on Linux.
mod libc_errno {
    pub(super) const EPERM: i32 = 1;
    pub(super) const ENOENT: i32 = 2;
    pub(super) const EACCES: i32 = 13;
    pub(super) const EEXIST: i32 = 17;
    pub(super) const EINVAL: i32 = 22;
}

/// Asserts that what the put directory does not do is refused through the kernel: `put/` and the
/// directories that lead to the prefix are not listed and take no file; a final name is not
/// created; a staged file is not opened again; a hard link, a symbolic link, a node, a truncate, a
/// set-id mode, an extended attribute, an `rmdir`, a rename across directories and a rename of a
/// committed name are `EPERM`; a name with white space is `EINVAL`; an unlink of a `.part` name
/// aborts it and one of a committed name is `EPERM`; `mkdir` makes a directory where free names
/// are not there.
#[test]
fn what_the_put_directory_does_not_do_is_refused_through_the_mount() {
    with_fuse(|| {
        let served = Served::start("put-mount-refused");
        let put = served.at("");
        let job = served.at(JOB);
        assert_eq!(
            errno_of(&std::fs::read_dir(&put).unwrap_err()),
            libc_errno::EACCES
        );
        assert_eq!(
            errno_of(&std::fs::read_dir(served.at("cas")).unwrap_err()),
            libc_errno::EACCES
        );
        // The kernel refuses a user that is not root by the mode 0711; the put directory refuses
        // root.
        let in_cas = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(served.at("cas/x.part"));
        let refused = errno_of(&in_cas.unwrap_err());
        assert!(
            [libc_errno::EACCES, libc_errno::EPERM].contains(&refused),
            "{refused}"
        );
        let spaced = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(job.join("a b.part"));
        assert_eq!(errno_of(&spaced.unwrap_err()), libc_errno::EINVAL);
        assert!(
            std::fs::metadata(served.rig.mountpoint.join("trees"))
                .unwrap()
                .is_dir()
        );
        let mut both = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(job.join("rw.part"))
            .unwrap();
        both.write_all(b"rw").unwrap();
        let mut back = [0_u8; 2];
        let read = std::io::Read::read(&mut both, &mut back);
        assert_eq!(errno_of(&read.unwrap_err()), libc_errno::EACCES);
        drop(both);
        let part = job.join("a.part");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .unwrap();
        // Now that the directory holds a file, a free name in it is not there, and a create of a
        // final name reaches the put directory.
        let final_name = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(job.join("pack"));
        assert_eq!(errno_of(&final_name.unwrap_err()), libc_errno::EPERM);
        assert_eq!(errno_of(&file.set_len(9).unwrap_err()), libc_errno::EPERM);
        let reopened = OpenOptions::new().write(true).open(&part);
        assert_eq!(errno_of(&reopened.unwrap_err()), libc_errno::EPERM);
        assert_eq!(
            errno_of(&std::fs::File::open(&part).unwrap_err()),
            libc_errno::EPERM
        );
        drop(file);
        let mode = std::fs::Permissions::from_mode(0o4600);
        assert_eq!(
            errno_of(&std::fs::set_permissions(&part, mode).unwrap_err()),
            libc_errno::EPERM
        );
        assert_eq!(
            errno_of(&std::fs::hard_link(&part, job.join("b.part")).unwrap_err()),
            libc_errno::EPERM
        );
        let linked = std::os::unix::fs::symlink("a.part", job.join("c.part"));
        assert_eq!(errno_of(&linked.unwrap_err()), libc_errno::EPERM);
        let node = rustix::fs::mknodat(
            CWD,
            job.join("d.part"),
            FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        );
        assert_eq!(node.unwrap_err().raw_os_error(), libc_errno::EPERM);
        let set = rustix::fs::setxattr(&part, "user.x", b"v", XattrFlags::empty());
        assert_eq!(set.unwrap_err().raw_os_error(), libc_errno::EPERM);
        let exchanged = rustix::fs::renameat_with(
            CWD,
            &part,
            CWD,
            job.join("rw.part"),
            rustix::fs::RenameFlags::EXCHANGE,
        );
        assert_eq!(exchanged.unwrap_err().raw_os_error(), libc_errno::EPERM);
        let other = served.at("cas/b/build/other");
        assert!(std::fs::metadata(&other).unwrap().is_dir());
        let across = std::fs::rename(&part, other.join("a.part"));
        assert_eq!(errno_of(&across.unwrap_err()), libc_errno::EPERM);
        write_file(&job.join("b.part"), b"b");
        std::fs::rename(job.join("b.part"), job.join("b")).unwrap();
        assert_eq!(
            errno_of(&std::fs::rename(job.join("b"), job.join("c")).unwrap_err()),
            libc_errno::EPERM
        );
        assert_eq!(
            errno_of(&std::fs::remove_file(job.join("b")).unwrap_err()),
            libc_errno::EPERM
        );
        std::fs::remove_file(&part).unwrap();
        assert_eq!(
            errno_of(&std::fs::metadata(&part).unwrap_err()),
            libc_errno::ENOENT
        );
        assert_eq!(
            errno_of(&std::fs::create_dir(other.join("free")).unwrap_err()),
            libc_errno::EEXIST
        );
        std::fs::create_dir(job.join("sub")).unwrap();
        assert!(std::fs::metadata(job.join("sub")).unwrap().is_dir());
        assert_eq!(
            errno_of(&std::fs::remove_dir(job.join("sub")).unwrap_err()),
            libc_errno::EPERM
        );
        write_file(&job.join("sub/deep.part"), b"deep");
        std::fs::rename(job.join("sub/deep.part"), job.join("sub/deep")).unwrap();
        assert_eq!(served.object(&format!("{JOB}/sub/deep")).unwrap(), b"deep");
    });
}

/// The writer process of [`a_killed_writer_leaves_its_file_staged_and_uploads_nothing`]: it
/// creates the file it is told, writes four bytes, and waits on its standard input until it is
/// killed.
#[test]
#[ignore = "the writer process that a_killed_writer_leaves_its_file_staged_and_uploads_nothing starts and kills"]
fn writer_process() {
    let Some(path) = std::env::args().find_map(|arg| arg.strip_prefix(WRITER).map(PathBuf::from))
    else {
        let _ = writeln!(std::io::stderr(), "skipped: no {WRITER}<path> argument");
        return;
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(b"half").unwrap();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
}

/// Asserts that a writer killed with its file open and half written leaves the file staged with
/// the bytes it wrote, the kernel's release of its open reaching the put directory, and nothing
/// uploaded; the file is then committed by a rename as it is.
#[test]
fn a_killed_writer_leaves_its_file_staged_and_uploads_nothing() {
    with_fuse(|| {
        let served = Served::start("put-mount-killed");
        let part = served.at(JOB).join("pack.part");
        let mut writer = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", WRITER_TEST, "--ignored", "--test-threads=1"])
            .arg(format!("{WRITER}{}", part.display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let store = served.space().store();
        until(|| {
            store.open_handles().len() == 1
                && std::fs::metadata(&part).is_ok_and(|meta| meta.len() == 4)
        });
        writer.kill().unwrap();
        assert!(!writer.wait().unwrap().success());
        until(|| store.open_handles().is_empty());
        assert_eq!(served.puts(), 0);
        assert_eq!(std::fs::metadata(&part).unwrap().len(), 4);
        std::fs::rename(&part, served.at(JOB).join("pack")).unwrap();
        assert_eq!(served.object(&format!("{JOB}/pack")).unwrap(), b"half");
    });
}

/// Asserts that concurrent jobs' commits through the kernel each upload their own bytes, and that
/// of two files of other bytes committed to one key at once exactly one is the object and the
/// other is `EEXIST`.
#[test]
fn concurrent_renames_through_the_mount_each_commit_their_own_bytes() {
    with_fuse(|| {
        let served = Served::start("put-mount-concurrent");
        let job = served.at(JOB);
        std::thread::scope(|scope| {
            let mut commits = Vec::new();
            for n in 0..16 {
                let job = &job;
                commits.push(scope.spawn(move || {
                    let part = job.join(format!("out{n}.part"));
                    write_file(&part, format!("bytes of {n}").as_bytes());
                    std::fs::rename(&part, job.join(format!("out{n}")))
                }));
            }
            let racing: Vec<_> = ["x", "y"]
                .into_iter()
                .map(|name| {
                    let job = &job;
                    scope.spawn(move || {
                        let part = job.join(format!("{name}.part"));
                        write_file(&part, name.as_bytes());
                        std::fs::rename(&part, job.join("one")).map(|()| name)
                    })
                })
                .collect();
            for commit in commits {
                commit.join().unwrap().unwrap();
            }
            let results: Vec<_> = racing
                .into_iter()
                .map(|racer| racer.join().unwrap())
                .collect();
            let won: Vec<&str> = results
                .iter()
                .filter_map(|result| result.as_ref().ok().copied())
                .collect();
            assert_eq!(won.len(), 1, "{results:?}");
            for result in &results {
                if let Err(error) = result {
                    assert_eq!(errno_of(error), libc_errno::EEXIST);
                }
            }
            assert_eq!(
                served.object(&format!("{JOB}/one")).unwrap(),
                won[0].as_bytes()
            );
        });
        for n in 0..16 {
            let object = served.object(&format!("{JOB}/out{n}")).unwrap();
            assert_eq!(object, format!("bytes of {n}").into_bytes());
        }
    });
}

/// Asserts that a hand-over of the mount to a successor keeps the job going: a file the job holds
/// open across it is written on and committed through the successor; a staged file the job closed
/// before it is committed; and a directory the job holds open while it was empty takes a create
/// after it.
#[test]
fn a_hand_over_keeps_the_job_s_open_file_its_staged_files_and_its_directory() {
    with_fuse(|| {
        let mut served = Served::start("put-mount-handover");
        let job = served.at(JOB);
        let empty = rustix::fs::open(
            served.at("cas/b/build/empty"),
            OFlags::DIRECTORY | OFlags::RDONLY,
            Mode::empty(),
        )
        .unwrap();
        let mut open = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(job.join("pack.part"))
            .unwrap();
        open.write_all(b"before ").unwrap();
        write_file(&job.join("L0.part"), b"large");
        served.hand_over();
        open.write_all(b"after").unwrap();
        drop(open);
        std::fs::rename(job.join("pack.part"), job.join("pack")).unwrap();
        std::fs::rename(job.join("L0.part"), job.join("L0")).unwrap();
        assert_eq!(
            served.object(&format!("{JOB}/pack")).unwrap(),
            b"before after"
        );
        assert_eq!(served.object(&format!("{JOB}/L0")).unwrap(), b"large");
        let made = rustix::fs::openat(
            empty.as_fd(),
            "late.part",
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        rustix::io::write(&made, b"late").unwrap();
        drop(made);
        rustix::fs::renameat(empty.as_fd(), "late.part", empty.as_fd(), "late").unwrap();
        assert_eq!(served.object("cas/b/build/empty/late").unwrap(), b"late");
    });
}

/// Asserts that a hand-over that does not complete leaves the running instance serving the put
/// directory: the resume removes the hand-over file and the job's commits go on; and that a
/// resume whose journal cannot be written afresh does not serve, and serves once it can.
#[test]
fn a_hand_over_that_does_not_complete_leaves_the_instance_serving() {
    with_fuse(|| {
        let served = Served::start("put-mount-resume");
        let job = served.at(JOB);
        write_file(&job.join("pack.part"), b"kept");
        let running = served.running.as_ref().unwrap();
        let handover = served.settings.runtime_dir.join("put-handover");
        running.quiesce();
        assert!(handover.exists());
        running.resume().unwrap();
        assert!(!handover.exists());
        std::fs::rename(job.join("pack.part"), job.join("pack")).unwrap();
        assert_eq!(served.object(&format!("{JOB}/pack")).unwrap(), b"kept");
        let stuck = served.rig.dir.path().join("stage").join("journal.new");
        std::fs::create_dir(&stuck).unwrap();
        std::fs::write(stuck.join("in-the-way"), b"").unwrap();
        running.quiesce();
        assert!(running.resume().is_err());
        std::fs::remove_file(stuck.join("in-the-way")).unwrap();
        std::fs::remove_dir(&stuck).unwrap();
        running.resume().unwrap();
        write_file(&job.join("L0.part"), b"after");
        std::fs::rename(job.join("L0.part"), job.join("L0")).unwrap();
        assert_eq!(served.object(&format!("{JOB}/L0")).unwrap(), b"after");
    });
}

/// Asserts that a commit still uploading when a hand-over begins is finished and answered by the
/// running instance before its quiesce returns, so before the successor is told `done`: the job's
/// rename succeeds and the object is uploaded. The upload is let go from the gap where the quiesce
/// is about to wait for it. Catches a hand-over that leaves a job's commit to a process that ends
/// at `ready`, whose rename the kernel does not send again.
#[test]
fn a_commit_uploading_at_a_hand_over_is_answered_before_the_quiesce_returns() {
    with_fuse(|| {
        let mut served = Served::start("put-mount-commit-at-handover");
        let job = served.at(JOB);
        write_file(&job.join("pack.part"), b"uploading");
        let puts = served.hold();
        let first = served.running.take().unwrap();
        let gate = Arc::clone(&puts);
        *first.core().put().unwrap().before_drain.lock().unwrap() =
            Some(Arc::new(move || gate.let_go()));
        let (device, control) = first.share().unwrap();
        std::thread::scope(|scope| {
            let rename = scope.spawn(|| std::fs::rename(job.join("pack.part"), job.join("pack")));
            puts.wait_for(1);
            first.quiesce();
            assert_eq!(
                served.object(&format!("{JOB}/pack")).as_deref(),
                Some(&b"uploading"[..]),
                "the commit was not finished before the quiesce returned"
            );
            puts.let_go();
            rename.join().unwrap().unwrap();
            served.adopt(device, Some(UnixListener::from(control)));
            first.give_up();
        });
        drop(first);
    });
}

/// Asserts that while a hand-over waits for a commit whose upload the node's cache has not
/// finished (the upstream holds it), the mount still answers: a `statfs`
/// of the mount made in that wait is answered, and only then is the upload let go, so the quiesce
/// returns with the commit uploaded. Catches a quiesce that stops the readers before it waits for
/// the upload, which leaves the mount answering nothing until the upload's deadline: a `statfs`
/// that never returns, and an upload never let go, which ends at its deadline unfinished.
#[test]
fn the_mount_answers_while_a_hand_over_waits_for_an_upload() {
    with_fuse(|| {
        let mut served = Served::start("put-mount-answers-in-quiesce");
        let job = served.at(JOB);
        write_file(&job.join("pack.part"), b"uploading");
        let puts = served.hold();
        let first = served.running.take().unwrap();
        let (said, answered) = mpsc::channel();
        let (gate, mountpoint) = (Arc::clone(&puts), served.rig.mountpoint.clone());
        let asked = AtomicBool::new(false);
        *first.core().put().unwrap().before_drain.lock().unwrap() = Some(Arc::new(move || {
            if asked.swap(true, Ordering::AcqRel) {
                return;
            }
            let (gate, mountpoint, said) = (Arc::clone(&gate), mountpoint.clone(), said.clone());
            let _ = std::thread::spawn(move || {
                let _ = said.send(rustix::fs::statfs(&mountpoint).is_ok());
                gate.let_go();
            });
        }));
        let (device, control) = first.share().unwrap();
        std::thread::scope(|scope| {
            let rename = scope.spawn(|| std::fs::rename(job.join("pack.part"), job.join("pack")));
            puts.wait_for(1);
            first.quiesce();
            // An answer that did not come leaves the upload held: it is let go for the rename.
            let heard = answered.try_recv();
            puts.let_go();
            assert_eq!(
                heard,
                Ok(true),
                "the mount did not answer while the quiesce waited for the upload"
            );
            assert_eq!(
                served.object(&format!("{JOB}/pack")).as_deref(),
                Some(&b"uploading"[..])
            );
            rename.join().unwrap().unwrap();
            served.adopt(device, Some(UnixListener::from(control)));
            first.give_up();
        });
        drop(first);
    });
}

/// Asserts that a rename made while no process reads the connection (between the quiesce of the
/// running instance and the successor's start) is answered by the successor and commits.
#[test]
fn a_rename_made_during_a_hand_over_is_answered_by_the_successor() {
    with_fuse(|| {
        let mut served = Served::start("put-mount-during");
        let job = served.at(JOB);
        write_file(&job.join("pack.part"), b"waiting");
        let first = served.running.take().unwrap();
        let (device, control) = first.share().unwrap();
        first.quiesce();
        std::thread::scope(|scope| {
            let rename = scope.spawn(|| std::fs::rename(job.join("pack.part"), job.join("pack")));
            served.adopt(device, Some(UnixListener::from(control)));
            first.give_up();
            rename.join().unwrap().unwrap();
        });
        drop(first);
        assert_eq!(served.object(&format!("{JOB}/pack")).unwrap(), b"waiting");
    });
}

/// Asserts that a restart that leaves the mount to the service manager keeps the job's staged
/// file and open file, and that one without the hand-over file (a crash) still finds the
/// directory the job holds once a name is in it, and the staged file, which then commits.
#[test]
fn a_restart_keeps_the_staged_files_with_or_without_the_hand_over_file() {
    with_fuse(|| {
        let mut served = Served::start("put-mount-restart");
        let job = served.at(JOB);
        let held =
            rustix::fs::open(&job, OFlags::DIRECTORY | OFlags::RDONLY, Mode::empty()).unwrap();
        let mut open = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(job.join("a.part"))
            .unwrap();
        open.write_all(b"one").unwrap();
        served.restart();
        open.write_all(b"two").unwrap();
        drop(open);
        let first = served.running.take().unwrap();
        let device = first.device().unwrap();
        first.leave();
        std::fs::remove_file(served.settings.runtime_dir.join("put-handover")).unwrap();
        served.adopt(device, None);
        rustix::fs::renameat(held.as_fd(), "a.part", held.as_fd(), "a").unwrap();
        assert_eq!(served.object(&format!("{JOB}/a")).unwrap(), b"onetwo");
    });
}

/// Asserts the attributes the kernel is told, on the `tsfuse` encoders of the put directory: a
/// directory is `S_IFDIR` with its permission bits and two links; a file is `S_IFREG` with its
/// permission bits, one link, its size and its blocks of 512 bytes; the time is sent in seconds and
/// nanoseconds of the millisecond count (the access, modification and change times alike); the
/// lookup reply carries the inode number and a validity of zero; and a name that is not there has
/// the inode number 0 and no link.
#[test]
fn the_kernel_is_told_the_attributes_of_the_space() {
    const S_IFDIR: u32 = 0o040_000;
    const S_IFREG: u32 = 0o100_000;
    let dir = attr(&PutAttr {
        ino: 9,
        dir: true,
        size: 0,
        perm: 0o700,
        uid: 1,
        gid: 2,
        ms: 1_500,
    });
    assert_eq!(
        (dir.ino, dir.mode, dir.nlink, dir.uid, dir.gid),
        (9, S_IFDIR | 0o700, 2, 1, 2)
    );
    assert_eq!((dir.mtime, dir.mtimensec), (1, 500_000_000));
    assert_eq!((dir.atime, dir.atimensec), (1, 500_000_000));
    assert_eq!((dir.ctime, dir.ctimensec), (1, 500_000_000));
    let file = attr(&PutAttr {
        ino: 10,
        dir: false,
        size: 513,
        perm: 0o600,
        uid: 1,
        gid: 2,
        ms: 0,
    });
    assert_eq!(
        (file.mode, file.nlink, file.blocks, file.size),
        (S_IFREG | 0o600, 1, 2, 513)
    );
    assert_eq!((file.mtime, file.mtimensec), (0, 0));
    let looked_up = entry(&PutAttr {
        ino: 10,
        dir: false,
        size: 513,
        perm: 0o600,
        uid: 1,
        gid: 2,
        ms: 0,
    });
    assert_eq!(
        (
            looked_up.nodeid,
            looked_up.attr.ino,
            looked_up.entry_valid,
            looked_up.attr_valid
        ),
        (10, 10, 0, 0)
    );
    let none = absent();
    assert_eq!((none.nodeid, none.attr.ino, none.attr.nlink), (0, 0, 0));
}

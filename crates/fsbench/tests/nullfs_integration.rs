//! The null file system over a real mount, through its library interface and its command. A
//! mount needs `/dev/fuse` and `fusermount3` (the tests mount as the user that runs them): a test
//! that cannot mount fails, and says why. Integration target: it mounts FUSE and runs the
//! system's `fusermount3`.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use fsbench::list::{Entry, FileList};
use fsbench::nullfs::{NullFs, OpenMode, Options, Tree, config, serve};
use fuser::{BackgroundSession, Errno, Session};
use tierstore_testkit::dir::TestDir;

fn list(paths: &[(&str, u64)]) -> FileList {
    FileList {
        files: paths
            .iter()
            .map(|&(path, size)| Entry {
                path: path.to_owned(),
                size,
            })
            .collect(),
        sha256: String::new(),
    }
}

fn sample() -> FileList {
    list(&[("a/b/c.h", 3), ("a/d", 300_000), ("e", 0)])
}

fn options(open: OpenMode, timeout: Duration) -> Options {
    Options {
        entry_timeout: timeout,
        attr_timeout: timeout,
        open,
        threads: 4,
        allow_other: false,
    }
}

fn me() -> (u32, u32) {
    (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    )
}

/// A mount of the null file system of a list, and its scratch directory.
struct Mounted {
    // Unmounted before the directory is removed.
    session: Option<BackgroundSession>,
    root: std::path::PathBuf,
    _dir: TestDir,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            session.umount_and_join().unwrap();
        }
    }
}

fn mounted(name: &str, files: &FileList, options: Options) -> Mounted {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let root = dir.path().join("mnt");
    std::fs::create_dir(&root).unwrap();
    let fs = NullFs::new(Tree::of(files).unwrap(), options, me());
    let session = Session::new(fs, &root, &config(&options))
        .and_then(Session::spawn)
        .unwrap_or_else(|e| {
            panic!("mount the null file system (needs /dev/fuse and fusermount3): {e}")
        });
    Mounted {
        session: Some(session),
        root,
        _dir: dir,
    }
}

/// Through a mount, every file of the list is there with its size, mode 0444, the mounting
/// user as owner and the fixed time, and reads as that many zero bytes; directories are 0555 and
/// list their names; a name that is not there is `ENOENT`; a directory of 20000 entries lists
/// every one, over several requests.
#[test]
fn the_files_of_the_list_read_as_zeros_through_the_mount() {
    // Enough names that a listing of `big` takes more than one request at any buffer size the
    // kernel uses (at most 1 MiB): 20000 entries of 48-byte names.
    let mut paths: Vec<(String, u64)> = (0..20_000)
        .map(|i| (format!("big/{i:05}-{}", "n".repeat(42)), i % 7))
        .collect();
    paths.extend([
        ("a/b/c.h".to_owned(), 3),
        ("a/d".to_owned(), 300_000),
        ("e".to_owned(), 0),
    ]);
    paths.sort();
    let files = list(
        &paths
            .iter()
            .map(|(p, s)| (p.as_str(), *s))
            .collect::<Vec<_>>(),
    );
    let m = mounted(
        "fsbench-nullfs-read",
        &files,
        options(OpenMode::Explicit, Duration::ZERO),
    );
    let d = std::fs::metadata(m.root.join("a/d")).unwrap();
    assert_eq!(
        (d.len(), d.mode() & 0o7777, d.nlink(), d.mtime()),
        (300_000, 0o444, 1, 0)
    );
    assert_eq!((d.uid(), d.gid()), me());
    assert_eq!(
        std::fs::read(m.root.join("a/d")).unwrap(),
        vec![0u8; 300_000]
    );
    assert_eq!(std::fs::read(m.root.join("a/b/c.h")).unwrap(), [0, 0, 0]);
    assert!(std::fs::read(m.root.join("e")).unwrap().is_empty());
    let a = std::fs::metadata(m.root.join("a")).unwrap();
    assert!(a.is_dir());
    assert_eq!(a.mode() & 0o7777, 0o555);
    let mut names: Vec<String> = std::fs::read_dir(&m.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["a", "big", "e"]);
    let big = std::fs::read_dir(m.root.join("big")).unwrap().count();
    assert_eq!(big, 20_000);
    let missing = std::fs::metadata(m.root.join("a/zz")).unwrap_err();
    assert_eq!(missing.raw_os_error(), Some(Errno::ENOENT.code()));
}

/// In the no-open mode the first open is answered `ENOSYS`, which the kernel takes as no-open:
/// every open, that one included, then succeeds, and the files read the same.
#[test]
fn the_no_open_mode_opens_files_without_asking_the_server() {
    let m = mounted(
        "fsbench-nullfs-noopen",
        &sample(),
        options(OpenMode::NoOpen, Duration::from_secs(3600)),
    );
    for _ in 0..3 {
        let mut file = std::fs::File::open(m.root.join("a/d")).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![0u8; 300_000]);
    }
}

/// `serve` mounts, says `mounted`, serves while its input is open and unmounts when it ends; a
/// mount point that is not there is an error that names it, and so is a list with no tree.
#[test]
fn serve_mounts_until_its_input_ends() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fsbench-nullfs-serve");
    let root = dir.path().join("mnt");
    std::fs::create_dir(&root).unwrap();
    let (input, input_end) = std::io::pipe().unwrap();
    let (output_end, output) = std::io::pipe().unwrap();
    let server = {
        let root = root.clone();
        let mounting = options(OpenMode::Explicit, Duration::ZERO);
        std::thread::spawn(move || serve(&sample(), &root, mounting, input, output))
    };
    let mut said = String::new();
    BufReader::new(output_end).read_line(&mut said).unwrap();
    assert_eq!(said, "mounted\n");
    assert_eq!(std::fs::metadata(root.join("a/d")).unwrap().len(), 300_000);
    drop(input_end);
    server.join().unwrap().unwrap();
    assert!(std::fs::metadata(root.join("a")).is_err(), "unmounted");
    let missing = dir.path().join("missing");
    let error = serve(
        &sample(),
        &missing,
        options(OpenMode::Explicit, Duration::ZERO),
        std::io::empty(),
        std::io::sink(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with(&format!("mount {}: ", missing.display())),
        "{error}"
    );
    let error = serve(
        &list(&[("a", 1), ("a/b", 1)]),
        &root,
        options(OpenMode::Explicit, Duration::ZERO),
        std::io::empty(),
        std::io::sink(),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "a/b: a file of the list is named as a directory"
    );
    let error = serve(
        &sample(),
        &root,
        options(OpenMode::Explicit, Duration::ZERO),
        std::io::empty(),
        Broken,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "null-fs: write: refused");
    assert!(
        std::fs::metadata(root.join("a")).is_err(),
        "unmounted when the session is dropped"
    );
}

/// A writer that refuses every write.
struct Broken;

impl std::io::Write for Broken {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("refused"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The mount is read-only, named `fsbench-null`, and open to its owner alone unless asked.
#[test]
fn the_mount_options_are_those_of_tierstore_fs_read_only() {
    let mut opts = options(OpenMode::Explicit, Duration::ZERO);
    let owner = config(&opts);
    assert_eq!(owner.acl, fuser::SessionACL::Owner);
    assert_eq!(owner.n_threads, Some(4));
    assert!(!owner.clone_fd);
    assert!(owner.mount_options.contains(&fuser::MountOption::RO));
    assert!(
        owner
            .mount_options
            .contains(&fuser::MountOption::DefaultPermissions)
    );
    opts.allow_other = true;
    assert_eq!(config(&opts).acl, fuser::SessionACL::All);
}

/// `fsbench null-fs` mounts its list, says `mounted` on standard output, serves, and unmounts and
/// exits 0 when its standard input closes.
#[test]
fn the_null_fs_command_mounts_until_its_input_ends() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fsbench-nullfs-command");
    let list = dir.path().join("list");
    std::fs::write(&list, format!("{}\n3\tf\n", fsbench::list::HEADER)).unwrap();
    let mnt = dir.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fsbench"))
        .args(["null-fs", "--open", "none", "--threads", "2", "--list"])
        .arg(&list)
        .arg("--mountpoint")
        .arg(&mnt)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut said)
        .unwrap();
    assert_eq!(said, "mounted\n");
    assert_eq!(std::fs::read(mnt.join("f")).unwrap(), [0, 0, 0]);
    child.stdin.take().unwrap().write_all(b"ignored\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(std::fs::metadata(mnt.join("f")).is_err(), "unmounted");
}

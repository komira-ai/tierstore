//! The null file system on `tsfuse` over a real mount, through `serve` and through its command:
//! with cloned readers, by `writev` and by splice (with `SPLICE_F_MOVE`), with the caches off and
//! in the no-open mode, every file reads as its bytes of the data file. A mount needs
//! `/dev/fuse`, and `mount(2)` as root or `fusermount3`: a test that cannot mount fails, and says
//! why. Integration target: it mounts FUSE and runs the system's `fusermount3`.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::num::NonZeroUsize;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use fsbench::list::{Entry, FileList};
use fsbench::nullfs::OpenMode;
use fsbench::tree::Tree;
use fsbench::tsnull::{Options, Reply, Transport, serve};
use tierstore_testkit::dir::TestDir;

/// ENOENT.
const ENOENT: i32 = 2;

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

/// `a/b/c.h` (3 bytes), `a/d` (300,000 bytes: several READs of any size the kernel sends),
/// `e` (empty).
fn sample() -> FileList {
    list(&[("a/b/c.h", 3), ("a/d", 300_000), ("e", 0)])
}

fn options(open: OpenMode, timeout: Duration, readers: usize, reply: Reply) -> Options {
    Options {
        entry_timeout: timeout,
        attr_timeout: timeout,
        open,
        readers: NonZeroUsize::new(readers).unwrap(),
        reply,
        splice_min: 16 * 1024,
        move_pages: false,
        allow_other: false,
        transport: Transport::Dev,
    }
}

/// The byte at `i` of the data file: a pattern that differs at every offset a read could be off
/// by.
fn byte(i: u64) -> u8 {
    u8::try_from(i % 251).unwrap()
}

/// A scratch directory with a mount point `mnt` and a data file `data` of `files` holding
/// [`byte`] at every offset.
fn scratch(name: &str, files: &FileList) -> (TestDir, PathBuf, PathBuf) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let mnt = dir.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let data = dir.path().join("data");
    let len = Tree::of(files).unwrap().data_len();
    std::fs::write(&data, (0..len).map(byte).collect::<Vec<u8>>()).unwrap();
    (dir, mnt, data)
}

/// The bytes of a file of [`sample`] as the data file holds them: from `at`, `len` of them.
fn expected(at: u64, len: u64) -> Vec<u8> {
    (at..at + len).map(byte).collect()
}

/// A served mount: `serve` on a thread, ended by closing its input.
struct Served {
    input: Option<std::io::PipeWriter>,
    output: BufReader<std::io::PipeReader>,
    server: Option<std::thread::JoinHandle<Result<(), fsbench::Error>>>,
}

impl Served {
    fn start(files: &FileList, mnt: &Path, data: &Path, options: Options) -> Self {
        let (input, input_end) = std::io::pipe().unwrap();
        let (output_end, output) = std::io::pipe().unwrap();
        let server = {
            let (files, mnt, data) = (files.clone(), mnt.to_path_buf(), data.to_path_buf());
            std::thread::spawn(move || serve(&files, &mnt, &data, options, input, output))
        };
        let mut served = Self {
            input: Some(input_end),
            output: BufReader::new(output_end),
            server: Some(server),
        };
        let said = served.line();
        assert_eq!(
            said, "mounted\n",
            "mount the null file system on tsfuse (needs /dev/fuse, and root or fusermount3)"
        );
        served
    }

    fn line(&mut self) -> String {
        let mut said = String::new();
        self.output.read_line(&mut said).unwrap();
        said
    }

    /// Closes the input: the server unmounts and says what it served.
    fn stop(mut self) -> serde_json::Value {
        drop(self.input.take());
        let said = self.line();
        self.server.take().unwrap().join().unwrap().unwrap();
        serde_json::from_str(&said).unwrap()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        drop(self.input.take());
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// With the caches off and three readers answering by `writev`, every file of the list is there
/// with its size, mode 0444 and the mounting user as owner, and reads as its bytes of the data
/// file; directories list their names; a name that is not there is ENOENT. Every READ went by
/// `pread` (each byte at least once), and the server says it served three readers.
#[test]
fn every_file_reads_as_its_bytes_of_the_data_file_by_writev() {
    let files = sample();
    let (_dir, mnt, data) = scratch("fsbench-tsnull-writev", &files);
    let served = Served::start(
        &files,
        &mnt,
        &data,
        options(OpenMode::Explicit, Duration::ZERO, 3, Reply::Writev),
    );
    let d = std::fs::metadata(mnt.join("a/d")).unwrap();
    assert_eq!((d.len(), d.mode() & 0o7777, d.nlink()), (300_000, 0o444, 1));
    assert_eq!(
        (d.uid(), d.gid()),
        (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw()
        )
    );
    assert_eq!(
        std::fs::read(mnt.join("a/d")).unwrap(),
        expected(4096, 300_000)
    );
    assert_eq!(std::fs::read(mnt.join("a/b/c.h")).unwrap(), expected(0, 3));
    assert!(std::fs::read(mnt.join("e")).unwrap().is_empty());
    let mut names: Vec<String> = std::fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["a", "e"]);
    assert!(std::fs::metadata(mnt.join("a")).unwrap().is_dir());
    let missing = std::fs::metadata(mnt.join("a/zz")).unwrap_err();
    assert_eq!(missing.raw_os_error(), Some(ENOENT));
    let said = served.stop();
    assert_eq!(said["readers"], 3);
    assert_eq!(said["splice_bytes"], 0);
    assert!(said["pread_bytes"].as_u64().unwrap() >= 300_003, "{said}");
    assert!(std::fs::metadata(mnt.join("a")).is_err(), "unmounted");
}

/// In the no-open mode with long timeouts and two readers answering by splice with
/// `SPLICE_F_MOVE`, the first open is answered `ENOSYS` and every open then succeeds; the files
/// read as their bytes, and the READs at and above the threshold were spliced.
#[test]
fn files_read_the_same_by_splice_in_the_no_open_mode() {
    let files = sample();
    let (_dir, mnt, data) = scratch("fsbench-tsnull-splice", &files);
    let mut opts = options(
        OpenMode::NoOpen,
        Duration::from_secs(3600),
        2,
        Reply::Splice,
    );
    opts.move_pages = true;
    let served = Served::start(&files, &mnt, &data, opts);
    for _ in 0..3 {
        assert_eq!(
            std::fs::read(mnt.join("a/d")).unwrap(),
            expected(4096, 300_000)
        );
    }
    assert_eq!(std::fs::read(mnt.join("a/b/c.h")).unwrap(), expected(0, 3));
    let said = served.stop();
    assert_eq!(said["readers"], 2);
    let spliced = said["splice_bytes"].as_u64().unwrap();
    assert!(spliced >= 16 * 1024, "{said}");
    assert!(
        spliced + said["pread_bytes"].as_u64().unwrap() >= 300_003,
        "every byte was read at least once: {said}"
    );
}

/// A mount point that is not there is an error that names the mount, a list with no tree an
/// error that names the path, and an output that refuses the `mounted` line an error that says
/// so, after which nothing is left mounted.
#[test]
fn serve_fails_before_or_after_the_mount_and_leaves_nothing_mounted() {
    let files = sample();
    let (dir, mnt, data) = scratch("fsbench-tsnull-fail", &files);
    let opts = options(OpenMode::Explicit, Duration::ZERO, 1, Reply::Writev);
    let missing = dir.path().join("missing");
    let error = serve(
        &files,
        &missing,
        &data,
        opts,
        std::io::empty(),
        std::io::sink(),
    )
    .unwrap_err();
    assert!(
        error.to_string().starts_with("tsfuse-null-fs: mount: "),
        "{error}"
    );
    let bad = list(&[("a", 1), ("a/b", 1)]);
    let error = serve(&bad, &mnt, &data, opts, std::io::empty(), std::io::sink()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "a/b: a file of the list is named as a directory"
    );
    let error = serve(&files, &mnt, &data, opts, std::io::empty(), Broken).unwrap_err();
    assert_eq!(error.to_string(), "tsfuse-null-fs: write: refused");
    assert!(std::fs::metadata(mnt.join("a")).is_err(), "detached");
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

/// `fsbench tsfuse-null-fs` makes its data file, mounts its list, says `mounted`, serves its
/// files (zeros: the data file it made is a hole), and when its standard input closes unmounts,
/// says what it served and exits 0.
#[test]
fn the_tsfuse_null_fs_command_mounts_until_its_input_ends() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fsbench-tsnull-command");
    let list = dir.path().join("list");
    std::fs::write(&list, format!("{}\n5000\tf\n", fsbench::list::HEADER)).unwrap();
    let mnt = dir.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let data = dir.path().join("data");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fsbench"))
        .args(["tsfuse-null-fs", "--readers", "2", "--reply", "splice"])
        .args(["--splice-min", "4096", "--open", "none", "--list"])
        .arg(&list)
        .arg("--mountpoint")
        .arg(&mnt)
        .arg("--data")
        .arg(&data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut said = String::new();
    output.read_line(&mut said).unwrap();
    assert_eq!(said, "mounted\n");
    assert_eq!(std::fs::metadata(&data).unwrap().len(), 8192, "two pages");
    assert_eq!(std::fs::read(mnt.join("f")).unwrap(), vec![0u8; 5000]);
    child.stdin.take().unwrap().write_all(b"ignored\n").unwrap();
    let mut served = String::new();
    output.read_line(&mut served).unwrap();
    let served: serde_json::Value = serde_json::from_str(&served).unwrap();
    assert_eq!(served["readers"], 2);
    assert!(child.wait().unwrap().success());
    assert!(std::fs::metadata(mnt.join("f")).is_err(), "unmounted");
}

/// `fsbench tsfuse-null-fs --transport uring` serves its files over io_uring where
/// `fuse.enable_uring` is on (and says what its entries registered at the depth asked for), and
/// on `/dev/fuse` saying `Disabled` where it is off; either way the files read as their bytes.
#[test]
fn the_tsfuse_null_fs_asked_for_io_uring_serves_on_the_transport_the_kernel_allows() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fsbench-tsnull-uring");
    let list = dir.path().join("list");
    std::fs::write(&list, format!("{}\n200000\tf\n", fsbench::list::HEADER)).unwrap();
    let mnt = dir.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fsbench"))
        .args([
            "tsfuse-null-fs",
            "--transport",
            "uring",
            "--uring-depth",
            "2",
            "--list",
        ])
        .arg(&list)
        .arg("--mountpoint")
        .arg(&mnt)
        .arg("--data")
        .arg(dir.path().join("data"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut said = String::new();
    output.read_line(&mut said).unwrap();
    assert_eq!(said, "mounted\n");
    assert_eq!(std::fs::read(mnt.join("f")).unwrap(), vec![0u8; 200_000]);
    drop(child.stdin.take());
    let mut served = String::new();
    output.read_line(&mut served).unwrap();
    let served: serde_json::Value = serde_json::from_str(&served).unwrap();
    assert!(child.wait().unwrap().success());
    let enabled =
        std::fs::read_to_string(tsfuse::ENABLE_URING).is_ok_and(|v| matches!(v.trim(), "Y" | "1"));
    if enabled {
        assert_eq!(served["transport"], "io_uring");
        assert_eq!(served["uring_depth"], 2);
        assert_eq!(served["max_pages"], 32);
        let queues = served["uring_queues"].as_u64().unwrap();
        assert_eq!(
            served["uring_registered_bytes"],
            queues * 2 * (288 + 128 * 1024)
        );
    } else {
        assert_eq!(served["transport"], "dev-fuse (Disabled)");
        assert_eq!(served["uring_registered_bytes"], serde_json::Value::Null);
    }
}

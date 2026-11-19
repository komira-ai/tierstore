//! Whole runs: readers and burners started from the `fsbench` executable, over a plain directory
//! and over the null file system mounted by `fsbench null-fs`, measured with this machine's
//! `/proc` and `/sys`. Integration target: it reads the host's `/proc` and `/sys`, mounts FUSE and
//! runs the system's `fusermount3`. No figure of time is asserted; the counts are exact, because
//! every reader is given a number of cycles.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead as _, BufReader};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use fsbench::host::Host;
use fsbench::record::RunRecord;
use fsbench::run::{Plan, run};
use rustix::fs::inotify;
use tierstore_testkit::dir::TestDir;

/// A tree of 40 files of 1 to 40 KiB below `dir/tree`, and its list at `dir/list`.
fn tree(dir: &Path) -> (PathBuf, PathBuf) {
    let root = dir.join("tree");
    for i in 0..40u64 {
        let path = root.join(format!("d{}/f{i:02}", i % 4));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; ((i + 1) * 1024) as usize]).unwrap();
    }
    let list = dir.join("list");
    fsbench::list::write(&root, &list).unwrap();
    (root, list)
}

fn plan(list: &Path, tree: &Path, server_pid: u32) -> Plan {
    Plan {
        exe: PathBuf::from(env!("CARGO_BIN_EXE_fsbench")),
        list: list.to_path_buf(),
        tree: tree.to_path_buf(),
        variant: "test".to_owned(),
        regime: "caches-off".to_owned(),
        runs: 2,
        readers: 3,
        burners: 2,
        cycle_ms: 3_600_000,
        max_cycles: 25,
        read_bytes: 4096,
        // The five largest files: 40 + 39 + 38 + 37 + 36 KiB.
        throughput_budget: 190 * 1024,
        throughput_readers: 2,
        throughput_rounds: 2,
        sample_every: Duration::from_millis(1),
        server_pid,
        as_user: Some((
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        )),
        settings: BTreeMap::from([("server".to_owned(), "none".to_owned())]),
        host: Host::real(),
    }
}

/// What every run of `plan` over the tree of `tree()` measures, whatever serves it.
fn check(records: &[RunRecord], plan: &Plan) {
    assert_eq!(records.len(), plan.runs as usize);
    for (i, record) in records.iter().enumerate() {
        assert_eq!((record.run, record.variant.as_str()), (i as u32, "test"));
        assert_eq!((record.input.files, record.input.bytes), (40, 820 * 1024));
        assert_eq!(
            (
                record.cycles.count,
                record.cycles.errors,
                &record.cycles.first_error
            ),
            (75, 0, &None)
        );
        for h in [
            &record.cycles.stat,
            &record.cycles.open,
            &record.cycles.read,
            &record.cycles.close,
        ] {
            assert_eq!(h.count, 75);
        }
        assert_eq!(record.input.throughput_rounds, plan.throughput_rounds);
        for phase in [&record.cold, &record.warm] {
            // Each of the rounds reads the five files.
            let rounds = u64::from(plan.throughput_rounds);
            assert_eq!(
                (phase.files, phase.bytes, phase.errors),
                (5 * rounds, 190 * 1024 * rounds, 0)
            );
            assert!(phase.cpu.is_some(), "the CPU of each phase is read");
        }
        assert!(record.samples.count >= 1);
        assert_eq!(record.server.pid, plan.server_pid);
        assert!(record.env.kernel.is_some());
        assert_eq!(record.settings["server"], "none");
    }
}

/// A run over a plain directory: every reader makes its cycles, the whole-file reads read the
/// largest files cold and warm, the records are written as lines and a line about each run is
/// logged; the directory is no FUSE mount, so no request is counted as waiting.
#[test]
fn a_run_over_a_directory_writes_a_record_per_run() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fsbench-run-dir");
    let (root, list) = tree(dir.path());
    let plan = plan(&list, &root, std::process::id());
    let (mut out, mut log) = (Vec::new(), Vec::new());
    let records = run(&plan, &mut out, &mut log).unwrap();
    // And as the user that runs the test, without asking to change it.
    let mut own = plan.clone();
    own.as_user = None;
    own.runs = 1;
    check(&run(&own, std::io::sink(), std::io::sink()).unwrap(), &own);
    check(&records, &plan);
    // Compared as JSON: a float read back may differ in its last bit from the one written.
    let written: Vec<String> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let want: Vec<String> = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    assert_eq!(written, want);
    let log = String::from_utf8(log).unwrap();
    assert_eq!(log.lines().count(), 2);
    assert!(log.starts_with("fsbench: test caches-off run 0: "), "{log}");
    assert_eq!(records[0].samples.waiting_mean, None);
}

/// A run over the null file system that `fsbench null-fs` serves, measured as its server: the
/// same counts, the server's executable is the harness's, and the mount's line is recorded. The
/// requests waiting are counted when the machine lists its FUSE connections.
#[test]
fn a_run_over_the_null_file_system_measures_its_server() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fsbench-run-null");
    let (_, list) = tree(dir.path());
    let mnt = dir.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let mut server = Command::new(env!("CARGO_BIN_EXE_fsbench"))
        .args(["null-fs", "--list"])
        .arg(&list)
        .arg("--mountpoint")
        .arg(&mnt)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut said)
        .unwrap();
    assert_eq!(
        said, "mounted\n",
        "the null file system could not mount (needs /dev/fuse and fusermount3)"
    );
    let mut plan = plan(&list, &mnt, server.id());
    plan.runs = 1;
    let result = run(&plan, std::io::sink(), std::io::sink());
    drop(server.stdin.take());
    assert!(server.wait().unwrap().success());
    let records = result.unwrap();
    check(&records, &plan);
    let record = &records[0];
    assert_eq!(
        record.server.exe_sha256,
        fsbench::host::file_sha256(Path::new(env!("CARGO_BIN_EXE_fsbench")))
    );
    assert!(
        record
            .env
            .mountinfo
            .as_deref()
            .unwrap()
            .contains(" - fuse.fsbench-null fsbench-null ")
    );
    let listed = Path::new("/sys/fs/fuse/connections").is_dir();
    assert_eq!(record.samples.waiting_mean.is_some(), listed);
}

/// The `list` command lists the tree, the `run` command appends a record per run to its results
/// file and `report` makes the tables of it; a run whose server is not there fails, says so, and exits 1.
#[test]
fn the_run_and_report_commands_append_and_read_the_results() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fsbench-run-command");
    let (root, _) = tree(dir.path());
    let list = dir.path().join("list-of-the-command");
    let listed = Command::new(env!("CARGO_BIN_EXE_fsbench"))
        .args(["list", "--tree"])
        .arg(&root)
        .arg("--out")
        .arg(&list)
        .output()
        .unwrap();
    assert!(listed.status.success());
    let said = String::from_utf8(listed.stdout).unwrap();
    assert!(
        said.starts_with("40 files, 839680 bytes, sha256 "),
        "{said}"
    );
    let results = dir.path().join("results.jsonl");
    let fsbench = |extra: &[&str], pid: u32| {
        Command::new(env!("CARGO_BIN_EXE_fsbench"))
            .args([
                "run",
                "--variant",
                "test",
                "--regime",
                "caches-off",
                "--runs",
                "1",
            ])
            .args([
                "--readers",
                "3",
                "--burners",
                "1",
                "--max-cycles",
                "25",
                "--throughput-mib",
                "1",
            ])
            .args([
                "--throughput-readers",
                "2",
                "--sample-ms",
                "1",
                "--setting",
                "server=none",
            ])
            .arg("--as-user")
            .arg(format!(
                "{}:{}",
                rustix::process::geteuid().as_raw(),
                rustix::process::getegid().as_raw()
            ))
            .arg("--server-pid")
            .arg(pid.to_string())
            .arg("--list")
            .arg(&list)
            .arg("--tree")
            .arg(&root)
            .arg("--out")
            .arg(&results)
            .args(extra)
            .output()
            .unwrap()
    };
    // The first run reads the files whole once (the default), the second twice.
    for extra in [&[][..], &["--throughput-rounds", "2"][..]] {
        let done = fsbench(extra, std::process::id());
        assert!(
            done.status.success(),
            "{}",
            String::from_utf8_lossy(&done.stderr)
        );
    }
    let records = fsbench::report::read(&results).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].cycles.count, 75);
    let rounds: Vec<u32> = records.iter().map(|r| r.input.throughput_rounds).collect();
    assert_eq!(rounds, [1, 2]);
    for (once, twice) in [
        (&records[0].cold, &records[1].cold),
        (&records[0].warm, &records[1].warm),
    ] {
        assert!(once.files > 0);
        assert_eq!(
            (twice.files, twice.bytes, twice.errors),
            (2 * once.files, 2 * once.bytes, 0),
            "--throughput-rounds 2 reads the same files twice"
        );
    }
    let report = Command::new(env!("CARGO_BIN_EXE_fsbench"))
        .args(["report", "--results"])
        .arg(&results)
        .output()
        .unwrap();
    assert!(report.status.success());
    assert_eq!(
        String::from_utf8(report.stdout).unwrap(),
        fsbench::report::render(&records, 1)
    );
    // A process id above the kernel's limit (`pid_max` is at most 2^22) names no process.
    let failed = fsbench(&[], 1 << 23);
    assert_eq!(failed.status.code(), Some(1));
    let stderr = String::from_utf8(failed.stderr).unwrap();
    assert!(
        stderr.starts_with("fsbench: read /proc/8388608/stat: "),
        "{stderr}"
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

/// A run whose record cannot be written fails, after the run, and says so.
#[test]
fn a_run_whose_record_cannot_be_written_fails() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fsbench-run-broken");
    let (root, list) = tree(dir.path());
    let mut plan = plan(&list, &root, std::process::id());
    plan.runs = 1;
    let error = run(&plan, Broken, std::io::sink()).unwrap_err();
    assert_eq!(error.to_string(), "results: write a record: refused");
}

/// The process ids of this process's children that run `fsbench reader` over `list`.
fn readers_of(list: &Path) -> Vec<i32> {
    let me = std::process::id().to_string();
    let want = list.as_os_str().as_encoded_bytes();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let parent = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1));
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            parent == Some(me.as_str())
                && cmdline.split(|&b| b == 0).any(|arg| arg == b"reader")
                && cmdline.split(|&b| b == 0).any(|arg| arg == want)
        })
        .collect()
}

/// The opens of the files one directory below a tree's root, as inotify reports them.
struct Opens {
    inotify: OwnedFd,
    /// The name of the directory of each watch.
    dirs: BTreeMap<i32, String>,
}

impl Opens {
    /// Watches the opens of the files in each directory of `root` (the files of `tree()` are one
    /// directory below it).
    fn watch(root: &Path) -> Self {
        let flags = inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK;
        let inotify = inotify::init(flags).unwrap();
        let mut dirs = BTreeMap::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let wd = inotify::add_watch(&inotify, entry.path(), inotify::WatchFlags::OPEN).unwrap();
            dirs.insert(wd, entry.file_name().into_string().unwrap());
        }
        Self { inotify, dirs }
    }

    /// Waits until, for each of `readers` readers, a file of its share of `list` was opened, or
    /// until `ended`; returns the readers a file of whose share was opened. Reader `i`'s share is
    /// every `readers`-th file of the list from the `i`-th, and a reader opens a file of the tree
    /// only in its cycles, so an open of a file of its share means its go was written to it.
    fn wait_for_shares(
        &self,
        list: &Path,
        readers: usize,
        ended: impl Fn() -> bool,
    ) -> BTreeSet<usize> {
        let files = fsbench::list::read(list).unwrap().files;
        let share: BTreeMap<&str, usize> = files
            .iter()
            .enumerate()
            .map(|(i, entry)| (entry.path.as_str(), i % readers))
            .collect();
        let mut opened = BTreeSet::new();
        let mut buf = [MaybeUninit::uninit(); 4096];
        let mut events = inotify::Reader::new(&self.inotify, &mut buf);
        while opened.len() < readers && !ended() {
            match events.next() {
                Ok(event) => {
                    let dir = self.dirs.get(&event.wd());
                    let name = event.file_name().and_then(|n| n.to_str().ok());
                    if let (Some(dir), Some(name)) = (dir, name)
                        && let Some(&reader) = share.get(format!("{dir}/{name}").as_str())
                    {
                        opened.insert(reader);
                    }
                }
                Err(rustix::io::Errno::AGAIN) => std::thread::yield_now(),
                Err(e) => panic!("read the opens: {e}"),
            }
        }
        opened
    }
}

/// A reader that dies during its cycles ends the run with an error that names it, and leaves no
/// reader or burner behind. The readers have no limit on their cycles and an hour to make them,
/// and they are killed only once each has opened a file of its share, that is once each has its
/// go: a reader killed before its go was written would end the run in the write of the go.
#[test]
fn a_reader_that_dies_ends_the_run_with_an_error() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fsbench-run-killed");
    let (root, list) = tree(dir.path());
    let mut plan = plan(&list, &root, std::process::id());
    plan.max_cycles = 0;
    plan.runs = 1;
    let opens = Opens::watch(&root);
    let running = std::thread::spawn(move || run(&plan, std::io::sink(), std::io::sink()));
    let cycling = opens.wait_for_shares(&list, 3, || running.is_finished());
    assert_eq!(cycling, BTreeSet::from([0, 1, 2]), "every reader cycles");
    // Each reader opened a file, so each is this process's child, running `fsbench reader`.
    let readers = readers_of(&list);
    assert_eq!(readers.len(), 3);
    for pid in readers {
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    }
    let error = running.join().unwrap().unwrap_err();
    assert_eq!(error.to_string(), "reader 0: ended without its answer");
    assert!(readers_of(&list).is_empty(), "no reader is left");
}

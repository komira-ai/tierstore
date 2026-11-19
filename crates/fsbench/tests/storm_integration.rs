//! The storm over a real mount, through `fsbench::storm::run` and through the command line: on
//! `/dev/fuse`, and asked for io_uring (served on io_uring where `fuse.enable_uring` is on, and
//! otherwise on `/dev/fuse`, saying why), with buffered and with direct reads, every phase runs
//! its probes and its storm threads and is recorded. Every wait is on an event (a storm thread's
//! first answered read, a probe's count of `stat`s); no figure of time is asserted. A mount
//! needs `/dev/fuse`, and `mount(2)` as root or `fusermount3`: a test that cannot mount fails,
//! and says why. Integration target: it mounts FUSE and runs the system's `fusermount3`.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::collections::BTreeMap;
use std::num::{NonZeroU16, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fsbench::host::Host;
use fsbench::storm::{Plan, other_cpu, run, usable_cpus};
use fsbench::stormfs::ColdRead;
use fsbench::stormrec::StormRecord;
use fsbench::tsnull::Transport;
use tierstore_testkit::dir::TestDir;

/// The probes' `stat`s in each phase.
const PROBES: u64 = 25;

/// Whether this machine's `fuse.enable_uring` is on.
fn uring_enabled() -> bool {
    std::fs::read_to_string(tsfuse::ENABLE_URING).is_ok_and(|v| matches!(v.trim(), "Y" | "1"))
}

fn plan(mountpoint: &Path, transport: Transport, read: ColdRead) -> Plan {
    let host = Host::real();
    let cpus = usable_cpus(&host).unwrap();
    let storm_cpu = cpus[0];
    Plan {
        mountpoint: mountpoint.to_path_buf(),
        label: "test".to_owned(),
        runs: 1,
        transport,
        readers: NonZeroUsize::new(2).unwrap(),
        park: Duration::from_micros(200),
        read,
        per_cpu: 2,
        other_cpu: other_cpu(&cpus, storm_cpu, None).unwrap(),
        cpus,
        storm_cpu,
        window: Duration::from_secs(3600),
        max_probes: PROBES,
        sample_every: Duration::from_millis(1),
        settings: BTreeMap::from([("k".to_owned(), "v".to_owned())]),
        host,
    }
}

/// A scratch directory with an empty mount point.
fn scratch(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let mnt = dir.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    (dir, mnt)
}

/// Runs `plan` once: the record, which is also the one line written.
fn run_once(plan: &Plan) -> StormRecord {
    let (mut out, mut log) = (Vec::new(), Vec::new());
    let records = run(plan, &mut out, &mut log).unwrap_or_else(|e| {
        panic!("run the storm (needs /dev/fuse, and root or fusermount3): {e}")
    });
    assert_eq!(records.len(), 1);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        serde_json::to_string(&records[0]).unwrap() + "\n",
        "the record is the one line written"
    );
    let line = records.into_iter().next().unwrap();
    assert_eq!(
        String::from_utf8(log).unwrap(),
        format!("storm test run 0: {}, 3 phases\n", line.transport)
    );
    line
}

/// Every phase of `record` of `plan`: its storm threads, every probe's `stat`s answered with no
/// error, every storm read answered with no error, and the samples taken.
fn check_phases(record: &StormRecord, plan: &Plan) {
    let per = plan.per_cpu;
    let names: Vec<(&str, u32, u32)> = record
        .phases
        .iter()
        .map(|p| (p.name.as_str(), p.storm_threads, p.stormed_cpus))
        .collect();
    let n = plan.cpus.len() as u32;
    assert_eq!(
        names,
        [
            ("idle", 0, 0),
            ("one-cpu", per, 1),
            ("every-cpu", per * n, n)
        ]
    );
    for phase in &record.phases {
        assert_eq!(phase.storm_errors, 0, "{}", phase.name);
        assert!(phase.held.samples >= 1 && phase.parked.samples >= 1);
        for probe in &phase.probes {
            assert_eq!(
                (probe.stat.count, probe.errors),
                (PROBES, 0),
                "{} {}",
                phase.name,
                probe.role
            );
        }
        let roles: Vec<(&str, usize, bool)> = phase
            .probes
            .iter()
            .map(|p| (p.role.as_str(), p.cpu, p.stormed))
            .collect();
        let mut expected = vec![("storm-cpu", plan.storm_cpu, phase.name != "idle")];
        if let Some(other) = plan.other_cpu {
            expected.push(("other-cpu", other, phase.name == "every-cpu"));
        }
        assert_eq!(roles, expected, "{}", phase.name);
    }
    assert_eq!(record.settings["k"], "v");
    assert_eq!(
        (record.readers, record.park_us, record.per_cpu),
        (2, 200, per)
    );
}

/// On `/dev/fuse` with buffered reads, every phase runs and no ring entry is ever held.
#[test]
fn a_storm_on_dev_fuse_runs_every_phase() {
    let (_dir, mnt) = scratch("fsbench-storm-dev");
    let plan = plan(&mnt, Transport::Dev, ColdRead::Buffered);
    let record = run_once(&plan);
    check_phases(&record, &plan);
    assert_eq!(record.transport, "dev-fuse (NotAsked)");
    assert_eq!(record.uring_registered_bytes, None);
    assert!(record.phases.iter().all(|p| p.held.max == 0));
    assert_eq!(record.read, "buffered");
    assert!(
        std::fs::read_dir(&mnt).unwrap().next().is_none(),
        "unmounted"
    );
}

/// Asked for io_uring with direct reads, the session serves on io_uring where the parameter is
/// on (every possible CPU's queue registered at depth 2, and what it registers recorded), and
/// on `/dev/fuse` saying `Disabled` where it is off; either way every phase runs.
#[test]
fn a_storm_asked_for_io_uring_runs_on_the_transport_the_kernel_allows() {
    let (_dir, mnt) = scratch("fsbench-storm-uring");
    let uring = tsfuse::UringConfig {
        depth: NonZeroU16::new(2).unwrap(),
        ..tsfuse::UringConfig::default()
    };
    let plan = plan(&mnt, Transport::Uring(uring), ColdRead::Direct);
    let record = run_once(&plan);
    check_phases(&record, &plan);
    assert_eq!(record.read, "direct");
    if uring_enabled() {
        assert_eq!(record.transport, "io_uring");
        assert_eq!(record.uring_depth, Some(2));
        let queues = record.uring_queues.unwrap();
        assert_eq!(
            record.uring_registered_bytes,
            Some(u64::from(queues) * 2 * (288 + 128 * 1024))
        );
    } else {
        assert_eq!(record.transport, "dev-fuse (Disabled)");
        assert_eq!(record.uring_registered_bytes, None);
    }
}

/// A mount point that is not there fails the storm before any record, and says so.
#[test]
fn a_storm_that_cannot_mount_fails_and_writes_nothing() {
    let (dir, _mnt) = scratch("fsbench-storm-fail");
    let plan = plan(
        &dir.path().join("missing"),
        Transport::Dev,
        ColdRead::Buffered,
    );
    let mut out = Vec::new();
    let error = run(&plan, &mut out, &mut Vec::new()).unwrap_err();
    assert!(error.to_string().starts_with("storm: mount: "), "{error}");
    assert!(out.is_empty());
}

/// `fsbench storm` appends a record per run to its results file, and `fsbench storm-report`
/// makes the tables of it; a storm CPU that is not online is refused before anything mounts.
#[test]
fn the_storm_commands_record_and_report() {
    let (dir, mnt) = scratch("fsbench-storm-command");
    let out = dir.path().join("storm.jsonl");
    let fsbench = |args: Vec<String>| {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = fsbench::cli::run(
            std::iter::once("fsbench".into()).chain(args.into_iter().map(Into::into)),
            Path::new(env!("CARGO_BIN_EXE_fsbench")),
            Box::new(std::io::empty()),
            &mut stdout,
            &mut stderr,
        );
        (
            code,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    };
    let storm = |extra: &[&str]| {
        let mut args: Vec<String> = [
            "storm",
            "--label",
            "cmd",
            "--runs",
            "2",
            "--per-cpu",
            "1",
            "--park-us",
            "100",
            "--max-probes",
            "5",
            "--sample-ms",
            "1",
            "--setting",
            "a=b",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        args.extend(["--mountpoint".to_owned(), mnt.display().to_string()]);
        args.extend(["--out".to_owned(), out.display().to_string()]);
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        fsbench(args)
    };
    let (code, _, stderr) = storm(&[]);
    assert_eq!(code, std::process::ExitCode::SUCCESS, "{stderr}");
    assert_eq!(stderr.lines().count(), 2, "a line per run: {stderr}");
    let records = fsbench::stormrec::read(&out).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].run, 1);
    assert_eq!(records[0].settings["a"], "b");
    let (code, report, _) = fsbench(vec![
        "storm-report".to_owned(),
        "--results".to_owned(),
        out.display().to_string(),
        "--discard".to_owned(),
        "1".to_owned(),
    ]);
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    assert!(
        report.contains("| cmd | dev-fuse (NotAsked) | - | - | 0.10 | buffered | 1 | 1 | 1 of 2 |"),
        "{report}"
    );
    assert!(
        report.contains("| cmd | one-cpu | storm-cpu | yes |"),
        "{report}"
    );
    let (code, _, stderr) = storm(&["--storm-cpu", "100000"]);
    assert_eq!(code, std::process::ExitCode::from(1));
    assert_eq!(stderr, "fsbench: CPU 100000 is not online\n");
    assert_eq!(
        fsbench::stormrec::read(&out).unwrap().len(),
        2,
        "nothing more"
    );
}

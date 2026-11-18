use super::*;
use crate::clock::StepClock;
use crate::loopfs::tests::Script;
use crate::report::Out;
use crate::testutil::Fake;

fn config(f: &Fake) -> Config {
    Config {
        targets: vec![Target {
            label: "rootfs".into(),
            dir: f.drive("root"),
            devs: vec!["sda".into()],
        }],
        blocks_kib: vec![128, 1024],
        jobs: vec![1, 2],
        modes: vec![WriteMode::BufferedSync, WriteMode::Direct],
        warmup: Duration::from_millis(2),
        measure: Duration::from_millis(80),
        sync_files: 2,
        sync_commits: 3,
        sync_background: 1,
        mixed_read_set: 4 << 20,
        punch_bytes: 64 * MIB,
        dontneed_bytes: 40 * MIB,
        dontneed_cap: 8 * MIB,
        diskstats: f.diskstats(),
        tests: ["write", "fdatasync", "mixed", "punch", "dontneed"]
            .map(String::from)
            .to_vec(),
        meminfo: f.meminfo(),
    }
}

fn clock() -> StepClock {
    StepClock::new(Duration::from_millis(1), 0)
}

/// A coarser clock for the tests that run seconds of commits.
fn coarse() -> StepClock {
    StepClock::new(Duration::from_millis(10), 0)
}

#[test]
fn write_grid_has_a_row_per_cell_and_leaves_no_files() {
    let f = Fake::new("cache", &["sda"]);
    let cfg = config(&f);
    let rows = write_grid(&cfg, &cfg.targets[0], &clock()).unwrap();
    assert_eq!(rows.len(), 8);
    assert!(
        rows.iter()
            .all(|r| r.test == "write" && r.mb_s > 0.0 && r.lat.is_some())
    );
    assert_eq!(
        rows[0].params,
        "block=128KiB jobs=1 mode=buffered+fdatasync/32MiB"
    );
    assert_eq!(std::fs::read_dir(&cfg.targets[0].dir).unwrap().count(), 0);
    assert_eq!(WriteMode::Direct.name(), "direct");
}

#[test]
fn a_buffered_writer_syncs_every_32_mib() {
    let f = Fake::new("cache", &["sda"]);
    let c = StepClock::new(Duration::from_micros(10), 0);
    let span = Span::starting(&c, Duration::ZERO, Duration::from_millis(1));
    let (bytes, lat) = writer(
        &f.drive("w").join("x"),
        1 << 20,
        WriteMode::BufferedSync,
        &c,
        span,
    )
    .unwrap();
    // 1 ms of 10 us steps, two reads a write: about 50 writes of 1 MiB, so a sync ran.
    assert!(bytes > 32 * MIB, "{bytes}");
    assert_eq!(lat.len() as u64 * (1 << 20), bytes);
}

#[test]
fn writer_and_jobs_report_failures() {
    let f = Fake::new("cache", &["sda"]);
    let c = clock();
    let span = Span::starting(&c, Duration::ZERO, Duration::from_millis(5));
    let gone = f.path().join("none/x");
    assert!(writer(&gone, 4096, WriteMode::Direct, &c, span).is_err());
    assert!(
        writers(
            &f.path().join("none"),
            "t",
            2,
            4096,
            WriteMode::Direct,
            &c,
            span
        )
        .is_err()
    );
    // An unaligned O_DIRECT write is refused by the file system.
    let d = f.drive("u");
    assert!(writer(&d.join("y"), 100, WriteMode::Direct, &c, span).is_err());
}

#[test]
fn fsync_test_times_every_commit_of_every_file_idle_and_loaded() {
    let f = Fake::new("cache", &["sda"]);
    let cfg = config(&f);
    let t = &cfg.targets[0];
    let idle = fsync_test(&cfg, t, 0, &coarse()).unwrap();
    assert_eq!(idle.lat.unwrap().n, 6);
    assert_eq!(
        idle.params,
        "files=2 batch=1MiB every=1s commits=3 background=0"
    );
    let loaded = fsync_test(&cfg, t, 1, &coarse()).unwrap();
    assert_eq!(loaded.lat.unwrap().n, 6);
    assert_eq!(std::fs::read_dir(&t.dir).unwrap().count(), 0);
    let mut bad = t.clone();
    bad.dir = f.path().join("none");
    assert!(fsync_test(&cfg, &bad, 0, &coarse()).is_err());
}

#[test]
fn mixed_test_reads_and_writes_and_reuses_its_read_set() {
    let f = Fake::new("cache", &["sda"]);
    let cfg = config(&f);
    let t = &cfg.targets[0];
    let r = mixed_test(&cfg, t, &clock()).unwrap();
    assert!(r.mb_s > 0.0);
    assert!(r.extra["write_mb_s"] > 0.0);
    // The read set stays for the next run and is not rewritten; a wrong-size file is.
    std::fs::write(t.dir.join("b-read-0"), b"short").unwrap();
    mixed_test(&cfg, t, &clock()).unwrap();
    assert_eq!(
        std::fs::metadata(t.dir.join("b-read-0")).unwrap().len(),
        MIB
    );
    let mut bad = t.clone();
    bad.dir = f.path().join("none");
    assert!(mixed_test(&cfg, &bad, &clock()).is_err());
}

#[test]
fn a_mixed_job_syncs_its_writes_every_32_mib() {
    let f = Fake::new("cache", &["sda"]);
    let d = f.drive("m");
    let set = vec![d.join("r")];
    write_data_file(&set[0], MIB, 4096, 0).unwrap();
    let c = StepClock::new(Duration::from_millis(1), 0);
    let span = Span::starting(&c, Duration::ZERO, Duration::from_millis(400));
    let (rd, wr, lat) = mixed_job(0, &set, MIB, &d, &c, span).unwrap();
    // About 200 operations, 30% of them 1 MiB writes: more than 32 MiB written, so one sync.
    assert!(wr > 32 * MIB, "{wr}");
    assert_eq!(rd, lat.len() as u64 * (128 << 10));
}

#[test]
fn mixed_job_reports_a_missing_read_file() {
    let f = Fake::new("cache", &["sda"]);
    let c = clock();
    let span = Span::starting(&c, Duration::ZERO, Duration::from_millis(5));
    let d = f.drive("m");
    assert!(mixed_job(0, &[d.join("none")], MIB, &d, &c, span).is_err());
}

#[test]
fn punch_frees_half_the_file_and_reports_the_image() {
    let f = Fake::new("cache", &["sda"]);
    let cfg = config(&f);
    let t = &cfg.targets[0];
    let img = f.path().join("image");
    std::fs::write(&img, vec![1u8; 1 << 16]).unwrap();
    let r = punch_test(&cfg, t, Some(&img), None).unwrap();
    assert!((r.extra["file_mib_before"] - 64.0).abs() < 1.0);
    assert!((r.extra["file_mib_after"] - 32.0).abs() < 1.0);
    assert!(r.extra.contains_key("image_freed_mib"));
    assert!(!r.extra.contains_key("image_freed_after_fstrim_mib"));
    let script = Script::default();
    let r = punch_test(&cfg, t, Some(&img), Some((&script, Path::new("/mnt/x")))).unwrap();
    assert!(r.extra.contains_key("image_freed_after_fstrim_mib"));
    assert_eq!(script.seen.lock().unwrap()[0], "fstrim /mnt/x");
    let r = punch_test(&cfg, t, None, None).unwrap();
    assert!(!r.extra.contains_key("image_freed_mib"));
    let fail = Script {
        fail: vec!["fstrim"],
        ..Script::default()
    };
    assert!(punch_test(&cfg, t, Some(&img), Some((&fail, Path::new("/m")))).is_err());
    assert!(punch_test(&cfg, t, Some(&f.path().join("none")), None).is_err());
}

#[test]
fn dontneed_policies_drop_what_they_say() {
    let f = Fake::new("cache", &["sda"]);
    let cfg = config(&f);
    let t = &cfg.targets[0];
    // The page cache reads 0 before the writes and 5 MiB after them, at every call pair.
    let calls = Mutex::new(0u64);
    let resident = || {
        let mut n = calls.lock().unwrap();
        *n += 1;
        Ok(if *n % 2 == 1 { 0 } else { 5 * MIB })
    };
    for p in [Policy::Never, Policy::AtCommit, Policy::Cap] {
        let r = dontneed_test(&cfg, t, p, &resident, &clock()).unwrap();
        assert_eq!(r.lat.unwrap().n, 40, "{p:?}");
        assert!((r.extra["cache_grew_mib"] - 5.0).abs() < 1e-9);
        assert!(
            r.params
                .starts_with(&format!("policy={p:?} bytes=40MiB cap=8MiB"))
        );
    }
    assert_eq!(*calls.lock().unwrap(), 6);
    // A cap under one commit batch drops only what was written (no step past the end).
    let tight = Config {
        dontneed_bytes: 4 * MIB,
        dontneed_cap: MIB,
        ..cfg.clone()
    };
    assert_eq!(
        dontneed_test(&tight, t, Policy::Cap, &resident, &clock())
            .unwrap()
            .lat
            .unwrap()
            .n,
        4
    );
    let failing = || Err(Error::Invalid("no meminfo".into()));
    assert!(dontneed_test(&cfg, t, Policy::Never, &failing, &clock()).is_err());
}

#[test]
fn cached_bytes_reads_meminfo_and_refuses_a_file_without_it() {
    let f = Fake::new("cache", &[]);
    let p = f.path().join("meminfo");
    std::fs::write(&p, "MemTotal: 100 kB\nCached:          2048 kB\n").unwrap();
    assert_eq!(cached_bytes(&p).unwrap(), 2048 * 1024);
    std::fs::write(&p, "MemTotal: 100 kB\n").unwrap();
    assert!(
        cached_bytes(&p)
            .unwrap_err()
            .to_string()
            .ends_with("has no Cached: line")
    );
    assert!(cached_bytes(&f.path().join("none")).is_err());
}

#[test]
fn run_covers_every_test_and_writes_a_table_per_test() {
    let f = Fake::new("cache", &["sda"]);
    let mut cfg = config(&f);
    cfg.blocks_kib = vec![1024];
    cfg.jobs = vec![1];
    cfg.modes = vec![WriteMode::Direct];
    let script = Script::default();
    let out = Out::new(&f.path().join("out"), "b").unwrap();
    let rows = run(&cfg, &[], &script, &coarse(), &out, None).unwrap();
    let tests: Vec<&str> = rows.iter().map(|r| r.test.as_str()).collect();
    assert_eq!(
        tests,
        [
            "write",
            "fdatasync",
            "fdatasync",
            "mixed",
            "punch",
            "dontneed",
            "dontneed",
            "dontneed"
        ]
    );
    let md = std::fs::read_to_string(out.path("md")).unwrap();
    for test in ["write", "fdatasync", "mixed", "punch", "dontneed"] {
        assert!(
            md.contains(&format!("### B, cache device: {test}")),
            "{test}"
        );
    }
    assert!(md.contains("| rootfs | jobs=8 read=70%x128KiB-random-direct"));
    assert_eq!(std::fs::read_dir(&cfg.targets[0].dir).unwrap().count(), 0);
    // Without a meminfo the DONTNEED tests fail, and with them the run.
    let mut gone = cfg.clone();
    gone.meminfo = f.path().join("none");
    let extras = [Extras {
        image: None,
        fstrim: None,
    }];
    assert!(run(&gone, &extras, &script, &coarse(), &out, None).is_err());
    assert!(tables(&[]).is_empty());
}

#[test]
fn run_runs_only_the_chosen_tests() {
    let f = Fake::new("cache", &["sda"]);
    let mut cfg = config(&f);
    cfg.tests = vec!["punch".into()];
    let out = Out::new(&f.path().join("out"), "b").unwrap();
    let rows = run(&cfg, &[], &Script::default(), &clock(), &out, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].test, "punch");
}

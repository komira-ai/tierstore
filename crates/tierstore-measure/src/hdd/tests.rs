use super::*;
use crate::clock::StepClock;
use crate::testutil::Fake;

fn config(f: &Fake, sets: &[&str]) -> Config {
    Config {
        drives: vec![
            Drive {
                dir: f.drive("hdd1"),
                dev: "sdb".into(),
            },
            Drive {
                dir: f.drive("hdd2"),
                dev: "sdc".into(),
            },
        ],
        sets: sets.iter().map(|s| (*s).to_string()).collect(),
        sizes_mib: vec![1],
        streams: vec![1, 2],
        modes: vec![ReadMode::Direct, ReadMode::Readahead(512)],
        patterns: vec![Pattern::Adjacent, Pattern::Scattered],
        files: 2,
        file_size: 2 * MIB,
        warmup: Duration::from_millis(2),
        measure: Duration::from_millis(60),
        sys: f.sys(),
    }
}

#[test]
fn read_modes_round_trip_and_refuse_other_names() {
    for m in [ReadMode::Direct, ReadMode::Readahead(8192)] {
        assert_eq!(ReadMode::parse(&m.name()).unwrap(), m);
    }
    for bad in ["ra", "rax", "buffered"] {
        assert!(ReadMode::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn prepare_writes_region_files_once_and_replaces_a_wrong_size() {
    let f = Fake::new("hdd", &["sdb", "sdc"]);
    let cfg = config(&f, &["one"]);
    prepare(&cfg).unwrap();
    let files = region_files(&cfg.drives[0].dir, 2);
    assert!(
        files
            .iter()
            .all(|p| std::fs::metadata(p).unwrap().len() == 2 * MIB)
    );
    std::fs::write(&files[1], b"short").unwrap();
    prepare(&cfg).unwrap();
    assert_eq!(std::fs::metadata(&files[1]).unwrap().len(), 2 * MIB);
    let mut bad = cfg.clone();
    bad.file_size = 100;
    std::fs::remove_file(&files[0]).unwrap();
    assert!(prepare(&bad).is_err());
}

#[test]
fn the_grid_runs_every_cell_restores_readahead_and_writes_tables() {
    let f = Fake::new("hdd", &["sdb", "sdc"]);
    let cfg = config(&f, &["one", "both"]);
    prepare(&cfg).unwrap();
    let out = Out::new(&f.path().join("out"), "a").unwrap();
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let rows = run(&cfg, &clock, &f.diskstats(), &out, None).unwrap();
    assert_eq!(rows.len(), 2 * 2 * 2 * 2);
    assert!(rows.iter().all(|r| r.mb_s > 0.0));
    let both = rows.iter().find(|r| r.set == "both").unwrap();
    assert_eq!(both.io.len(), 2);
    for dev in ["sdb", "sdc"] {
        let ra = QueueSetting::new(&cfg.sys, dev, "read_ahead_kb")
            .read()
            .unwrap();
        assert_eq!(ra, "8192", "{dev} restored");
    }
    let md = std::fs::read_to_string(out.path("md")).unwrap();
    assert!(md.contains("### A, raw HDD reads, drives: one"));
    assert!(md.contains("### A, raw HDD reads, drives: both"));
    assert!(md.contains("### A, derived"));
    let lines = std::fs::read_to_string(out.path("jsonl")).unwrap();
    assert_eq!(lines.lines().count(), 17);
    assert!(lines.lines().last().unwrap().starts_with("{\"derived\""));
}

#[test]
fn a_bad_set_or_a_missing_device_setting_fails_the_cell() {
    let f = Fake::new("hdd", &["sdb"]);
    let cfg = config(&f, &["one"]);
    prepare(&cfg).unwrap();
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let d = f.diskstats();
    let e = cell(
        &cfg,
        &clock,
        "three",
        1,
        1,
        ReadMode::Direct,
        Pattern::Adjacent,
        &d,
    );
    assert_eq!(
        e.unwrap_err().to_string(),
        "drive set three: want one or both"
    );
    // sdc has no sysfs entry in this machine.
    let e = cell(
        &cfg,
        &clock,
        "both",
        1,
        1,
        ReadMode::Readahead(512),
        Pattern::Adjacent,
        &d,
    );
    assert!(e.unwrap_err().to_string().starts_with("read "));
    let mut none = cfg.clone();
    none.drives.clear();
    let e = cell(
        &none,
        &clock,
        "one",
        1,
        1,
        ReadMode::Direct,
        Pattern::Adjacent,
        &d,
    );
    assert_eq!(e.unwrap_err().to_string(), "no drive");
}

fn row(set: &str, streams: usize, mode: &str, pattern: &str, req: u64, mb: f64, p50: f64) -> Row {
    Row {
        set: set.into(),
        req_mib: req,
        streams,
        mode: mode.into(),
        pattern: pattern.into(),
        mb_s: mb,
        lat: summarize(&[p50]),
        io: BTreeMap::new(),
        seen: Seen::default(),
    }
}

#[test]
fn derive_takes_the_best_adjacent_rate_and_the_scattered_median() {
    let rows = vec![
        row("one", 1, "direct", "adjacent", 4, 150.0, 30.0),
        row("one", 1, "direct", "adjacent", 8, 180.0, 50.0),
        row("one", 2, "direct", "adjacent", 8, 999.0, 50.0),
        row("one", 1, "direct", "scattered", 1, 20.0, 20.0),
    ];
    let d = derive(&rows).unwrap();
    assert!((d.media_mb_s - 180.0).abs() < 1e-9);
    // 20 ms less 1 MiB at 180 MB/s (5.83 ms) = 14.17 ms; x 180 / 1.048576 / 1000 = 2.43 -> 3.
    assert!((d.pos_ms - (20.0 - 1.048_576 / 180.0 * 1000.0)).abs() < 1e-9);
    assert_eq!(d.gap_cells, 3);
    assert_eq!(derive(&rows[..3]), None);
    assert_eq!(derive(&rows[3..]), None);
    let mut no_lat = rows.clone();
    no_lat[3].lat = None;
    assert_eq!(derive(&no_lat), None);
    // A table row without iostat columns shows zeros, and one without latency shows dashes.
    let t = tables(&no_lat);
    assert_eq!(t.len(), 1);
    assert!(t[0].markdown().contains(
        "| 1 | 1 | direct | scattered | 20.0 | - | - | 0 | 0.0 | 0.00 | 0.0 | 0.0 | 0 |"
    ));
}

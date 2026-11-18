use super::*;
use crate::clock::StepClock;
use crate::loopfs::tests::Script;
use crate::testutil::Fake;

const FIEMAP: &str =
    "/x/p.pack:\n 0: [0..2047]: 100..2147\n 1: [2048..4095]: hole\n 2: [4096..8191]: 9000..13095\n";

fn script() -> Script {
    Script {
        answers: vec![("xfs_io -r -c fiemap", FIEMAP.into())],
        ..Script::default()
    }
}

fn clock() -> StepClock {
    StepClock::new(Duration::from_millis(1), 0)
}

fn config(f: &Fake) -> Config {
    Config {
        dir: f.drive("hdd1"),
        dev: "sdb".into(),
        sizes_mib: vec![1],
        writers: vec![1, 2],
        measure: Duration::from_millis(20),
        sustained: 3 * MIB,
        pack: MIB,
        window: 2 * MIB,
        rewrite_cap: 2 * MIB,
        keep_free: 0.0,
        diskstats: f.diskstats(),
    }
}

#[test]
fn a_pack_is_written_whole_and_renamed_into_place() {
    let f = Fake::new("write", &["sdb"]);
    let d = f.drive("d");
    for (i, how) in [
        DEFAULT,
        How {
            prealloc: false,
            depth: 3,
            chunk: 8192,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("p{i}");
        let secs = write_pack(&d, &name, 5 * 8192, how, 1, &clock()).unwrap();
        assert!(secs > 0.0);
        let p = d.join(format!("{name}.pack"));
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 5 * 8192);
        assert!(!d.join(format!(".{name}.up")).exists());
    }
    write_pack(&d, "empty", 0, DEFAULT, 0, &clock()).unwrap();
    assert_eq!(std::fs::metadata(d.join("empty.pack")).unwrap().len(), 0);
    for how in [
        How {
            chunk: 100,
            ..DEFAULT
        },
        How {
            chunk: 0,
            ..DEFAULT
        },
        How {
            depth: 0,
            ..DEFAULT
        },
    ] {
        assert!(matches!(
            write_pack(&d, "x", 4096, how, 0, &clock()),
            Err(Error::Invalid(_))
        ));
    }
    assert!(matches!(
        write_pack(&d, "x", 100, DEFAULT, 0, &clock()),
        Err(Error::Invalid(_))
    ));
    assert!(write_pack(&f.path().join("none"), "x", 4096, DEFAULT, 0, &clock()).is_err());
    // A leftover temporary file of the same name is refused, not overwritten.
    std::fs::write(d.join(".y.up"), b"").unwrap();
    assert!(write_pack(&d, "y", 4096, DEFAULT, 0, &clock()).is_err());
}

#[test]
fn extents_counts_data_extents_not_holes() {
    let s = script();
    assert_eq!(extents(&s, Path::new("/x/p.pack")).unwrap(), 2);
    assert_eq!(s.seen.lock().unwrap()[0], "xfs_io -r -c fiemap /x/p.pack");
    let fail = Script {
        fail: vec!["xfs_io"],
        ..Script::default()
    };
    assert!(extents(&fail, Path::new("/x")).is_err());
    set_extsize(&s, Path::new("/d"), 64).unwrap();
    assert_eq!(s.seen.lock().unwrap()[1], "xfs_io -c extsize 64m /d");
}

#[test]
fn cells_cover_size_writers_hint_and_fallocate() {
    let f = Fake::new("write", &["sdb"]);
    let cfg = config(&f);
    let s = script();
    let rows = cells(&cfg, &s, &clock()).unwrap();
    assert_eq!(rows.len(), 2 * 2 * 2);
    assert_eq!(
        rows[0].params,
        "size=1MiB writers=1 extsize64m=false fallocate=false"
    );
    assert!(
        rows.iter()
            .all(|r| r.mb_s > 0.0 && r.extents.unwrap().p50 == 2.0)
    );
    assert!(rows.iter().all(|r| r.io.is_some()));
    let hints = s
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.contains("extsize 64m"))
        .count();
    assert_eq!(hints, 4);
    assert!(!cfg.dir.join("c5-cell").exists());
    let fail = Script {
        fail: vec!["xfs_io"],
        ..Script::default()
    };
    assert!(cells(&cfg, &fail, &clock()).is_err());
}

#[test]
fn sustained_writes_the_curve_deletes_a_third_and_rewrites() {
    let f = Fake::new("write", &["sdb"]);
    let cfg = config(&f);
    let rows = sustained(&cfg, &script(), &clock()).unwrap();
    let phases: Vec<&str> = rows.iter().map(|r| r.phase.as_str()).collect();
    assert_eq!(phases, ["sustained", "sustained", "rewrite"]);
    assert_eq!(rows[1].params, "written=0GiB");
    assert!(ratio(&rows).unwrap() > 0.0);
    assert!(!cfg.dir.join("c5-sustained").exists());
    let md: Vec<String> = tables(&rows).iter().map(Table::markdown).collect();
    assert_eq!(md.len(), 3);
    assert!(md[2].contains("rewrite after deletes over fresh"));
}

#[test]
fn sustained_refuses_to_eat_into_the_free_space_it_must_keep() {
    let f = Fake::new("write", &["sdb"]);
    let mut cfg = config(&f);
    cfg.keep_free = 1.0;
    let e = sustained(&cfg, &script(), &clock()).unwrap_err();
    assert!(e.to_string().contains("under the 100% it must keep"), "{e}");
    cfg.keep_free = 0.0;
    cfg.diskstats = f.path().join("none");
    assert!(sustained(&cfg, &script(), &clock()).is_err());
    let fail = Script {
        fail: vec!["xfs_io"],
        ..Script::default()
    };
    assert!(sustained(&config(&f), &fail, &clock()).is_err());
}

#[test]
fn ratio_needs_both_phases_and_tables_skip_empty_ones() {
    let r = |phase: &str, mb: f64| Row {
        phase: phase.into(),
        params: String::new(),
        mb_s: mb,
        pack_secs: None,
        extents: None,
        io: None,
    };
    assert_eq!(ratio(&[r("sustained", 100.0)]), None);
    assert_eq!(ratio(&[r("rewrite", 100.0)]), None);
    assert!((ratio(&[r("sustained", 100.0), r("rewrite", 40.0)]).unwrap() - 0.4).abs() < 1e-9);
    let t = tables(&[r("cells", 1.0)]);
    assert_eq!(t.len(), 1);
    assert!(
        t[0].markdown()
            .contains("|  | 1.0 | - | - | - | - | - | - |")
    );
}

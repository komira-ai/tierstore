use super::*;
use crate::clock::StepClock;
use crate::loopfs::tests::Script;
use crate::testutil::Fake;

/// A file system of `total` bytes whose use is the bytes of the pack files under it.
struct Packs {
    total: u64,
}

impl Space for Packs {
    fn total_used(&self, dir: &Path) -> Result<(u64, u64), Error> {
        let mut used = 0;
        for d in std::fs::read_dir(dir).map_err(Error::io("list", dir))? {
            let d = d.unwrap().path();
            if d.is_dir() {
                for f in std::fs::read_dir(&d).unwrap() {
                    used += f.unwrap().metadata().unwrap().len();
                }
            }
        }
        Ok((self.total, used))
    }
}

fn config(f: &Fake) -> Config {
    Config {
        dir: f.drive("churn"),
        devs: vec!["sdb".into()],
        lo: 50.0,
        hi: 60.0,
        sizes_mib: vec![1, 2],
        turns: 1.0,
        window: 8 * MIB,
        read_every: Duration::from_millis(1),
        diskstats: f.diskstats(),
    }
}

fn script() -> Script {
    Script {
        answers: vec![("xfs_io -r -c fiemap", " 0: [0..2047]: 1..2048\n".into())],
        ..Script::default()
    }
}

#[test]
fn fills_to_the_band_then_churns_a_turn_inside_it() {
    let f = Fake::new("churn", &["sdb"]);
    let cfg = config(&f);
    let space = Packs { total: 64 * MIB };
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let rows = run(&cfg, &space, &script(), &clock).unwrap();
    assert_eq!(rows[0].phase, "fill");
    let churn: Vec<&Row> = rows.iter().filter(|r| r.phase == "churn").collect();
    assert!(!churn.is_empty());
    // Every churn window wrote at least 8 MiB, and together at least one turn (64 MiB).
    assert!(churn.len() * 8 >= 64);
    assert!(rows.iter().map(|r| r.evicted).sum::<u64>() > 0);
    // After each window the fill is at most the band's top plus one pack (2 MiB of 64).
    assert!(
        rows.iter().all(|r| r.fill <= 60.0 + 100.0 * 2.0 / 64.0),
        "{rows:?}"
    );
    assert!(churn.iter().all(|r| r.fill >= 50.0 - 100.0 * 2.0 / 64.0));
    assert!(rows.iter().all(|r| r.extents == 1 && r.write_mb_s > 0.0));
    let t = tables("hdd1 85%", &rows, 8.0 / 1024.0, 64.0 / 1024.0);
    assert_eq!(t.len(), 2);
    let md = t[1].markdown();
    assert!(md.contains("| fill (fresh) |"));
    assert!(md.contains("| churn turn 1 |"));
}

#[test]
fn refuses_an_empty_band_and_reports_failures() {
    let f = Fake::new("churn", &["sdb"]);
    let space = Packs { total: 64 * MIB };
    let clock = StepClock::new(Duration::from_millis(1), 0);
    for (lo, hi, sizes, window) in [
        (60.0, 50.0, vec![1], MIB),
        (50.0, 100.0, vec![1], MIB),
        (50.0, 60.0, vec![], MIB),
        (50.0, 60.0, vec![1], 0),
    ] {
        let cfg = Config {
            lo,
            hi,
            sizes_mib: sizes,
            window,
            ..config(&f)
        };
        assert!(matches!(
            run(&cfg, &space, &script(), &clock),
            Err(Error::Invalid(_))
        ));
    }
    let fail = Script {
        fail: vec!["xfs_io"],
        ..Script::default()
    };
    assert!(run(&config(&f), &space, &fail, &clock).is_err());
    let mut bad = config(&f);
    bad.diskstats = f.path().join("none");
    assert!(run(&bad, &space, &script(), &clock).is_err());
    let mut gone = config(&f);
    gone.dir = f.path().join("file");
    std::fs::write(&gone.dir, b"").unwrap();
    assert!(run(&gone, &space, &script(), &clock).is_err());
}

#[test]
fn the_store_reads_recent_packs_and_evicts_the_least_recently_read() {
    let mut s = Store::default();
    let mut rng = Rng::new(1);
    assert_eq!(s.pick(&mut rng), None);
    assert!(s.evict().is_none());
    s.add("a".into(), 1);
    s.add("b".into(), 1);
    s.add("c".into(), 1);
    // Reading "a" makes "b" the least recently read.
    s.packs[0].last_read = 100;
    assert_eq!(s.evict().unwrap().name, "b");
    let mut newest = 0;
    for _ in 0..1000 {
        if s.pick(&mut rng).unwrap().0 == "c" {
            newest += 1;
        }
    }
    // The newest of two packs is read about half the time or more.
    assert!(newest > 400, "{newest}");
}

#[test]
fn statvfs_reads_the_real_file_system_and_summary_groups_turns() {
    let f = Fake::new("churn", &["sdb"]);
    let (total, used) = Statvfs.total_used(f.path()).unwrap();
    assert!(total >= used);
    assert!(total > 0);
    assert!(Statvfs.total_used(&f.path().join("none")).is_err());
    let r = |phase: &str, mb: f64| Row {
        phase: phase.into(),
        written_gib: 0.0,
        write_mb_s: mb,
        read_mb_s: 0.0,
        fill: 0.0,
        evicted: 0,
        extents: 0,
        io: BTreeMap::new(),
    };
    let rows = [
        r("fill", 100.0),
        r("churn", 80.0),
        r("churn", 60.0),
        r("churn", 50.0),
    ];
    let s = summary(&rows, 1.0, 2.0);
    assert_eq!(
        s,
        vec![
            ("fill (fresh)".to_string(), 100.0),
            ("churn turn 1".to_string(), 70.0),
            ("churn turn 2".to_string(), 50.0)
        ]
    );
    let only_churn = tables("x", &rows[1..], 1.0, 2.0);
    assert!(
        only_churn[1]
            .markdown()
            .contains("| churn turn 1 | 70.0 | - |")
    );
}

/// A clock whose first sleep sets a stop flag: a loop that checks the flag, then sleeps, runs
/// exactly one iteration.
struct StopOnSleep<'a> {
    inner: StepClock,
    stop: &'a AtomicBool,
}

impl Clock for StopOnSleep<'_> {
    fn now(&self) -> Duration {
        self.inner.now()
    }
    fn unix_secs(&self) -> u64 {
        self.inner.unix_secs()
    }
    fn sleep(&self, _d: Duration) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[test]
fn the_reader_skips_a_pack_evicted_after_it_was_picked() {
    let f = Fake::new("churn", &[]);
    let cfg = config(&f);
    let store = Mutex::new(Store::default());
    store.lock().unwrap().add("00/gone.pack".into(), 8 * MIB);
    let stop = AtomicBool::new(false);
    let clock = StopOnSleep {
        inner: StepClock::new(Duration::from_millis(1), 0),
        stop: &stop,
    };
    let read = AtomicU64::new(0);
    reader(&cfg, &store, &stop, &read, &clock).unwrap();
    assert_eq!(read.load(Ordering::SeqCst), 0);
}

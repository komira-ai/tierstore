use super::*;
use crate::clock::StepClock;
use crate::testutil::Fake;
use std::collections::BTreeMap;

fn drives(f: &Fake) -> BTreeMap<String, PathBuf> {
    [
        ("hdd1".to_string(), f.drive("d1")),
        ("hdd2".to_string(), f.drive("d2")),
    ]
    .into()
}

fn small() -> Packs {
    Packs {
        count: 2,
        size: 8 * MIB,
    }
}

/// Every cell made small: 1 MiB chunks, 2 MiB parts and demands, a short window.
fn shrink(mut c: Cell) -> Cell {
    c.chunk = 1 << 20;
    if c.load.part > 0 {
        c.load.part = 2 * MIB;
    }
    c.load.demand = c.load.demand.min(MIB);
    c.load.every = Duration::from_millis(5);
    c.params.slice_bytes = 2 * MIB;
    // Slow consumers fast enough that a part ends inside the short window.
    c.load.slow_mb_s *= 100.0;
    c
}

#[test]
fn prepare_writes_every_pack_once_on_every_drive() {
    let f = Fake::new("arms", &[]);
    let dirs: Vec<PathBuf> = drives(&f).into_values().collect();
    prepare(&dirs, &small()).unwrap();
    for d in &dirs {
        for i in 0..2 {
            assert_eq!(
                std::fs::metadata(d.join(format!("p{i}.pack")))
                    .unwrap()
                    .len(),
                8 * MIB
            );
        }
    }
    std::fs::write(dirs[0].join("p1.pack"), b"x").unwrap();
    prepare(&dirs, &small()).unwrap();
    assert_eq!(
        std::fs::metadata(dirs[0].join("p1.pack")).unwrap().len(),
        8 * MIB
    );
    let file = f.path().join("file");
    std::fs::write(&file, b"").unwrap();
    assert!(prepare(&[file.join("x")], &small()).is_err());
    assert_eq!(small().name(5), "p1.pack");
}

#[test]
fn every_sweep_runs_against_a_local_server() {
    let f = Fake::new("arms", &["sdb"]);
    let ds = drives(&f);
    let dirs: Vec<PathBuf> = ds.values().cloned().collect();
    prepare(&dirs, &small()).unwrap();
    let sampler = Sampler::new(&f.diskstats(), &["sdb".to_string()]);
    let mut cells = c2("hdd1");
    cells.extend(c3("hdd1", "hdd2"));
    cells.extend(c4("hdd1"));
    cells.extend(mixed("hdd1"));
    cells.extend(slow_cells("hdd1"));
    assert_eq!(cells.len(), 18 + 4 + 4 + 2 + 2);
    let mut rows = Vec::new();
    for c in cells {
        let c = shrink(c);
        let clock = StepClock::new(Duration::from_millis(1), 0);
        // A write cell needs a window long enough for a whole pack to be written inside it.
        let window = Duration::from_millis(if c.writes { 400 } else { 60 });
        let one = Duration::from_millis(1);
        let r = local_cell(&c, &ds, &small(), &clock, Some(&sampler), one, window).unwrap();
        assert_eq!(r.io.len(), 1);
        rows.push(r);
    }
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert!(labels.contains(&"path=sendfile+dontneed"));
    assert!(labels.contains(&"two-copies-striped"));
    assert!(labels.contains(&"2 slow at 10MB/s release=200ms"));
    assert!(
        rows.iter()
            .filter(|r| r.arm == "C-4")
            .all(|r| r.first.is_some())
    );
    // Written packs are removed after the cell.
    assert!(
        std::fs::read_dir(&ds["hdd1"]).unwrap().all(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('p'))
    );
    let t = tables(&rows);
    assert_eq!(t.len(), 5);
    assert!(t[0].markdown().starts_with("### C-2, cold-tier prototype"));
}

#[test]
fn a_hedge_wins_when_the_primary_is_slow_and_loses_when_it_is_not() {
    let f = Fake::new("arms", &[]);
    let ds = drives(&f);
    prepare(&ds.values().cloned().collect::<Vec<_>>(), &small()).unwrap();
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let mut cfg = base("hdd1");
    cfg.chunk = 1 << 20;
    let server = Server::new(
        Config {
            drives: ds.clone(),
            params: cfg.params,
            chunk: cfg.chunk,
            path: cfg.path,
            release_after: None,
        },
        &clock,
    );
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let h = scope.spawn(|| server.serve(&l, &stop));
        let _stop = crate::testutil::StopOnDrop(&stop);
        // Hold both slots of hdd1: the primary waits in its queue; the hedge to hdd2 answers.
        let s1 = server.sched("hdd1").unwrap();
        let a = s1.admit(Class::Demand, false).unwrap().0;
        let b = s1.admit(Class::Demand, false).unwrap().0;
        let (first, _, hedged, won) = std::thread::scope(|inner| {
            let g = inner.spawn(|| {
                hedged_get(
                    addr,
                    "hdd1",
                    "hdd2",
                    Duration::from_millis(3),
                    "p0.pack",
                    0,
                    MIB,
                    &clock,
                )
            });
            // Let the primary go once the hedge has been answered.
            while server.sched("hdd2").unwrap().counts().admitted == 0 {
                std::thread::yield_now();
            }
            s1.release(a);
            s1.release(b);
            g.join().unwrap().unwrap()
        });
        assert!(
            hedged,
            "the primary could not answer before the hedge was sent (first {first}, won {won})"
        );
        // An idle primary answers before the hedge is sent: no hedge.
        let (_, _, hedged, _) = hedged_get(
            addr,
            "hdd1",
            "hdd2",
            Duration::from_secs(3600),
            "p0.pack",
            0,
            MIB,
            &clock,
        )
        .unwrap();
        assert!(!hedged);
        // A missing pack is answered 404, which is an answer, not a failure.
        assert!(
            hedged_get(
                addr,
                "hdd1",
                "hdd2",
                Duration::from_secs(3600),
                "none",
                0,
                MIB,
                &clock
            )
            .is_ok()
        );
        stop.store(true, Ordering::SeqCst);
        h.join().unwrap().unwrap();
    });
    // No server: every load fails to connect.
    let dead = tierstore_testkit::port::dead_addr();
    let span = || Span::starting(&clock, Duration::ZERO, Duration::from_millis(10));
    let mut load = base("hdd1").load;
    assert!(run_load(dead, &small(), &load, &clock, span()).is_err());
    load.background = 0;
    load.slow = 1;
    assert!(run_load(dead, &small(), &load, &clock, span()).is_err());
    load.slow = 0;
    load.hedge = Some(("hdd2".into(), Duration::ZERO));
    assert!(run_load(dead, &small(), &load, &clock, span()).is_err());
}

#[test]
fn refused_background_reads_are_counted_and_rows_summarise() {
    let f = Fake::new("arms", &[]);
    let ds = drives(&f);
    prepare(&ds.values().cloned().collect::<Vec<_>>(), &small()).unwrap();
    let mut c = shrink(base("hdd1"));
    c.params.queue_max = 0;
    c.load.demand = 0;
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let r = local_cell(
        &c,
        &ds,
        &small(),
        &clock,
        None,
        Duration::ZERO,
        Duration::from_millis(30),
    )
    .unwrap();
    assert!(r.refused > 0);
    assert_eq!(r.bg_mb_s, 0.0);
    assert!(r.io.is_empty());
    let o = Outcome {
        first_ms: vec![1.0],
        ..Outcome::default()
    };
    let served = [Served {
        class: Class::Demand,
        wait_ms: 2.0,
        service_ms: 1.0,
        bytes: 1,
    }];
    let row = row("x", "y", &o, 1.0, &served, Counts::default(), 2_000_000);
    assert_eq!(row.demand_wait.unwrap().max, 2.0);
    assert!((row.write_mb_s - 2.0).abs() < 1e-9);
    let md = tables(&[row])[0].markdown();
    assert!(
        md.contains("| y | 0.0 | - | 1.0 | 1.0 | 1.0 | - | 2.0 | 0/0 | 0.0 | 2.0 | 0 | 0 | 0 |  |")
    );
    // A local cell whose drives do not exist fails in its load.
    let mut bad = ds.clone();
    bad.insert("hdd1".into(), f.path().join("none"));
    let mut w = shrink(base("hdd1"));
    w.writes = true;
    assert!(
        local_cell(
            &w,
            &bad,
            &small(),
            &clock,
            None,
            Duration::ZERO,
            Duration::from_millis(10)
        )
        .is_err()
    );
}

#[test]
fn the_winner_is_the_earlier_first_byte_counted_from_the_primary_start() {
    let g = |first_ms: f64, total_ms: f64| Got {
        status: 206,
        first_ms,
        total_ms,
        bytes: 1,
        wait_ms: 0.0,
    };
    assert_eq!(winner(&g(50.0, 60.0), None), (50.0, 60.0, false, false));
    assert_eq!(
        winner(&g(50.0, 60.0), Some(&(g(10.0, 20.0), 30.0))),
        (40.0, 50.0, true, true)
    );
    assert_eq!(
        winner(&g(35.0, 45.0), Some(&(g(10.0, 20.0), 30.0))),
        (35.0, 45.0, true, false)
    );
}

#[test]
fn sweeps_are_named() {
    let names: Vec<String> = ["c2", "c3", "c4", "mixed", "slow"]
        .map(String::from)
        .to_vec();
    let cells = cells_for(&names, "hdd1", "hdd2").unwrap();
    assert_eq!(cells.len(), 30);
    assert_eq!(cells[18].load.stripe, Vec::<String>::new());
    let e = cells_for(&["c9".to_string()], "a", "b").unwrap_err();
    assert_eq!(e.to_string(), "arm c9: want c2, c3, c4, mixed or slow");
}

#[test]
fn slow_reads_and_writes_that_end_inside_the_window_are_counted() {
    let f = Fake::new("arms", &[]);
    let ds = drives(&f);
    let packs = Packs {
        count: 1,
        size: MIB,
    };
    prepare(&ds.values().cloned().collect::<Vec<_>>(), &packs).unwrap();
    // A write cell whose window is long beside one 1 MiB pack: written packs are counted.
    let mut w = shrink(base("hdd1"));
    w.writes = true;
    w.load.background = 0;
    w.load.demand = 0;
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let r = local_cell(
        &w,
        &ds,
        &packs,
        &clock,
        None,
        Duration::ZERO,
        Duration::from_secs(2),
    )
    .unwrap();
    assert!(r.write_mb_s > 0.0);
    // A slow consumer whose rate is far above what the window needs: its parts are counted.
    let mut s = shrink(base("hdd1"));
    s.load.background = 0;
    s.load.demand = 0;
    s.load.slow = 1;
    s.load.slow_mb_s = 1e6;
    let r = local_cell(
        &s,
        &ds,
        &packs,
        &clock,
        None,
        Duration::ZERO,
        Duration::from_millis(300),
    )
    .unwrap();
    assert!(r.slow_mb_s > 0.0);
}

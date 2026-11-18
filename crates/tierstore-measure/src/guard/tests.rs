use super::*;
use crate::clock::StepClock;
use crate::testutil::Fake;
use std::cell::Cell;

fn proc_with(f: &Fake, silo_bytes: u64, load: &str) -> PathBuf {
    let p = f.path().join("proc");
    for (pid, comm, bytes) in [
        ("1", "silo", silo_bytes),
        ("2", "bash", 999),
        ("3", "silo", 0),
    ] {
        let d = p.join(pid);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
        if pid != "3" {
            std::fs::write(
                d.join("io"),
                format!("rchar: 5\nread_bytes: {bytes}\nwrite_bytes: {bytes}\ncancelled_write_bytes: 7\n"),
            )
            .unwrap();
        }
    }
    std::fs::write(p.join("loadavg"), format!("{load} 1.0 1.0 1/100 5\n")).unwrap();
    p
}

fn guard(proc_root: PathBuf) -> Guard {
    Guard {
        proc_root,
        comms: vec!["silo".into()],
        max_mb_s: 1.0,
        max_load: 10.0,
        attempts: 3,
        quiet_window: Duration::from_millis(10),
        max_wait: Duration::from_millis(100),
        write_devs: Vec::new(),
        max_w_s: 5.0,
    }
}

#[test]
fn counts_only_the_guarded_processes_disk_bytes_and_reads_the_load() {
    let f = Fake::new("guard", &[]);
    let g = guard(proc_with(&f, 1000, "2.5"));
    assert_eq!(g.foreign_bytes().unwrap(), 2000);
    assert!((g.load1().unwrap() - 2.5).abs() < 1e-9);
    std::fs::write(g.proc_root.join("loadavg"), "x").unwrap();
    assert!(g.load1().is_err());
    std::fs::remove_file(g.proc_root.join("loadavg")).unwrap();
    assert!(g.load1().is_err());
    assert!(guard(f.path().join("none")).foreign_bytes().is_err());
}

#[test]
fn a_quiet_cell_runs_once_and_a_noisy_one_until_its_attempts_run_out() {
    let f = Fake::new("guard", &[]);
    let g = guard(proc_with(&f, 0, "1.0"));
    let c = StepClock::new(Duration::from_millis(1), 0);
    let (v, seen) = g.run(&c, || Ok(5)).unwrap();
    assert_eq!(v, 5);
    assert_eq!(seen.attempts, 1);
    assert!(g.wait_quiet(&c).unwrap());
    // Every attempt adds 10 MB of silo traffic: noisy each time, so all three attempts run.
    let n = Cell::new(0u64);
    let io = g.proc_root.join("1/io");
    let (_, seen) = g
        .run(&c, || {
            n.set(n.get() + 10_000_000);
            std::fs::write(&io, format!("read_bytes: {}\n", n.get())).unwrap();
            Ok(())
        })
        .unwrap();
    assert_eq!(seen.attempts, 3);
    assert!(seen.foreign_mb_s > 1.0);
    let e = g.run(&c, || Err::<(), _>(Error::Invalid("cell failed".into())));
    assert_eq!(e.unwrap_err().to_string(), "cell failed");
}

#[test]
fn a_loaded_node_is_not_quiet_and_the_wait_gives_up() {
    let f = Fake::new("guard", &[]);
    let g = guard(proc_with(&f, 0, "50.0"));
    let c = StepClock::new(Duration::from_millis(1), 0);
    assert!(!g.wait_quiet(&c).unwrap());
    let (_, seen) = g.run(&c, || Ok(())).unwrap();
    assert_eq!(seen.attempts, 3);
    assert!((seen.load1 - 50.0).abs() < 1e-9);
}

#[test]
fn guarded_without_a_guard_runs_once() {
    let c = StepClock::new(Duration::from_millis(1), 0);
    let (v, seen) = guarded(None, &c, || Ok(3)).unwrap();
    assert_eq!((v, seen.attempts), (3, 1));
    assert!(guarded(None, &c, || Err::<(), _>(Error::Invalid("x".into()))).is_err());
    let f = Fake::new("guard", &[]);
    let g = guard(proc_with(&f, 0, "1.0"));
    assert_eq!(guarded(Some(&g), &c, || Ok(1)).unwrap().1.attempts, 1);
}

#[test]
fn quiet_needs_both_limits() {
    let f = Fake::new("guard", &[]);
    let g = guard(proc_with(&f, 0, "1.0"));
    let seen = |foreign_mb_s, foreign_w_s, load1| Seen {
        foreign_mb_s,
        foreign_w_s,
        load1,
        attempts: 1,
    };
    assert!(g.quiet(&seen(1.0, 5.0, 10.0)));
    assert!(!g.quiet(&seen(1.1, 0.0, 1.0)));
    assert!(!g.quiet(&seen(0.0, 5.1, 1.0)));
    assert!(!g.quiet(&seen(0.0, 0.0, 10.1)));
}

#[test]
fn writes_on_the_watched_drives_make_a_cell_noisy() {
    let f = Fake::new("guard", &["sdb"]);
    let mut g = guard(proc_with(&f, 0, "1.0"));
    g.write_devs = vec!["sdb".into()];
    std::fs::copy(f.diskstats(), g.proc_root.join("diskstats")).unwrap();
    let c = StepClock::new(Duration::from_millis(1), 0);
    let stats = g.proc_root.join("diskstats");
    let n = Cell::new(0u64);
    let (_, seen) = g
        .run(&c, || {
            n.set(n.get() + 1000);
            std::fs::write(&stats, format!("8 0 sdb 0 0 0 0 {} 0 0 0 0 0 0\n", n.get())).unwrap();
            Ok(())
        })
        .unwrap();
    assert_eq!(seen.attempts, 3);
    assert!(seen.foreign_w_s > 5.0);
    g.write_devs = vec!["sdq".into()];
    assert!(g.writes().is_err());
}

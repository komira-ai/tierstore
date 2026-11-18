//! Section 14 rows: "Drive scheduler (fake drive, injected clock)" for the hot device's
//! `HotSched` ("the NVMe `HotSched` honours its depth and class order"); the settings checks of
//! 10.14 the hot tier owns; and the real directory behind `HotDev` (`DirDev`) and the index's
//! view of the device (`DevFs`).
//!
//! Catches: a queue that runs a lower class before a higher one, or more calls at once than its
//! depth; a watchdog that stalls at exactly `stall_after`, leaves queued calls waiting, admits a
//! call while stalled, or never returns to `up`; an inline queue that moves a call to another
//! thread; a rename that replaces a target it was told not to, or removes the source when it
//! refuses; a create that truncates an existing file; a list of a missing directory that fails
//! or is unsorted; a `DevFs` append that does not create the file, or a `sync` that does not
//! make a nested file's directory entries durable; a configuration that cannot survive its
//! declared failure and is accepted anyway.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;

use crate::support::{Exec, FakeDev, Handle, config};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::upstream::DriveState;
use tierstore_format::block::BlockGeometry;
use tierstore_harness::disk::FakeDisk;
use tierstore_hot::config::{ConfigError, HotConfig};
use tierstore_hot::dev::{DirDev, HotDev, dir_of};
use tierstore_hot::devfs::DevFs;
use tierstore_hot::sched::{HotSched, SchedError};
use tierstore_index::fs::Fs;
use tierstore_store_api::sched::DriveClass;
use tierstore_testkit::dir::TestDir;

// ---------------------------------------------------------------------------------------------
// HotSched.

/// A counting gate: `wait` blocks until a token is there, `open` adds tokens.
struct Gate {
    tokens: Mutex<usize>,
    cv: Condvar,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            tokens: Mutex::new(0),
            cv: Condvar::new(),
        })
    }

    fn wait(&self) {
        let mut n = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        while *n == 0 {
            n = self.cv.wait(n).unwrap_or_else(PoisonError::into_inner);
        }
        *n -= 1;
    }

    fn open(&self, tokens: usize) {
        *self.tokens.lock().unwrap_or_else(PoisonError::into_inner) += tokens;
        self.cv.notify_all();
    }
}

/// Polls the executor until the call finished (its result is sent from an I/O thread).
fn finished<T>(exec: &Exec, handle: &Handle<T>) -> T {
    loop {
        exec.run();
        if let Some(v) = handle.take() {
            return v;
        }
        thread::yield_now();
    }
}

/// Starts a call of `class` that tells `started` it runs and then waits for the gate.
fn gated(
    exec: &Exec,
    sched: &HotSched,
    class: DriveClass,
    gate: &Arc<Gate>,
    started: &mpsc::Sender<()>,
) -> Handle<Result<(), SchedError>> {
    let (s, g, tx) = (sched.clone(), Arc::clone(gate), started.clone());
    exec.start(async move {
        s.run(class, move || {
            tx.send(()).unwrap();
            g.wait();
        })
        .await
    })
}

/// Waits until no call is running (the worker has taken the finished call off its books).
fn idle(sched: &HotSched) {
    while sched.report().streams != 0 {
        thread::yield_now();
    }
}

#[test]
fn queued_calls_run_demand_then_write_then_prepare_then_background() {
    let sched = HotSched::new(1, 10_000, Arc::new(ManualClock::new())).unwrap();
    let exec = Exec::default();
    let gate = Gate::new();
    let (started_tx, started_rx) = mpsc::channel();
    // The one thread is busy with a call the test holds.
    let blocker = gated(&exec, &sched, DriveClass::Background, &gate, &started_tx);
    exec.run();
    started_rx.recv().unwrap();
    let (order_tx, order_rx) = mpsc::channel();
    let queued: Vec<_> = [
        ("background", DriveClass::Background),
        ("prepare", DriveClass::Prepare),
        ("write", DriveClass::Write),
        ("demand", DriveClass::Demand),
    ]
    .into_iter()
    .map(|(name, class)| {
        let (s, tx) = (sched.clone(), order_tx.clone());
        exec.start(async move { s.run(class, move || tx.send(name).unwrap()).await })
    })
    .collect();
    exec.run();
    assert_eq!(
        sched.report().queued,
        [1, 1, 1, 1],
        "one queued call per class"
    );
    gate.open(1);
    let order: Vec<&str> = (0..4).map(|_| order_rx.recv().unwrap()).collect();
    assert_eq!(order, ["demand", "write", "prepare", "background"]);
    finished(&exec, &blocker).unwrap();
    for h in &queued {
        finished(&exec, h).unwrap();
    }
}

#[test]
fn a_depth_of_zero_still_has_one_thread_to_run_calls() {
    let sched = HotSched::new(0, 1000, Arc::new(ManualClock::new())).unwrap();
    let exec = Exec::default();
    let call = {
        let s = sched.clone();
        exec.start(async move { s.run(DriveClass::Demand, || "ran").await })
    };
    assert_eq!(finished(&exec, &call).unwrap(), "ran");
}

#[test]
fn calls_of_one_class_run_in_arrival_order() {
    let sched = HotSched::new(1, 10_000, Arc::new(ManualClock::new())).unwrap();
    let exec = Exec::default();
    let gate = Gate::new();
    let (started_tx, started_rx) = mpsc::channel();
    let blocker = gated(&exec, &sched, DriveClass::Write, &gate, &started_tx);
    exec.run();
    started_rx.recv().unwrap();
    let (order_tx, order_rx) = mpsc::channel();
    let queued: Vec<_> = (0..4)
        .map(|i| {
            let (s, tx) = (sched.clone(), order_tx.clone());
            let h = exec
                .start(async move { s.run(DriveClass::Write, move || tx.send(i).unwrap()).await });
            exec.run();
            h
        })
        .collect();
    gate.open(1);
    let order: Vec<i32> = (0..4).map(|_| order_rx.recv().unwrap()).collect();
    assert_eq!(order, [0, 1, 2, 3]);
    finished(&exec, &blocker).unwrap();
    for h in &queued {
        finished(&exec, h).unwrap();
    }
}

#[test]
fn never_more_calls_run_at_once_than_the_depth() {
    let depth = 3;
    let sched = HotSched::new(depth, 10_000, Arc::new(ManualClock::new())).unwrap();
    let exec = Exec::default();
    let gate = Gate::new();
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = mpsc::channel();
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let (s, g, tx) = (sched.clone(), Arc::clone(&gate), started_tx.clone());
            let (running, peak) = (Arc::clone(&running), Arc::clone(&peak));
            exec.start(async move {
                s.run(DriveClass::Write, move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tx.send(()).unwrap();
                    g.wait();
                    running.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            })
        })
        .collect();
    exec.run();
    for _ in 0..depth {
        started_rx.recv().unwrap();
    }
    let report = sched.report();
    assert_eq!(
        running.load(Ordering::SeqCst),
        depth,
        "the first calls fill every thread"
    );
    assert_eq!(report.streams as usize, depth);
    assert_eq!(report.queued, [0, 5, 0, 0], "the rest wait in their class");
    // Each finished call frees a thread for exactly one queued call.
    for _ in 0..5 {
        gate.open(1);
        started_rx.recv().unwrap();
        assert!(peak.load(Ordering::SeqCst) <= depth);
    }
    gate.open(depth);
    for h in &calls {
        finished(&exec, h).unwrap();
    }
    assert_eq!(
        peak.load(Ordering::SeqCst),
        depth,
        "the depth was reached, never passed"
    );
}

#[test]
fn the_watchdog_stalls_the_device_fails_queued_calls_and_refuses_new_ones_until_the_call_returns() {
    let clock = ManualClock::new();
    let sched = HotSched::new(1, 1000, Arc::new(clock.clone())).unwrap();
    let exec = Exec::default();
    let gate = Gate::new();
    let (started_tx, started_rx) = mpsc::channel();
    let hung = gated(&exec, &sched, DriveClass::Write, &gate, &started_tx);
    exec.run();
    started_rx.recv().unwrap();
    let waiting_ran = Arc::new(AtomicBool::new(false));
    let waiting = {
        let (s, ran) = (sched.clone(), Arc::clone(&waiting_ran));
        exec.start(async move {
            s.run(DriveClass::Demand, move || {
                ran.store(true, Ordering::SeqCst)
            })
            .await
        })
    };
    exec.run();
    assert_eq!(sched.report().queued, [1, 0, 0, 0]);
    // A call exactly `stall_after` old is not stuck; one millisecond more is.
    clock.advance(1000);
    assert_eq!(sched.watchdog(), DriveState::Up);
    assert_eq!(sched.stalls(), 0);
    clock.advance(1);
    assert_eq!(sched.watchdog(), DriveState::Stalled);
    assert_eq!(sched.stalls(), 1);
    assert!(matches!(
        finished(&exec, &waiting),
        Err(SchedError::Stalled)
    ));
    assert!(
        !waiting_ran.load(Ordering::SeqCst),
        "a queued call that failed never ran"
    );
    // A new call is refused at once, without running.
    let refused_ran = Arc::new(AtomicBool::new(false));
    let refused = {
        let (s, ran) = (sched.clone(), Arc::clone(&refused_ran));
        exec.start(async move {
            s.run(DriveClass::Demand, move || {
                ran.store(true, Ordering::SeqCst)
            })
            .await
        })
    };
    assert!(matches!(
        finished(&exec, &refused),
        Err(SchedError::Stalled)
    ));
    assert!(!refused_ran.load(Ordering::SeqCst));
    let report = sched.report();
    assert_eq!(report.state, Some(DriveState::Stalled));
    assert_eq!(report.stuck_threads, 1);
    assert_eq!(report.oldest_call_ms, Some(1001));
    assert_eq!(report.queued, [0, 0, 0, 0]);
    // Checking again while it stays stalled is not a second stall.
    assert_eq!(sched.watchdog(), DriveState::Stalled);
    assert_eq!(sched.stalls(), 1);
    // The call returns: the device is `up` at the next check and serves again.
    gate.open(1);
    finished(&exec, &hung).unwrap();
    idle(&sched);
    assert_eq!(sched.watchdog(), DriveState::Up);
    assert_eq!(sched.report().state, Some(DriveState::Up));
    let again = {
        let s = sched.clone();
        exec.start(async move { s.run(DriveClass::Write, || 7).await })
    };
    assert_eq!(finished(&exec, &again).unwrap(), 7);
    assert_eq!(sched.stalls(), 1);
}

#[test]
fn report_counts_queued_calls_by_class_and_the_age_of_the_oldest_running_call() {
    let clock = ManualClock::new();
    let sched = HotSched::new(1, 10_000, Arc::new(clock.clone())).unwrap();
    let exec = Exec::default();
    let gate = Gate::new();
    let (started_tx, started_rx) = mpsc::channel();
    assert_eq!(sched.report().oldest_call_ms, None, "nothing runs yet");
    assert_eq!(sched.report().state, Some(DriveState::Up));
    let running = gated(&exec, &sched, DriveClass::Write, &gate, &started_tx);
    exec.run();
    started_rx.recv().unwrap();
    clock.advance(250);
    let mut queued = Vec::new();
    for class in [
        DriveClass::Demand,
        DriveClass::Write,
        DriveClass::Write,
        DriveClass::Background,
    ] {
        queued.push(gated(&exec, &sched, class, &gate, &started_tx));
    }
    exec.run();
    let report = sched.report();
    assert_eq!(report.queued, [1, 2, 0, 1]);
    assert_eq!(report.streams, 1);
    assert_eq!(report.oldest_call_ms, Some(250));
    assert_eq!(report.stuck_threads, 0);
    gate.open(5);
    finished(&exec, &running).unwrap();
    for h in &queued {
        finished(&exec, h).unwrap();
    }
    idle(&sched);
    assert_eq!(sched.report().queued, [0, 0, 0, 0]);
}

#[test]
fn an_inline_queue_runs_each_call_on_its_caller_in_arrival_order() {
    let sched = HotSched::inline(1000, Arc::new(ManualClock::new()));
    let exec = Exec::default();
    let me = thread::current().id();
    let ran_on = Arc::new(Mutex::new(Vec::new()));
    let calls: Vec<_> = [
        DriveClass::Background,
        DriveClass::Demand,
        DriveClass::Write,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, class)| {
        let (s, ran_on) = (sched.clone(), Arc::clone(&ran_on));
        exec.start(async move {
            s.run(class, move || {
                ran_on.lock().unwrap().push((i, thread::current().id()));
            })
            .await
        })
    })
    .collect();
    exec.run();
    for h in &calls {
        h.take().unwrap().unwrap();
    }
    let ran_on = ran_on.lock().unwrap();
    assert_eq!(
        ran_on.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert!(
        ran_on.iter().all(|(_, t)| *t == me),
        "an inline call ran on another thread"
    );
    assert_eq!(sched.report().queued, [0, 0, 0, 0]);
}

#[test]
fn an_inline_queue_stalls_when_a_call_outlives_stall_after_and_serves_again_after_the_check() {
    let clock = ManualClock::new();
    let sched = HotSched::inline(500, Arc::new(clock.clone()));
    let exec = Exec::default();
    let probe = {
        let (s, c) = (sched.clone(), clock.clone());
        exec.start(async move {
            let s2 = s.clone();
            s.run(DriveClass::Write, move || {
                c.advance(501);
                s2.watchdog()
            })
            .await
        })
    };
    exec.run();
    assert_eq!(probe.take().unwrap().unwrap(), DriveState::Stalled);
    let refused = {
        let s = sched.clone();
        exec.start(async move { s.run(DriveClass::Write, || 1).await })
    };
    exec.run();
    assert!(matches!(refused.take().unwrap(), Err(SchedError::Stalled)));
    assert_eq!(sched.watchdog(), DriveState::Up);
    let served = {
        let s = sched.clone();
        exec.start(async move { s.run(DriveClass::Write, || 1).await })
    };
    exec.run();
    assert_eq!(served.take().unwrap().unwrap(), 1);
    assert_eq!(sched.stalls(), 1);
}

// ---------------------------------------------------------------------------------------------
// Settings.

fn defaults() -> HotConfig {
    HotConfig::defaults(1 << 40, BlockGeometry::DEFAULT)
}

#[test]
fn the_defaults_of_10_14_pass_their_checks_and_a_small_test_configuration_too() {
    let cfg = defaults();
    assert_eq!(cfg.validate(3), Ok(()));
    assert_eq!((cfg.replicas, cfg.min_acks), (3, 2));
    assert_eq!(cfg.seal_bytes, BlockGeometry::DEFAULT.max_object());
    assert!(
        cfg.on_full.is_none(),
        "the defaults are for a node with a cold store"
    );
    assert_eq!(config(3, 2).validate(3), Ok(()));
    assert_eq!(config(2, 2).validate(2), Ok(()));
}

#[test]
fn replicas_must_be_between_one_and_the_number_of_failure_domains() {
    let mut cfg = defaults();
    cfg.replicas = 0;
    assert_eq!(
        cfg.validate(3),
        Err(ConfigError::Replicas {
            replicas: 0,
            domains: 3
        })
    );
    cfg.replicas = 4;
    assert_eq!(
        cfg.validate(3),
        Err(ConfigError::Replicas {
            replicas: 4,
            domains: 3
        })
    );
    cfg.replicas = 3;
    assert_eq!(
        cfg.validate(3),
        Ok(()),
        "as many replicas as domains is allowed"
    );
}

#[test]
fn min_acks_must_be_between_one_and_replicas() {
    let mut cfg = defaults();
    cfg.min_acks = 0;
    assert_eq!(
        cfg.validate(3),
        Err(ConfigError::MinAcks {
            min_acks: 0,
            replicas: 3
        })
    );
    cfg.min_acks = 4;
    assert_eq!(
        cfg.validate(3),
        Err(ConfigError::MinAcks {
            min_acks: 4,
            replicas: 3
        })
    );
    cfg.min_acks = 3;
    assert_eq!(cfg.validate(3), Ok(()));
    cfg.min_acks = 1;
    assert_eq!(cfg.validate(3), Ok(()));
}

#[test]
fn seal_bytes_may_not_pass_what_a_block_holds() {
    let mut cfg = defaults();
    let max = cfg.geometry.max_object();
    cfg.seal_bytes = max + 1;
    assert_eq!(
        cfg.validate(3),
        Err(ConfigError::SealBytes {
            seal_bytes: max + 1,
            max
        })
    );
    cfg.seal_bytes = max;
    assert_eq!(cfg.validate(3), Ok(()));
}

#[test]
fn large_min_may_not_pass_a_quarter_of_a_block() {
    let mut cfg = defaults();
    let quarter = cfg.geometry.block_bytes() / 4;
    cfg.large_min = quarter + 1;
    assert_eq!(
        cfg.validate(3),
        Err(ConfigError::LargeMin {
            large_min: quarter + 1
        })
    );
    cfg.large_min = quarter;
    assert_eq!(cfg.validate(3), Ok(()));
}

#[test]
fn seal_min_bytes_may_not_pass_seal_bytes() {
    let mut cfg = defaults();
    cfg.seal_min_bytes = cfg.seal_bytes + 1;
    assert_eq!(cfg.validate(3), Err(ConfigError::SealMinBytes));
    cfg.seal_min_bytes = cfg.seal_bytes;
    assert_eq!(cfg.validate(3), Ok(()));
}

#[test]
fn batch_max_queue_depth_and_capacity_must_be_above_zero() {
    for zero in [
        (|c: &mut HotConfig| c.batch_max = 0) as fn(&mut HotConfig),
        |c| c.queue_depth = 0,
        |c| c.capacity = 0,
    ] {
        let mut cfg = defaults();
        zero(&mut cfg);
        assert_eq!(cfg.validate(3), Err(ConfigError::Zero));
    }
}

#[test]
fn the_dirty_marks_must_be_strictly_ordered_and_at_most_the_whole_device() {
    type Break = fn(&mut HotConfig);
    let cases: [(&str, Break); 7] = [
        ("stop is zero", |c| c.marks.destage_stop = 0.0),
        ("stop equals start", |c| {
            c.marks.destage_stop = c.marks.destage_start
        }),
        ("start above urgent", |c| {
            c.marks.destage_start = c.marks.urgent + 0.01
        }),
        ("urgent equals slow", |c| c.marks.urgent = c.marks.slow),
        ("slow above reject", |c| {
            c.marks.slow = c.marks.reject + 0.01
        }),
        ("reject above one", |c| c.marks.reject = 1.01),
        ("a mark is not a number", |c| c.marks.urgent = f64::NAN),
    ];
    for (name, break_it) in cases {
        let mut cfg = defaults();
        break_it(&mut cfg);
        assert_eq!(cfg.validate(3), Err(ConfigError::Marks), "{name}");
    }
    let mut cfg = defaults();
    cfg.marks.reject = 1.0;
    assert_eq!(
        cfg.validate(3),
        Ok(()),
        "reject at the whole device is allowed"
    );
}

// ---------------------------------------------------------------------------------------------
// DirDev.

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

#[test]
fn dir_of_is_everything_before_the_last_slash() {
    assert_eq!(dir_of("seg/1.seg"), "seg");
    assert_eq!(dir_of("big/ab/x.obj"), "big/ab");
    assert_eq!(dir_of("x"), "");
    assert_eq!(dir_of(""), "");
}

#[test]
fn create_makes_the_directories_and_refuses_an_existing_file_without_touching_it() {
    let dir = scratch("dirdev-create");
    let dev = DirDev::new(dir.path());
    dev.create("big/ab/x.obj").unwrap();
    assert_eq!(dev.len("big/ab/x.obj").unwrap(), 0);
    dev.write_at("big/ab/x.obj", 0, b"kept").unwrap();
    let err = dev.create("big/ab/x.obj").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(dev.read_at("big/ab/x.obj", 0, 100).unwrap(), b"kept");
}

#[test]
fn write_at_reads_back_and_a_write_past_the_end_zero_fills_the_gap() {
    let dir = scratch("dirdev-write");
    let dev = DirDev::new(dir.path());
    dev.create("seg/1.seg").unwrap();
    dev.write_at("seg/1.seg", 0, b"hello").unwrap();
    dev.write_at("seg/1.seg", 8, b"world").unwrap();
    assert_eq!(dev.len("seg/1.seg").unwrap(), 13);
    assert_eq!(
        dev.read_at("seg/1.seg", 0, 13).unwrap(),
        b"hello\0\0\0world"
    );
    dev.write_at("seg/1.seg", 1, b"E").unwrap();
    assert_eq!(dev.read_at("seg/1.seg", 0, 5).unwrap(), b"hEllo");
    let missing = dev.write_at("seg/none.seg", 0, b"x").unwrap_err();
    assert_eq!(
        missing.kind(),
        ErrorKind::NotFound,
        "a write does not create the file"
    );
}

#[test]
fn read_at_returns_fewer_bytes_at_the_end_and_none_past_it() {
    let dir = scratch("dirdev-read");
    let dev = DirDev::new(dir.path());
    dev.create("f").unwrap();
    dev.write_at("f", 0, b"0123456789").unwrap();
    assert_eq!(dev.read_at("f", 4, 3).unwrap(), b"456");
    assert_eq!(dev.read_at("f", 8, 100).unwrap(), b"89");
    assert_eq!(dev.read_at("f", 10, 4).unwrap(), b"");
    assert_eq!(dev.read_at("f", 50, 4).unwrap(), b"");
    assert_eq!(
        dev.read_at("f", 3, u64::MAX).unwrap(),
        b"3456789",
        "a huge length does not overflow"
    );
    assert_eq!(
        dev.read_at("nope", 0, 1).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(dev.len("nope").unwrap_err().kind(), ErrorKind::NotFound);
}

#[test]
fn set_len_cuts_and_zero_extends() {
    let dir = scratch("dirdev-setlen");
    let dev = DirDev::new(dir.path());
    dev.create("f").unwrap();
    dev.write_at("f", 0, b"abcdef").unwrap();
    dev.set_len("f", 3).unwrap();
    assert_eq!(dev.read_at("f", 0, 10).unwrap(), b"abc");
    dev.set_len("f", 6).unwrap();
    assert_eq!(dev.read_at("f", 0, 10).unwrap(), b"abc\0\0\0");
    assert_eq!(
        dev.set_len("nope", 1).unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn allocate_reserves_space_without_changing_the_length() {
    let dir = scratch("dirdev-allocate");
    let dev = DirDev::new(dir.path());
    dev.create("big/ab/up").unwrap();
    dev.allocate("big/ab/up", 1 << 20).unwrap();
    assert_eq!(
        dev.len("big/ab/up").unwrap(),
        0,
        "KEEP_SIZE: the length is the bytes written"
    );
    dev.write_at("big/ab/up", 0, b"data").unwrap();
    assert_eq!(dev.len("big/ab/up").unwrap(), 4);
    assert_eq!(
        dev.allocate("nope", 10).unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn fdatasync_and_fsync_dir_succeed_on_files_and_directories_and_fail_on_missing_ones() {
    let dir = scratch("dirdev-sync");
    let dev = DirDev::new(dir.path());
    dev.create("seg/1.seg").unwrap();
    dev.write_at("seg/1.seg", 0, b"x").unwrap();
    dev.fdatasync("seg/1.seg").unwrap();
    dev.fsync_dir("seg").unwrap();
    dev.fsync_dir("").unwrap();
    assert_eq!(
        dev.fdatasync("seg/none").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        dev.fsync_dir("none").unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn rename_with_noreplace_refuses_an_existing_target_and_leaves_both_files() {
    let dir = scratch("dirdev-noreplace");
    let dev = DirDev::new(dir.path());
    dev.create("big/.up").unwrap();
    dev.write_at("big/.up", 0, b"new").unwrap();
    dev.create("big/ab/x.obj").unwrap();
    dev.write_at("big/ab/x.obj", 0, b"old").unwrap();
    let err = dev.rename("big/.up", "big/ab/x.obj", true).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        dev.read_at("big/.up", 0, 10).unwrap(),
        b"new",
        "the source stays"
    );
    assert_eq!(
        dev.read_at("big/ab/x.obj", 0, 10).unwrap(),
        b"old",
        "the target is untouched"
    );
}

#[test]
fn rename_with_noreplace_moves_to_a_free_name_making_its_directory() {
    let dir = scratch("dirdev-move");
    let dev = DirDev::new(dir.path());
    dev.create("big/.up").unwrap();
    dev.write_at("big/.up", 0, b"bytes").unwrap();
    dev.rename("big/.up", "big/cd/x.obj", true).unwrap();
    assert_eq!(dev.read_at("big/cd/x.obj", 0, 10).unwrap(), b"bytes");
    assert_eq!(dev.len("big/.up").unwrap_err().kind(), ErrorKind::NotFound);
}

#[test]
fn rename_without_noreplace_replaces_the_target() {
    let dir = scratch("dirdev-replace");
    let dev = DirDev::new(dir.path());
    dev.create("seg/1.seg.repair").unwrap();
    dev.write_at("seg/1.seg.repair", 0, b"good").unwrap();
    dev.create("seg/1.seg").unwrap();
    dev.write_at("seg/1.seg", 0, b"bad").unwrap();
    dev.rename("seg/1.seg.repair", "seg/1.seg", false).unwrap();
    assert_eq!(dev.read_at("seg/1.seg", 0, 10).unwrap(), b"good");
    assert_eq!(
        dev.len("seg/1.seg.repair").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        dev.rename("seg/none", "seg/2.seg", false)
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn unlink_removes_the_file_and_a_second_unlink_says_not_found() {
    let dir = scratch("dirdev-unlink");
    let dev = DirDev::new(dir.path());
    dev.create("cache/1.1.seg").unwrap();
    dev.unlink("cache/1.1.seg").unwrap();
    assert_eq!(
        dev.len("cache/1.1.seg").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        dev.unlink("cache/1.1.seg").unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn list_of_a_missing_directory_is_empty_and_names_come_back_sorted() {
    let dir = scratch("dirdev-list");
    let dev = DirDev::new(dir.path());
    assert_eq!(dev.list("seg").unwrap(), Vec::<String>::new());
    for name in ["seg/3.seg", "seg/1.seg", "seg/2.seg.repair", "seg/10.seg"] {
        dev.create(name).unwrap();
    }
    dev.create("big/ab/x.obj").unwrap();
    assert_eq!(
        dev.list("seg").unwrap(),
        ["1.seg", "10.seg", "2.seg.repair", "3.seg"]
    );
    assert_eq!(dev.list("big").unwrap(), ["ab"]);
    assert_eq!(dev.list("").unwrap(), ["big", "seg"]);
}

// ---------------------------------------------------------------------------------------------
// DevFs.

#[test]
fn devfs_append_creates_the_file_and_its_directories_then_appends() {
    let dir = scratch("devfs-append");
    let fs = DevFs::new(Arc::new(DirDev::new(dir.path())));
    fs.append("tomb/1/1.log", b"abc").unwrap();
    fs.append("tomb/1/1.log", b"def").unwrap();
    assert_eq!(fs.len("tomb/1/1.log").unwrap(), 6);
    assert_eq!(fs.read_at("tomb/1/1.log", 0, 100).unwrap(), b"abcdef");
    assert_eq!(fs.read_at("tomb/1/1.log", 2, 2).unwrap(), b"cd");
    assert_eq!(fs.list("tomb/1").unwrap(), ["1.log"]);
    assert_eq!(fs.list("tomb/2").unwrap(), Vec::<String>::new());
    assert_eq!(
        std::fs::read(dir.path().join("tomb/1/1.log")).unwrap(),
        b"abcdef"
    );
}

#[test]
fn devfs_truncate_sync_and_remove() {
    let dir = scratch("devfs-trunc");
    let fs = DevFs::new(Arc::new(DirDev::new(dir.path())));
    fs.append("a/b/f", b"0123456789").unwrap();
    fs.truncate("a/b/f", 4).unwrap();
    assert_eq!(fs.read_at("a/b/f", 0, 100).unwrap(), b"0123");
    fs.sync("a/b/f").unwrap();
    fs.remove("a/b/f").unwrap();
    assert_eq!(fs.len("a/b/f").unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(fs.remove("a/b/f").unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(fs.sync("a/b/f").unwrap_err().kind(), ErrorKind::NotFound);
}

#[test]
fn devfs_sync_makes_a_nested_file_and_every_directory_above_it_durable() {
    let disk = FakeDisk::new();
    let dev = Arc::new(FakeDev(disk.clone(), Arc::new(AtomicBool::new(true))));
    let fs = DevFs::new(dev);
    fs.append("a/b/f", b"durable").unwrap();
    fs.sync("a/b/f").unwrap();
    disk.crash();
    disk.restart();
    assert_eq!(disk.read_all("a/b/f").unwrap(), b"durable");
}

#[test]
fn devfs_remove_is_durable_when_it_returns() {
    let disk = FakeDisk::new();
    let dev = Arc::new(FakeDev(disk.clone(), Arc::new(AtomicBool::new(true))));
    let fs = DevFs::new(dev);
    fs.append("a/f", b"x").unwrap();
    fs.sync("a/f").unwrap();
    fs.remove("a/f").unwrap();
    disk.crash();
    disk.restart();
    assert!(
        !disk.exists("a/f").unwrap(),
        "a removed file came back after a crash"
    );
}

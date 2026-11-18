//! The loads run against the prototype, and the cell sweeps of arms C-2 (one drive), C-3 (two
//! copies), C-4 (first byte), the mixed read and write arm and the slow-consumer arm. A load is
//! background streams (whole packs, or parts in ascending order, optionally striped over two
//! drives), demand reads arriving at a fixed interval (optionally hedged to the other copy), slow
//! consumers, and a write stream (local runs only).

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::Error;
use crate::clock::Clock;
use crate::cold::client::{Conn, Got};
use crate::cold::sched::{Class, Counts, Params};
use crate::cold::server::{Config, ReadPath, Served, Server};
use crate::diskstats::{IoStat, Sampler};
use crate::io::{MIB, write_data_file};
use crate::report::{Table, num};
use crate::run::{Span, measure};
use crate::stats::{Summary, mb_per_s, summarize};
use crate::workload::Rng;

/// The packs every drive holds: `p<i>.pack`, each `size` bytes (the same names on every drive,
/// so each pack has a copy per drive).
#[derive(Debug, Clone)]
pub struct Packs {
    /// How many.
    pub count: usize,
    /// Bytes each.
    pub size: u64,
}

impl Packs {
    /// The name of pack `i`.
    #[must_use]
    pub fn name(&self, i: usize) -> String {
        format!("p{}.pack", i % self.count.max(1))
    }
}

/// Writes the packs into each directory (drives in parallel); a pack already there with the
/// right size is kept.
///
/// # Errors
/// When a write fails.
pub fn prepare(dirs: &[PathBuf], packs: &Packs) -> Result<(), Error> {
    std::thread::scope(|scope| {
        let hs: Vec<_> = dirs
            .iter()
            .map(|d| {
                scope.spawn(move || -> Result<(), Error> {
                    std::fs::create_dir_all(d).map_err(Error::io("mkdir", d))?;
                    for i in 0..packs.count {
                        let p = d.join(packs.name(i));
                        if std::fs::metadata(&p).map(|m| m.len()).ok() == Some(packs.size) {
                            continue;
                        }
                        let _ = std::fs::remove_file(&p);
                        write_data_file(&p, packs.size, 4 << 20, i as u64)?;
                    }
                    Ok(())
                })
            })
            .collect();
        hs.into_iter()
            .try_for_each(|h| crate::run::joined(h.join()))
    })
}

/// A load.
#[derive(Debug, Clone, Serialize)]
pub struct Load {
    /// The drive the load reads.
    pub drive: String,
    /// Background streams.
    pub background: usize,
    /// Bytes per background GET (0: whole packs).
    pub part: u64,
    /// The background streams' class.
    pub class: Class,
    /// Each background stream reads its parts from these drives in turn, one at a time per
    /// drive (empty: from `drive` alone).
    pub stripe: Vec<String>,
    /// Bytes per demand read (0: none).
    pub demand: u64,
    /// Time between demand reads.
    pub every: Duration,
    /// Hedge a demand to this drive when it has no first byte after this long.
    pub hedge: Option<(String, Duration)>,
    /// Slow consumers (64 MiB parts, prepare class).
    pub slow: usize,
    /// Their rate, MB/s.
    pub slow_mb_s: f64,
}

/// What a load measured.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Background bytes inside the window.
    pub bg_bytes: u64,
    /// Background GET times, ms.
    pub bg_ms: Vec<f64>,
    /// Demand first-byte times, ms (the winner's, when hedged).
    pub first_ms: Vec<f64>,
    /// Demand last-byte times, ms.
    pub total_ms: Vec<f64>,
    /// Demands hedged.
    pub hedges: u64,
    /// Hedges whose first byte came first.
    pub hedge_wins: u64,
    /// Slow consumers' bytes inside the window.
    pub slow_bytes: u64,
    /// Answers other than 206.
    pub refused: u64,
}

fn keep(out: &Mutex<Outcome>, f: impl FnOnce(&mut Outcome)) {
    f(&mut out
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner));
}

fn background(
    addr: SocketAddr,
    packs: &Packs,
    load: &Load,
    j: usize,
    clock: &dyn Clock,
    span: Span,
    out: &Mutex<Outcome>,
) -> Result<(), Error> {
    let drives: Vec<String> = if load.stripe.is_empty() {
        vec![load.drive.clone()]
    } else {
        load.stripe.clone()
    };
    let part = if load.part == 0 {
        packs.size
    } else {
        load.part.min(packs.size)
    };
    let parts = packs.size / part;
    std::thread::scope(|scope| {
        let hs: Vec<_> = drives
            .iter()
            .enumerate()
            .map(|(k, d)| {
                let n = drives.len() as u64;
                scope.spawn(move || -> Result<(), Error> {
                    let mut c = Conn::open(addr)?;
                    let mut pack = j;
                    loop {
                        let mut i = k as u64;
                        while i < parts {
                            if clock.now() >= span.end {
                                return Ok(());
                            }
                            let t0 = clock.now();
                            let g = c.get(
                                d,
                                &packs.name(pack),
                                i * part,
                                part,
                                load.class,
                                clock,
                                None,
                            )?;
                            let t1 = clock.now();
                            if g.status != 206 {
                                keep(out, |o| o.refused += 1);
                                clock.sleep(Duration::from_millis(100));
                            } else if span.holds(t0, t1) {
                                keep(out, |o| {
                                    o.bg_bytes += g.bytes;
                                    o.bg_ms.push(g.total_ms);
                                });
                            }
                            i += n;
                        }
                        pack += load.background.max(1);
                    }
                })
            })
            .collect();
        hs.into_iter()
            .try_for_each(|h| crate::run::joined(h.join()))
    })
}

fn slow(
    addr: SocketAddr,
    packs: &Packs,
    load: &Load,
    j: usize,
    clock: &dyn Clock,
    span: Span,
    out: &Mutex<Outcome>,
) -> Result<(), Error> {
    let mut c = Conn::open(addr)?;
    let part = (64 * MIB).min(packs.size);
    let mut i = 0u64;
    while clock.now() < span.end {
        let off = (i % (packs.size / part)) * part;
        let t0 = clock.now();
        let g = c.get(
            &load.drive,
            &packs.name(j + 1000),
            off,
            part,
            Class::Prepare,
            clock,
            Some(load.slow_mb_s),
        )?;
        if span.holds(t0, clock.now()) {
            keep(out, |o| o.slow_bytes += g.bytes);
        }
        i += 1;
    }
    Ok(())
}

fn demands(
    addr: SocketAddr,
    packs: &Packs,
    load: &Load,
    clock: &dyn Clock,
    span: Span,
    out: &Mutex<Outcome>,
) -> Result<(), Error> {
    let mut rng = Rng::new(42);
    let cells = (packs.size.saturating_sub(load.demand) / MIB).max(1);
    let mut next = clock.now();
    while clock.now() < span.end {
        while clock.now() < next {
            clock.sleep(Duration::from_millis(1));
        }
        next += load.every;
        let pack = packs.name(usize::try_from(rng.below(packs.count as u64)).unwrap_or(0));
        let off = rng.below(cells) * MIB;
        let t0 = clock.now();
        let (first, total, hedged, won) = match &load.hedge {
            None => {
                let mut c = Conn::open(addr)?;
                let d = Class::Demand;
                let g = c.get(&load.drive, &pack, off, load.demand, d, clock, None)?;
                (g.first_ms, g.total_ms, false, false)
            }
            Some((other, after)) => hedged_get(
                addr,
                &load.drive,
                other,
                *after,
                &pack,
                off,
                load.demand,
                clock,
            )?,
        };
        if span.holds(t0, clock.now()) {
            keep(out, |o| {
                o.first_ms.push(first);
                o.total_ms.push(total);
                o.hedges += u64::from(hedged);
                o.hedge_wins += u64::from(won);
            });
        }
    }
    Ok(())
}

/// A demand to `drive`, and the same to `other` when the first has no answer after `after`;
/// returns the winner's first-byte and last-byte ms, whether it hedged, and whether the hedge won.
#[expect(clippy::too_many_arguments, reason = "the two copies and the read")]
fn hedged_get(
    addr: SocketAddr,
    drive: &str,
    other: &str,
    after: Duration,
    pack: &str,
    off: u64,
    len: u64,
    clock: &dyn Clock,
) -> Result<(f64, f64, bool, bool), Error> {
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let primary = scope.spawn(|| {
            let g = Conn::open(addr)?.get(drive, pack, off, len, Class::Demand, clock, None);
            done.store(true, Ordering::SeqCst);
            g
        });
        let t0 = clock.now();
        while !done.load(Ordering::SeqCst) && clock.now() - t0 < after {
            clock.sleep(Duration::from_millis(1));
        }
        let hedge = (!done.load(Ordering::SeqCst)).then(|| {
            let d = (clock.now() - t0).as_secs_f64() * 1000.0;
            scope.spawn(move || {
                Conn::open(addr)?
                    .get(other, pack, off, len, Class::Demand, clock, None)
                    .map(|g| (g, d))
            })
        });
        let p: Got = crate::run::joined(primary.join())?;
        let h = match hedge {
            Some(h) => Some(crate::run::joined(h.join())?),
            None => None,
        };
        Ok(winner(&p, h.as_ref()))
    })
}

/// The first-byte and last-byte ms of the read that answered first, whether a hedge was sent,
/// and whether it won; a hedge's times count from the primary's start (`d` ms later).
#[must_use]
pub fn winner(p: &Got, hedge: Option<&(Got, f64)>) -> (f64, f64, bool, bool) {
    match hedge {
        None => (p.first_ms, p.total_ms, false, false),
        Some((h, d)) if h.first_ms + d < p.first_ms => (h.first_ms + d, h.total_ms + d, true, true),
        Some(_) => (p.first_ms, p.total_ms, true, false),
    }
}

/// Runs `load` against the service at `addr` over `span`.
///
/// # Errors
/// When a stream fails.
pub fn run_load(
    addr: SocketAddr,
    packs: &Packs,
    load: &Load,
    clock: &dyn Clock,
    span: Span,
) -> Result<Outcome, Error> {
    let out = Mutex::new(Outcome::default());
    std::thread::scope(|scope| {
        let mut hs = Vec::new();
        for j in 0..load.background {
            let out = &out;
            hs.push(scope.spawn(move || background(addr, packs, load, j, clock, span, out)));
        }
        for j in 0..load.slow {
            let out = &out;
            hs.push(scope.spawn(move || slow(addr, packs, load, j, clock, span, out)));
        }
        if load.demand > 0 {
            let out = &out;
            hs.push(scope.spawn(move || demands(addr, packs, load, clock, span, out)));
        }
        hs.into_iter()
            .try_for_each(|h| crate::run::joined(h.join()))
    })?;
    Ok(out
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}

/// One cell of a local sweep: the server's settings and the load.
#[derive(Debug, Clone)]
pub struct Cell {
    /// The arm (`C-2`, `C-3`, `mixed`, `slow`, `C-4`).
    pub arm: String,
    /// What varies, as `name=value` words.
    pub label: String,
    /// The scheduler.
    pub params: Params,
    /// Bytes per chunk.
    pub chunk: usize,
    /// How streams read.
    pub path: ReadPath,
    /// Slot release for stalled consumers.
    pub release_after: Option<Duration>,
    /// The load.
    pub load: Load,
    /// A write stream of 1 GiB packs on the load's drive beside it.
    pub writes: bool,
}

/// A cell's row.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// The arm.
    pub arm: String,
    /// What varies.
    pub label: String,
    /// Background MB/s.
    pub bg_mb_s: f64,
    /// Background GET ms.
    pub bg: Option<Summary>,
    /// Demand first byte ms.
    pub first: Option<Summary>,
    /// Demand last byte ms.
    pub total: Option<Summary>,
    /// Hedges, and hedges that won.
    pub hedges: (u64, u64),
    /// Slow consumers' MB/s.
    pub slow_mb_s: f64,
    /// Write MB/s.
    pub write_mb_s: f64,
    /// Refused answers.
    pub refused: u64,
    /// The scheduler's counters, summed over drives.
    pub counts: Counts,
    /// Server-side queue wait of demand requests, ms.
    pub demand_wait: Option<Summary>,
    /// The drives' `iostat`.
    pub io: std::collections::BTreeMap<String, IoStat>,
}

/// The outcome of a load as a row.
#[must_use]
pub fn row(
    arm: &str,
    label: &str,
    o: &Outcome,
    secs: f64,
    served: &[Served],
    counts: Counts,
    write_bytes: u64,
) -> Row {
    let waits: Vec<f64> = served
        .iter()
        .filter(|s| s.class == Class::Demand)
        .map(|s| s.wait_ms)
        .collect();
    Row {
        arm: arm.into(),
        label: label.into(),
        bg_mb_s: mb_per_s(o.bg_bytes, secs),
        bg: summarize(&o.bg_ms),
        first: summarize(&o.first_ms),
        total: summarize(&o.total_ms),
        hedges: (o.hedges, o.hedge_wins),
        slow_mb_s: mb_per_s(o.slow_bytes, secs),
        write_mb_s: mb_per_s(write_bytes, secs),
        refused: o.refused,
        counts,
        demand_wait: summarize(&waits),
        io: std::collections::BTreeMap::new(),
    }
}

/// Runs one cell against an in-process server on loopback.
///
/// # Errors
/// When the server or the load fails.
pub fn local_cell(
    cell: &Cell,
    drives: &std::collections::BTreeMap<String, PathBuf>,
    packs: &Packs,
    clock: &dyn Clock,
    sampler: Option<&Sampler>,
    warmup: Duration,
    window: Duration,
) -> Result<Row, Error> {
    let server = Server::new(
        Config {
            drives: drives.clone(),
            params: cell.params,
            chunk: cell.chunk,
            path: cell.path,
            release_after: cell.release_after,
        },
        clock,
    );
    let listener = TcpListener::bind("127.0.0.1:0").map_err(Error::net)?;
    let addr = listener.local_addr().map_err(Error::net)?;
    let stop = AtomicBool::new(false);
    let written = AtomicU64::new(0);
    let r: Result<(Outcome, std::collections::BTreeMap<String, IoStat>), Error> =
        std::thread::scope(|scope| {
            let srv = scope.spawn(|| server.serve(&listener, &stop));
            let r = measure(clock, sampler, warmup, window, |span| {
                std::thread::scope(|inner| {
                    let w = cell.writes.then(|| {
                        inner.spawn(|| -> Result<(), Error> {
                            let mut k = 0;
                            while clock.now() < span.end {
                                let name = format!("w{k}.pack");
                                let t0 = clock.now();
                                server.write_pack(&cell.load.drive, &name, packs.size)?;
                                if span.holds(t0, clock.now()) {
                                    written.fetch_add(packs.size, Ordering::SeqCst);
                                }
                                k += 1;
                            }
                            for i in 0..k {
                                let _ = std::fs::remove_file(
                                    drives[&cell.load.drive].join(format!("w{i}.pack")),
                                );
                            }
                            Ok(())
                        })
                    });
                    let o = run_load(addr, packs, &cell.load, clock, span);
                    if let Some(h) = w {
                        crate::run::joined(h.join())?;
                    }
                    o
                })
            });
            stop.store(true, Ordering::SeqCst);
            let s = crate::run::joined(srv.join());
            let (o, io) = r?;
            s?;
            Ok((o, io))
        });
    let (o, io) = r?;
    let counts = server.counts();
    let mut r = row(
        &cell.arm,
        &cell.label,
        &o,
        window.as_secs_f64(),
        &server.take_served(),
        counts,
        written.load(Ordering::SeqCst),
    );
    r.io = io;
    Ok(r)
}

/// The design's defaults: 2 streams (1 write), 32 MiB slices, preemption at most 4 a second,
/// queues of 8; 4 MiB chunks at depth 2 with `O_DIRECT`.
#[must_use]
pub fn base(drive: &str) -> Cell {
    Cell {
        arm: "C-2".into(),
        label: "base".into(),
        params: Params {
            streams: 2,
            write_streams: 1,
            slice_bytes: 32 * MIB,
            preempt: true,
            preempt_max_per_s: 4,
            queue_max: 8,
        },
        chunk: 4 << 20,
        path: ReadPath::Direct(2),
        release_after: None,
        load: Load {
            drive: drive.into(),
            background: 4,
            part: 64 * MIB,
            class: Class::Prepare,
            stripe: Vec::new(),
            demand: MIB,
            every: Duration::from_millis(500),
            hedge: None,
            slow: 0,
            slow_mb_s: 0.0,
        },
        writes: false,
    }
}

fn with(c: &Cell, arm: &str, label: String, f: impl FnOnce(&mut Cell)) -> Cell {
    let mut c = c.clone();
    c.arm = arm.into();
    c.label = label;
    f(&mut c);
    c
}

/// The cells of C-2: streams, chunk, depth, slice, read path, preemption and demand size, whole
/// packs against parts, one at a time around the defaults.
#[must_use]
pub fn c2(drive: &str) -> Vec<Cell> {
    let b = base(drive);
    let mut v = Vec::new();
    for s in [1, 2, 3, 4] {
        v.push(with(&b, "C-2", format!("streams={s}"), |c| {
            c.params.streams = s
        }));
    }
    for mib in [1, 2, 8] {
        v.push(with(&b, "C-2", format!("chunk={mib}MiB"), |c| {
            c.chunk = mib << 20
        }));
    }
    for d in [1, 4] {
        v.push(with(&b, "C-2", format!("depth={d}"), |c| {
            c.path = ReadPath::Direct(d)
        }));
    }
    for s in [8, 16, 64, 128] {
        v.push(with(&b, "C-2", format!("slice={s}MiB"), |c| {
            c.params.slice_bytes = s * MIB
        }));
    }
    v.push(with(&b, "C-2", "path=sendfile+dontneed".into(), |c| {
        c.path = ReadPath::Sendfile
    }));
    for (pre, len) in [(true, 4), (false, 1), (false, 4)] {
        v.push(with(
            &b,
            "C-2",
            format!("preempt={pre} demand={len}MiB"),
            |c| {
                c.params.preempt = pre;
                c.load.demand = len * MIB;
            },
        ));
    }
    v.push(with(&b, "C-2", "whole-packs".into(), |c| c.load.part = 0));
    v
}

/// The cells of C-3 run on one node: one copy streaming against two copies striped (a single
/// background stream), and a demand hedged to the other copy against a loaded drive.
#[must_use]
pub fn c3(a: &str, b: &str) -> Vec<Cell> {
    let one = with(&base(a), "C-3", "one-copy-stream".into(), |c| {
        c.load.background = 1;
        c.load.demand = 0;
    });
    vec![
        one.clone(),
        with(&one, "C-3", "two-copies-striped".into(), |c| {
            c.load.stripe = vec![a.into(), b.into()]
        }),
        with(&base(a), "C-3", "loaded-no-hedge".into(), |c| {
            c.load.background = 6
        }),
        with(&base(a), "C-3", "loaded-hedge-30ms".into(), |c| {
            c.load.background = 6;
            c.load.hedge = Some((b.into(), Duration::from_millis(30)));
        }),
    ]
}

/// The cells of C-4 on one node: a GET of each size at concurrency 1 on an idle drive.
#[must_use]
pub fn c4(drive: &str) -> Vec<Cell> {
    [1, 2, 4, 8]
        .into_iter()
        .map(|mib| {
            with(&base(drive), "C-4", format!("idle get={mib}MiB"), |c| {
                c.load.background = 0;
                c.load.demand = mib * MIB;
                c.load.every = Duration::from_millis(100);
            })
        })
        .collect()
}

/// The mixed arm: one write stream and one read stream with demands, with and without
/// preemption of the write.
#[must_use]
pub fn mixed(drive: &str) -> Vec<Cell> {
    [true, false]
        .into_iter()
        .map(|pre| {
            with(
                &base(drive),
                "mixed",
                format!("1 write + 1 read preempt={pre}"),
                |c| {
                    c.writes = true;
                    c.load.background = 1;
                    c.params.preempt = pre;
                },
            )
        })
        .collect()
}

/// The slow-consumer arm: two consumers reading at 10 MB/s beside two normal streams, with the
/// slot held and with it given back after 200 ms of a full socket.
#[must_use]
pub fn slow_cells(drive: &str) -> Vec<Cell> {
    [None, Some(Duration::from_millis(200))]
        .into_iter()
        .map(|rel| {
            with(
                &base(drive),
                "slow",
                format!(
                    "2 slow at 10MB/s release={}",
                    rel.map_or("none".into(), |d| format!("{}ms", d.as_millis()))
                ),
                |c| {
                    c.load.background = 2;
                    c.load.slow = 2;
                    c.load.slow_mb_s = 10.0;
                    c.release_after = rel;
                },
            )
        })
        .collect()
}

/// The cells of the named sweeps (`c2`, `c3`, `c4`, `mixed`, `slow`), loading drive `first`
/// (`other` holds the second copy).
///
/// # Errors
/// When a name is none of those.
pub fn cells_for(arms: &[String], first: &str, other: &str) -> Result<Vec<Cell>, Error> {
    let mut cells = Vec::new();
    for a in arms {
        cells.extend(match a.as_str() {
            "c2" => c2(first),
            "c3" => c3(first, other),
            "c4" => c4(first),
            "mixed" => mixed(first),
            "slow" => slow_cells(first),
            x => {
                return Err(Error::Invalid(format!(
                    "arm {x}: want c2, c3, c4, mixed or slow"
                )));
            }
        });
    }
    Ok(cells)
}

/// The tables of the cold arms.
#[must_use]
pub fn tables(rows: &[Row]) -> Vec<Table> {
    let mut out = Vec::new();
    let mut arms: Vec<&str> = rows.iter().map(|r| r.arm.as_str()).collect();
    arms.dedup();
    for arm in arms {
        let mut t = Table::new(
            &format!("{arm}, cold-tier prototype"),
            &[
                "cell",
                "bg MB/s",
                "bg GET p50 ms",
                "demand first p50",
                "p99",
                "max",
                "demand last p99",
                "server wait p99",
                "hedged/won",
                "slow MB/s",
                "write MB/s",
                "yields",
                "preempts",
                "parks",
                "util %",
            ],
        );
        for r in rows.iter().filter(|r| r.arm == arm) {
            let f =
                |s: Option<Summary>, g: fn(Summary) -> f64| s.map_or("-".into(), |s| num(g(s), 1));
            let util: Vec<String> =
                r.io.iter()
                    .map(|(k, v)| format!("{k}:{}", num(v.util, 0)))
                    .collect();
            t.row(vec![
                r.label.clone(),
                num(r.bg_mb_s, 1),
                f(r.bg, |s| s.p50),
                f(r.first, |s| s.p50),
                f(r.first, |s| s.p99),
                f(r.first, |s| s.max),
                f(r.total, |s| s.p99),
                f(r.demand_wait, |s| s.p99),
                format!("{}/{}", r.hedges.0, r.hedges.1),
                num(r.slow_mb_s, 1),
                num(r.write_mb_s, 1),
                r.counts.yields.to_string(),
                r.counts.preemptions.to_string(),
                r.counts.parks.to_string(),
                util.join(" "),
            ]);
        }
        out.push(t);
    }
    out
}

#[cfg(test)]
mod tests;

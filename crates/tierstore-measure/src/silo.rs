//! Arm C-1, silo's ranged GETs (the baseline the cold tier is compared with), and the listing
//! that arms C-6 (object sizes) and C-7 (placement) read. GETs go through the repository's own
//! S3 client to the node's local silo; each cell runs `conc` concurrent readers of `part`-byte
//! ranges, each range read once in the whole run (a cursor walks the packs), and reports MB/s,
//! first-byte and whole-part latency, and the node's drives' `iostat` (the kB read per byte
//! fetched shows erasure-coding amplification and cache hits). With several nodes, each takes
//! the packs whose index is its own modulo the node count and all start at one wall time.

use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use futures::StreamExt as _;
use objstore_s3::s3::{Credentials, S3Config, S3Store};
use objstore_s3::{ByteRange, Capabilities, KeyPrefix, ListToken, ObjectKey, PageSize};
use serde::Serialize;

use crate::Error;
use crate::clock::Clock;
use crate::diskstats::{IoStat, Sampler};
use crate::io::MIB;
use crate::report::{Table, num};
use crate::run::{Span, measure};
use crate::stats::{Summary, mb_per_s, summarize};

/// Reads silo's root credentials from its environment file (`MINIO_ROOT_USER=` and
/// `MINIO_ROOT_PASSWORD=` lines, values quoted or not).
///
/// # Errors
/// When the file cannot be read or lacks either line.
pub fn credentials(path: &Path) -> Result<Credentials, Error> {
    let text = std::fs::read_to_string(path).map_err(Error::io("read", path))?;
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(k)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
    };
    match (get("MINIO_ROOT_USER"), get("MINIO_ROOT_PASSWORD")) {
        (Some(u), Some(p)) => Ok(Credentials::new(u, p)),
        _ => Err(Error::Invalid(format!(
            "{} has no MINIO_ROOT_USER and MINIO_ROOT_PASSWORD",
            path.display()
        ))),
    }
}

/// A client of `bucket` at `endpoint`.
///
/// # Errors
/// When the endpoint or bucket is not usable.
pub fn store(endpoint: &str, bucket: &str, creds: Credentials) -> Result<S3Store, Error> {
    S3Store::new(S3Config {
        endpoint: endpoint.to_string(),
        region: "us-east-1".into(),
        bucket: bucket.to_string(),
        credentials: creds,
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(120),
        min_body_rate: None,
    })
    .map_err(Error::net)
}

/// Writes every object under `prefix` as `key<TAB>size` lines to `out`; returns the count.
/// With `after`, the listing starts after that key and the lines are appended to `out`, so a
/// listing that failed part-way resumes from the last key it wrote.
///
/// # Errors
/// When the listing or the write fails (the lines written so far stay in `out`).
pub async fn list(
    store: &S3Store,
    prefix: &str,
    after: Option<&str>,
    out: &Path,
) -> Result<u64, Error> {
    let p = KeyPrefix::new(prefix).map_err(|e| Error::Invalid(e.to_string()))?;
    let from = after
        .map(|k| ObjectKey::new(k).map_err(|e| Error::Invalid(e.to_string())))
        .transpose()?
        .map(|k| ListToken::after(&k));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(from.is_some())
        .truncate(from.is_none())
        .open(out)
        .map_err(Error::io("create", out))?;
    let mut f = std::io::BufWriter::new(file);
    let mut s = std::pin::pin!(objstore_s3::listing::objects(
        store,
        &p,
        from,
        PageSize::MAX
    ));
    let mut n = 0;
    let r = loop {
        let Some(o) = s.next().await else {
            break Ok(n);
        };
        match o {
            Ok(o) => {
                writeln!(f, "{}\t{}", o.key.as_str(), o.size).map_err(Error::io("write", out))?;
                n += 1;
            }
            Err(e) => break Err(Error::net(e)),
        }
    };
    f.flush().map_err(Error::io("write", out))?;
    r
}

/// The ranges a run reads: each `part` bytes of each pack once, packs in turn.
#[derive(Debug)]
pub struct Cursor {
    packs: Vec<(String, u64)>,
    i: usize,
    off: u64,
    wrapped: bool,
}

impl Cursor {
    /// A cursor over `packs` (key and size), the ones with index `node` modulo `nodes`.
    #[must_use]
    pub fn new(packs: &[(String, u64)], node: usize, nodes: usize) -> Self {
        let n = nodes.max(1);
        Self {
            packs: packs
                .iter()
                .enumerate()
                .filter(|(i, _)| i % n == node % n)
                .map(|(_, p)| p.clone())
                .collect(),
            i: 0,
            off: 0,
            wrapped: false,
        }
    }

    /// The next range of `part` bytes, or `None` when there are no packs at least `part` long.
    pub fn next(&mut self, part: u64) -> Option<(String, u64)> {
        for _ in 0..=self.packs.len() * 2 {
            let (k, size) = self.packs.get(self.i)?.clone();
            if self.off + part <= size {
                let off = self.off;
                self.off += part;
                return Some((k, off));
            }
            self.i += 1;
            self.off = 0;
            if self.i >= self.packs.len() {
                self.i = 0;
                self.wrapped = true;
            }
        }
        None
    }

    /// Whether it has started over (ranges are being read a second time).
    #[must_use]
    pub fn wrapped(&self) -> bool {
        self.wrapped
    }
}

/// One cell.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// Nodes reading at once.
    pub nodes: usize,
    /// Part size in MiB.
    pub part_mib: u64,
    /// Concurrent GETs on this node.
    pub conc: usize,
    /// MB/s of this node.
    pub mb_s: f64,
    /// Time to the first byte, ms.
    pub first_byte: Option<Summary>,
    /// Time to the last byte, ms.
    pub total: Option<Summary>,
    /// The node's drives.
    pub io: std::collections::BTreeMap<String, IoStat>,
    /// KiB read from this node's drives per KiB fetched.
    pub disk_per_byte: f64,
    /// Whether some range was read a second time in the run.
    pub reread: bool,
    /// Wall time of the cell's measured window, ms since the epoch.
    pub from_unix_ms: u64,
    /// Its end.
    pub end_unix_ms: u64,
}

/// What a reader of a cell measured.
#[derive(Debug, Default)]
struct Got {
    bytes: u64,
    first: Vec<f64>,
    total: Vec<f64>,
}

async fn reader(
    store: &S3Store,
    cursor: &Mutex<Cursor>,
    part: u64,
    clock: &dyn Clock,
    span: Span,
) -> Result<Got, Error> {
    let mut g = Got::default();
    loop {
        let t0 = clock.now();
        if t0 >= span.end {
            return Ok(g);
        }
        let next = cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next(part);
        let Some((key, off)) = next else {
            return Err(Error::Invalid(format!(
                "no pack holds a part of {part} bytes"
            )));
        };
        let k = ObjectKey::new(key).map_err(|e| Error::Invalid(e.to_string()))?;
        let range =
            ByteRange::new(off, part).ok_or_else(|| Error::Invalid("an empty range".into()))?;
        let mut read = store
            .get_stream(&k, Some(range))
            .await
            .map_err(Error::net)?;
        let mut first = None;
        let mut n = 0u64;
        while let Some(piece) = read.body.next().await {
            let piece = piece.map_err(Error::net)?;
            first.get_or_insert_with(|| clock.now());
            n += piece.len() as u64;
        }
        let t1 = clock.now();
        if span.holds(t0, t1) {
            g.bytes += n;
            g.first
                .push((first.unwrap_or(t1) - t0).as_secs_f64() * 1000.0);
            g.total.push((t1 - t0).as_secs_f64() * 1000.0);
        }
    }
}

/// The settings of the GET arm.
#[derive(Debug, Clone)]
pub struct Config {
    /// Part sizes in MiB.
    pub parts_mib: Vec<u64>,
    /// Concurrent GETs per node.
    pub conc: Vec<usize>,
    /// Warm-up per cell.
    pub warmup: Duration,
    /// Measured time per cell.
    pub measure: Duration,
    /// Nodes reading at once (for the table; each node runs its own process).
    pub nodes: usize,
    /// The drives of this node.
    pub devs: Vec<String>,
    /// `/proc/diskstats`.
    pub diskstats: std::path::PathBuf,
}

/// Runs every cell with `store`, reading the ranges `cursor` gives.
///
/// # Errors
/// When a GET fails.
pub fn run(
    cfg: &Config,
    store: &S3Store,
    cursor: Cursor,
    clock: &dyn Clock,
    rt: &tokio::runtime::Runtime,
) -> Result<Vec<Row>, Error> {
    let cursor = Mutex::new(cursor);
    let sampler = Sampler::new(&cfg.diskstats, &cfg.devs);
    let mut rows = Vec::new();
    for &mib in &cfg.parts_mib {
        for &conc in &cfg.conc {
            let unix0 = clock.unix_secs();
            let (got, io) = measure(clock, Some(&sampler), cfg.warmup, cfg.measure, |span| {
                rt.block_on(async {
                    let tasks = (0..conc).map(|_| reader(store, &cursor, mib * MIB, clock, span));
                    let all = futures::future::join_all(tasks).await;
                    let mut g = Got::default();
                    for r in all {
                        let r = r?;
                        g.bytes += r.bytes;
                        g.first.extend(r.first);
                        g.total.extend(r.total);
                    }
                    Ok(g)
                })
            })?;
            let rkb: f64 =
                io.values().map(|s| s.rkb_s).fold(0.0, |a, b| a + b) * cfg.measure.as_secs_f64();
            let from =
                (unix0 * 1000).saturating_add(u64::try_from(cfg.warmup.as_millis()).unwrap_or(0));
            rows.push(Row {
                nodes: cfg.nodes,
                part_mib: mib,
                conc,
                mb_s: mb_per_s(got.bytes, cfg.measure.as_secs_f64()),
                first_byte: summarize(&got.first),
                total: summarize(&got.total),
                io,
                disk_per_byte: if got.bytes == 0 {
                    0.0
                } else {
                    rkb * 1024.0 / got.bytes as f64
                },
                reread: cursor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .wrapped(),
                from_unix_ms: from,
                end_unix_ms: from + u64::try_from(cfg.measure.as_millis()).unwrap_or(0),
            });
        }
    }
    Ok(rows)
}

/// The arm's table for one node.
#[must_use]
pub fn table(node: &str, rows: &[Row]) -> Table {
    let mut t = Table::new(
        &format!("C-1, silo ranged GETs: {node}"),
        &[
            "nodes",
            "part MiB",
            "GETs/node",
            "MB/s (node)",
            "first byte p50",
            "p99",
            "part p50 ms",
            "p99",
            "rareq-sz KiB",
            "util %",
            "disk KiB/KiB",
            "reread",
        ],
    );
    for r in rows {
        let n = r.io.len().max(1) as f64;
        t.row(vec![
            r.nodes.to_string(),
            r.part_mib.to_string(),
            r.conc.to_string(),
            num(r.mb_s, 1),
            r.first_byte.map_or("-".into(), |s| num(s.p50, 1)),
            r.first_byte.map_or("-".into(), |s| num(s.p99, 1)),
            r.total.map_or("-".into(), |s| num(s.p50, 1)),
            r.total.map_or("-".into(), |s| num(s.p99, 1)),
            num(
                r.io.values().map(|s| s.rareq_kb).fold(0.0, |a, b| a + b) / n,
                0,
            ),
            num(r.io.values().map(|s| s.util).fold(0.0, |a, b| a + b) / n, 1),
            num(r.disk_per_byte, 2),
            r.reread.to_string(),
        ]);
    }
    t
}

#[cfg(test)]
mod tests;

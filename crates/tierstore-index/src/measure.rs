//! The measurement of 10.2: bytes per key on disk, point-read latency and bulk-load time of the
//! index on `fjall` or on `redb`, run by the `tierstore-index-measure` binary.
//!
//! [`run`] opens a [`KvIndex`] on the chosen engine in an empty directory, loads `keys` entries
//! in key order through [`Index::replay`] (the bulk load of recovery step 3), persists, and then
//! reads `reads` random keys one at a time, timing each read. The keys have the shape of the CAS
//! service's: `packs/`, 16 hex digits that grow with the entry's number (so generation order is key
//! order), and 48 hex digits of a hash of the number. Everything is a function of the options, the
//! seed included, so two runs on one machine measure the same store.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use tierstore_format::block::BlockGeometry;
use tierstore_format::digest::blake3_256;
use tierstore_format::ids::{BlockId, Generation, NodeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_store_api::index::{
    EntryLocation, Fact, Index, IndexEntry, IndexError, Lookup, ReplayMark,
};

use crate::backend::fjall::{FjallBackend, FjallOptions};
use crate::backend::redb::{RedbBackend, RedbOptions};
use crate::backend::{Backend, BackendError};
use crate::kv::{IndexConfig, KvIndex};

/// The embedded store to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// `fjall`, an LSM-tree: the design's choice.
    Fjall,
    /// `redb`, a copy-on-write B-tree: the alternative.
    Redb,
}

impl Engine {
    /// The name the flag `--engine` takes.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fjall => "fjall",
            Self::Redb => "redb",
        }
    }
}

/// What one run measures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The store.
    pub engine: Engine,
    /// The directory of the store: absent or empty.
    pub dir: PathBuf,
    /// The entries to load.
    pub keys: u64,
    /// The point reads to time.
    pub reads: u64,
    /// The store's cache, in bytes.
    pub cache_bytes: u64,
    /// The entries per `replay` call of the load.
    pub batch: usize,
    /// The seed of the keys the reads choose.
    pub seed: u64,
}

/// Why a measurement did not run or did not finish.
#[derive(Debug, thiserror::Error)]
pub enum MeasureError {
    /// A flag that is required was not given.
    #[error("missing required flag {0}")]
    MissingFlag(&'static str),
    /// A flag the tool does not know.
    #[error("unknown flag {0}")]
    UnknownFlag(String),
    /// A flag was the last argument, with no value.
    #[error("flag {0} needs a value")]
    MissingValue(String),
    /// An argument that is not valid UTF-8.
    #[error("argument {0:?} is not valid UTF-8")]
    NotUtf8(OsString),
    /// A flag's value is not acceptable.
    #[error("flag {flag}: {value:?} is not {expected}")]
    BadValue {
        /// The flag.
        flag: &'static str,
        /// What was given.
        value: String,
        /// What it should have been.
        expected: &'static str,
    },
    /// The directory of the store already holds something.
    #[error("directory {} is not empty", .0.display())]
    DirNotEmpty(PathBuf),
    /// A file or directory could not be read.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// What was being done.
        op: &'static str,
        /// The path.
        path: PathBuf,
        /// What the system said.
        source: std::io::Error,
    },
    /// The store failed to open.
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// The index failed.
    #[error(transparent)]
    Index(#[from] IndexError),
    /// A key just loaded was not found live by a point read.
    #[error("the key {0} was loaded but a point read did not find it live")]
    NotFound(String),
}

impl MeasureError {
    /// Whether the person who ran the tool asked for something it cannot do (exit code 2), as
    /// against a failure while measuring (exit code 1).
    #[must_use]
    pub const fn is_usage(&self) -> bool {
        !matches!(
            self,
            Self::Io { .. } | Self::Backend(_) | Self::Index(_) | Self::NotFound(_)
        )
    }
}

const DEFAULT_READS: u64 = 100_000;
const DEFAULT_CACHE_MIB: u64 = 256;
const DEFAULT_BATCH: u64 = 100_000;
const MIB: u64 = 1024 * 1024;

/// The number `value` of `flag`.
fn number(flag: &'static str, value: &str) -> Result<u64, MeasureError> {
    value.parse::<u64>().map_err(|_| MeasureError::BadValue {
        flag,
        value: value.to_owned(),
        expected: "a number",
    })
}

/// Reads `--engine fjall|redb --dir <path> --keys <n> [--reads <n>] [--cache-mib <n>]
/// [--batch <n>] [--seed <n>]`. The defaults are 100000 reads, a cache of 256 MiB, batches of
/// 100000 entries and seed 1.
///
/// # Errors
/// A flag that is missing, unknown or without a value, a value that is not a number, or an engine
/// other than `fjall` and `redb`. Zero keys, reads or batch are refused by [`run`].
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Options, MeasureError> {
    parse_args(&mut args.into_iter())
}

/// [`parse`] on one iterator type.
fn parse_args(args: &mut dyn Iterator<Item = OsString>) -> Result<Options, MeasureError> {
    let mut engine = None;
    let mut dir = None;
    let mut keys = None;
    let mut reads = DEFAULT_READS;
    let mut cache_mib = DEFAULT_CACHE_MIB;
    let mut batch = DEFAULT_BATCH;
    let mut seed = 1;
    while let Some(flag) = args.next() {
        let flag = flag.into_string().map_err(MeasureError::NotUtf8)?;
        let value = args
            .next()
            .ok_or_else(|| MeasureError::MissingValue(flag.clone()))?;
        if flag == "--dir" {
            dir = Some(PathBuf::from(value));
            continue;
        }
        let value = value.into_string().map_err(MeasureError::NotUtf8)?;
        match flag.as_str() {
            "--engine" => {
                engine = Some(match value.as_str() {
                    "fjall" => Engine::Fjall,
                    "redb" => Engine::Redb,
                    _ => {
                        return Err(MeasureError::BadValue {
                            flag: "--engine",
                            value,
                            expected: "fjall or redb",
                        });
                    }
                });
            }
            "--keys" => keys = Some(number("--keys", &value)?),
            "--reads" => reads = number("--reads", &value)?,
            "--cache-mib" => cache_mib = number("--cache-mib", &value)?,
            "--batch" => batch = number("--batch", &value)?,
            "--seed" => seed = number("--seed", &value)?,
            _ => return Err(MeasureError::UnknownFlag(flag)),
        }
    }
    Ok(Options {
        engine: engine.ok_or(MeasureError::MissingFlag("--engine"))?,
        dir: dir.ok_or(MeasureError::MissingFlag("--dir"))?,
        keys: keys.ok_or(MeasureError::MissingFlag("--keys"))?,
        reads,
        cache_bytes: cache_mib.saturating_mul(MIB),
        batch: usize::try_from(batch).unwrap_or(usize::MAX),
        seed,
    })
}

/// What one run measured.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The store.
    pub engine: Engine,
    /// The entries loaded.
    pub keys: u64,
    /// The wall time of the load and the persist after it, in milliseconds.
    pub load_ms: u64,
    /// The size of every file under the directory after the persist.
    pub disk_bytes: u64,
    /// `disk_bytes` per key.
    pub bytes_per_key: f64,
    /// The cache the store ran with.
    pub cache_bytes: u64,
    /// The median point read, in microseconds.
    pub read_p50_us: f64,
    /// The 99th percentile point read, in microseconds.
    pub read_p99_us: f64,
    /// The slowest point read, in microseconds.
    pub read_max_us: f64,
    /// The process's peak resident set (`VmHWM`), or 0 where it cannot be read.
    pub peak_rss_bytes: u64,
}

impl Report {
    /// The report as `name<TAB>value` lines, one per field.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        vec![
            format!("engine\t{}", self.engine.name()),
            format!("keys\t{}", self.keys),
            format!("load_ms\t{}", self.load_ms),
            format!("disk_bytes\t{}", self.disk_bytes),
            format!("bytes_per_key\t{:.2}", self.bytes_per_key),
            format!("cache_bytes\t{}", self.cache_bytes),
            format!("read_p50_us\t{:.3}", self.read_p50_us),
            format!("read_p99_us\t{:.3}", self.read_p99_us),
            format!("read_max_us\t{:.3}", self.read_max_us),
            format!("peak_rss_bytes\t{}", self.peak_rss_bytes),
        ]
    }
}

/// The key of entry `i` of `keys`: `packs/`, 16 hex digits of `i` spread over the `u64` range
/// (so the key order is the order of `i`), and 48 hex digits of the blake3 of `i`.
fn key_of(i: u64, keys: u64) -> Vec<u8> {
    let spread = i * (u64::MAX / keys);
    let hash = blake3_256(&i.to_le_bytes());
    let mut key = format!("packs/{spread:016x}");
    for byte in &hash[..24] {
        // Writing to a String does not fail.
        let _ = write!(key, "{byte:02x}");
    }
    key.into_bytes()
}

/// The entry of number `i`: a record in block `i / 1000` of node 1, a few KiB to a MiB long.
fn entry_of(i: u64) -> IndexEntry {
    let hash = blake3_256(&i.to_le_bytes());
    let size_bits = u32::from_le_bytes([hash[24], hash[25], hash[26], hash[27]]);
    IndexEntry {
        generation: Generation::of(&i.to_le_bytes()),
        location: EntryLocation::Block {
            block: BlockId::from_raw((1 << 56) | (i / 1000)),
            offset: (i % 1000) * 1_048_576,
        },
        length: 4096 + u64::from(size_bits % 1_048_576),
        stamp: Stamp::from_raw(((1_700_000_000_000 + i / 65_536) << 16) | (i % 65_536)),
        flags: RecordFlags(0),
        prior_blocks: Vec::new(),
        promoted: None,
    }
}

/// The wall clock of the measurement.
#[expect(
    clippy::disallowed_methods,
    reason = "a measurement tool times what it measures"
)]
fn now() -> Instant {
    Instant::now()
}

/// The bytes of every file under `dir`.
fn disk_bytes(dir: &Path) -> Result<u64, MeasureError> {
    let io = |op: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| MeasureError::Io { op, path, source }
    };
    let mut total = 0;
    for entry in std::fs::read_dir(dir).map_err(io("read dir", dir))? {
        let entry = entry.map_err(io("read dir", dir))?;
        let path = entry.path();
        let meta = entry.metadata().map_err(io("stat", &path))?;
        total += if meta.is_dir() {
            disk_bytes(&path)?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

/// The `VmHWM` of a `/proc/<pid>/status` text, in bytes; 0 when it has none.
fn peak_rss(status: &str) -> u64 {
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .and_then(|kib| kib.trim().parse::<u64>().ok())
        .map_or(0, |kib| kib * 1024)
}

/// The value at quantile `q` (0 to 1) of `sorted`, nearest rank.
fn quantile(sorted: &[u64], q: f64) -> u64 {
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Loads `opts.keys` entries in key order, `opts.batch` per `replay`, and persists.
fn load(index: &dyn Index, opts: &Options) -> Result<(), MeasureError> {
    let mut start = 0;
    while start < opts.keys {
        let end = start.saturating_add(opts.batch as u64).min(opts.keys);
        let mut facts = (start..end).map(|i| Fact::Record {
            key: key_of(i, opts.keys),
            entry: entry_of(i),
        });
        index.replay(&mut facts)?;
        start = end;
    }
    index.flush_mark(&ReplayMark::default())?;
    Ok(())
}

/// The time of `opts.reads` point reads of random loaded keys, in nanoseconds each.
fn read_times(index: &dyn Index, opts: &Options) -> Result<Vec<u64>, MeasureError> {
    // xorshift64; the state is never zero.
    let mut state = (opts.seed ^ 0x9E37_79B9_7F4A_7C15) | 1;
    let mut times = Vec::new();
    for _ in 0..opts.reads {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let key = key_of(state % opts.keys, opts.keys);
        let started = now();
        let found = index.get(&key)?;
        let took = started.elapsed();
        if !matches!(found, Lookup::Live(_)) {
            return Err(MeasureError::NotFound(
                String::from_utf8_lossy(&key).into_owned(),
            ));
        }
        times.push(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX));
    }
    Ok(times)
}

/// The whole measurement on an index over `backend`.
fn measure<B: Backend>(backend: B, opts: &Options) -> Result<Report, MeasureError> {
    let index = KvIndex::open(
        backend,
        IndexConfig {
            node: NodeId(1),
            geometry: BlockGeometry::DEFAULT,
        },
    )?;
    let started = now();
    load(&index, opts)?;
    let load_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let disk = disk_bytes(&opts.dir)?;
    index.set_ready(true);
    let mut times = read_times(&index, opts)?;
    times.sort_unstable();
    let us = |nanos: u64| nanos as f64 / 1000.0;
    Ok(Report {
        engine: opts.engine,
        keys: opts.keys,
        load_ms,
        disk_bytes: disk,
        bytes_per_key: disk as f64 / opts.keys as f64,
        cache_bytes: opts.cache_bytes,
        read_p50_us: us(quantile(&times, 0.5)),
        read_p99_us: us(quantile(&times, 0.99)),
        read_max_us: us(quantile(&times, 1.0)),
        peak_rss_bytes: std::fs::read_to_string("/proc/self/status")
            .map_or(0, |status| peak_rss(&status)),
    })
}

/// Refuses the options that measure nothing: zero keys, reads or batch.
fn validate(opts: &Options) -> Result<(), MeasureError> {
    let counts = [
        ("--keys", opts.keys),
        ("--reads", opts.reads),
        ("--batch", opts.batch as u64),
    ];
    for (flag, n) in counts {
        if n == 0 {
            return Err(MeasureError::BadValue {
                flag,
                value: n.to_string(),
                expected: "a positive number",
            });
        }
    }
    Ok(())
}

/// Runs the measurement of `opts`.
///
/// # Errors
/// Zero keys, reads or batch, a directory that is not empty, a store or an index that fails, a file that cannot be read.
pub fn run(opts: &Options) -> Result<Report, MeasureError> {
    validate(opts)?;
    if let Ok(mut entries) = std::fs::read_dir(&opts.dir)
        && entries.next().is_some()
    {
        return Err(MeasureError::DirNotEmpty(opts.dir.clone()));
    }
    match opts.engine {
        Engine::Fjall => measure(
            FjallBackend::open(
                &opts.dir,
                &FjallOptions {
                    cache_bytes: opts.cache_bytes,
                },
            )?,
            opts,
        ),
        Engine::Redb => measure(
            RedbBackend::open(
                &opts.dir,
                &RedbOptions {
                    cache_bytes: opts.cache_bytes,
                },
            )?,
            opts,
        ),
    }
}

/// The tool: parses `args` (without the program name), runs, prints the report as `name<TAB>value`
/// lines on standard output. A usage error exits 2 and a failure exits 1, each with a message on
/// standard error.
#[must_use]
pub fn main(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let report = parse(args).and_then(|opts| run(&opts));
    match report {
        Ok(report) => {
            let mut out = std::io::stdout().lock();
            for line in report.lines() {
                // A closed pipe is the reader's choice, not a failure of the measurement.
                let _ = writeln!(out, "{line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "tierstore-index-measure: {e}");
            ExitCode::from(if e.is_usage() { 2 } else { 1 })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MemBackend;
    use tierstore_testkit::dir::TestDir;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn opts(keys: u64, reads: u64, batch: usize) -> Options {
        Options {
            engine: Engine::Fjall,
            dir: PathBuf::from("unused"),
            keys,
            reads,
            cache_bytes: 0,
            batch,
            seed: 7,
        }
    }

    fn mem_index() -> KvIndex<MemBackend> {
        KvIndex::open(
            MemBackend::new(),
            IndexConfig {
                node: NodeId(1),
                geometry: BlockGeometry::DEFAULT,
            },
        )
        .expect("open on memory")
    }

    #[test]
    fn keys_ascend_with_their_number_and_have_the_shape_of_a_pack_key() {
        let keys = 1000;
        let all: Vec<Vec<u8>> = (0..keys).map(|i| key_of(i, keys)).collect();
        assert!(all.windows(2).all(|w| w[0] < w[1]));
        assert!(all.iter().all(|k| k.len() == 6 + 16 + 48));
        assert!(all[0].starts_with(b"packs/0000000000000000"));
        assert_eq!(key_of(5, keys), key_of(5, keys));
    }

    #[test]
    fn entries_point_into_blocks_of_a_thousand_and_carry_distinct_stamps() {
        let a = entry_of(0);
        let b = entry_of(1999);
        assert_eq!(
            a.location,
            EntryLocation::Block {
                block: BlockId::from_raw(1 << 56),
                offset: 0
            }
        );
        assert_eq!(
            b.location,
            EntryLocation::Block {
                block: BlockId::from_raw((1 << 56) | 1),
                offset: 999 * 1_048_576
            }
        );
        assert!(a.stamp < b.stamp);
        assert!(a.length >= 4096);
        assert!(a.length < 4096 + 1_048_576);
    }

    #[test]
    fn load_puts_every_key_live_whatever_the_batch_size() {
        for batch in [1, 7, 50, 1000] {
            let index = mem_index();
            let o = opts(50, 1, batch);
            load(&index, &o).expect("load");
            index.set_ready(true);
            for i in 0..50 {
                assert_eq!(
                    index.get(&key_of(i, 50)).expect("get"),
                    Lookup::Live(entry_of(i)),
                    "batch {batch} key {i}"
                );
            }
        }
    }

    #[test]
    fn read_times_gives_one_time_per_read() {
        let index = mem_index();
        let o = opts(20, 33, 20);
        load(&index, &o).expect("load");
        index.set_ready(true);
        assert_eq!(read_times(&index, &o).expect("reads").len(), 33);
    }

    #[test]
    fn read_times_names_the_key_a_read_did_not_find() {
        let index = mem_index();
        index.set_ready(true);
        let o = opts(20, 1, 20);
        let err = read_times(&index, &o).expect_err("empty index");
        assert!(matches!(err, MeasureError::NotFound(_)));
        assert_eq!(
            err.to_string().split(' ').nth(2).map(|k| &k[..6]),
            Some("packs/")
        );
        assert!(!err.is_usage());
    }

    #[test]
    fn read_times_fails_with_the_index_while_it_is_still_indexing() {
        let index = mem_index();
        let o = opts(20, 1, 20);
        let err = read_times(&index, &o).expect_err("indexing");
        assert!(matches!(err, MeasureError::Index(IndexError::Indexing)));
        assert!(!err.is_usage());
    }

    #[test]
    fn quantile_takes_the_nearest_rank() {
        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(quantile(&sorted, 0.5), 50);
        assert_eq!(quantile(&sorted, 0.99), 99);
        assert_eq!(quantile(&sorted, 1.0), 100);
        assert_eq!(quantile(&[9], 0.5), 9);
        assert_eq!(quantile(&[9, 10], 0.0), 9);
    }

    #[test]
    fn peak_rss_reads_vmhwm_in_bytes_and_is_zero_without_one() {
        let status = "Name:\tx\nVmPeak:\t 900 kB\nVmHWM:\t    1234 kB\nVmRSS:\t 5 kB\n";
        assert_eq!(peak_rss(status), 1234 * 1024);
        assert_eq!(peak_rss("Name:\tx\n"), 0);
        assert_eq!(peak_rss("VmHWM:\tmany kB\n"), 0);
        assert_eq!(peak_rss("VmHWM:\t12\n"), 0);
        assert_eq!(peak_rss(""), 0);
    }

    #[test]
    fn parse_applies_the_defaults() {
        let o = parse(args(&["--engine", "redb", "--dir", "d", "--keys", "10"])).expect("parse");
        assert_eq!(
            o,
            Options {
                engine: Engine::Redb,
                dir: PathBuf::from("d"),
                keys: 10,
                reads: 100_000,
                cache_bytes: 256 * 1024 * 1024,
                batch: 100_000,
                seed: 1,
            }
        );
    }

    #[test]
    fn parse_takes_every_flag_in_any_order() {
        let o = parse(args(&[
            "--seed",
            "0",
            "--batch",
            "5",
            "--cache-mib",
            "0",
            "--reads",
            "3",
            "--keys",
            "9",
            "--dir",
            "x/y",
            "--engine",
            "fjall",
        ]))
        .expect("parse");
        assert_eq!(
            o,
            Options {
                engine: Engine::Fjall,
                dir: PathBuf::from("x/y"),
                keys: 9,
                reads: 3,
                cache_bytes: 0,
                batch: 5,
                seed: 0,
            }
        );
    }

    #[test]
    fn run_refuses_zero_keys_reads_or_batch() {
        for (flag, o) in [
            ("--keys", opts(0, 1, 1)),
            ("--reads", opts(1, 0, 1)),
            ("--batch", opts(1, 1, 0)),
        ] {
            let e = run(&o).expect_err("zero");
            assert_eq!(
                e.to_string(),
                format!("flag {flag}: \"0\" is not a positive number")
            );
            assert!(e.is_usage());
        }
    }

    #[test]
    fn parse_saturates_a_cache_too_large_for_u64() {
        let o = parse(args(&[
            "--engine",
            "fjall",
            "--dir",
            "d",
            "--keys",
            "1",
            "--cache-mib",
            "18446744073709551615",
        ]))
        .expect("parse");
        assert_eq!(o.cache_bytes, u64::MAX);
    }

    fn parse_err(list: &[&str]) -> String {
        let e = parse(args(list)).expect_err("must fail");
        assert!(e.is_usage());
        e.to_string()
    }

    #[test]
    fn parse_names_the_flag_that_is_missing_or_unknown() {
        assert_eq!(
            parse_err(&["--dir", "d", "--keys", "1"]),
            "missing required flag --engine"
        );
        assert_eq!(
            parse_err(&["--engine", "redb", "--keys", "1"]),
            "missing required flag --dir"
        );
        assert_eq!(
            parse_err(&["--engine", "redb", "--dir", "d"]),
            "missing required flag --keys"
        );
        assert_eq!(parse_err(&["--bogus", "1"]), "unknown flag --bogus");
        assert_eq!(
            parse_err(&["--engine", "redb", "--keys"]),
            "flag --keys needs a value"
        );
    }

    #[test]
    fn parse_names_the_value_that_is_wrong() {
        assert_eq!(
            parse_err(&["--engine", "sqlite"]),
            "flag --engine: \"sqlite\" is not fjall or redb"
        );
        assert_eq!(
            parse_err(&["--keys", "ten"]),
            "flag --keys: \"ten\" is not a number"
        );
        assert_eq!(
            parse_err(&["--batch", "-1"]),
            "flag --batch: \"-1\" is not a number"
        );
        assert_eq!(
            parse_err(&["--cache-mib", "x"]),
            "flag --cache-mib: \"x\" is not a number"
        );
        assert_eq!(
            parse_err(&["--seed", "x"]),
            "flag --seed: \"x\" is not a number"
        );
    }

    #[cfg(unix)]
    #[test]
    fn parse_refuses_an_argument_that_is_not_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0xff, 0xfe]);
        let e = parse([bad.clone()]).expect_err("flag");
        assert!(matches!(e, MeasureError::NotUtf8(_)));
        assert!(e.is_usage());
        let e = parse([OsString::from("--keys"), bad.clone()]).expect_err("value");
        assert!(matches!(e, MeasureError::NotUtf8(_)));
        // A directory may be any bytes.
        let o = parse([
            OsString::from("--dir"),
            bad,
            OsString::from("--engine"),
            OsString::from("redb"),
            OsString::from("--keys"),
            OsString::from("1"),
        ])
        .expect("dir");
        assert_eq!(o.dir.as_os_str().len(), 2);
    }

    #[test]
    fn report_lines_name_every_field_with_a_tab() {
        let report = Report {
            engine: Engine::Redb,
            keys: 10,
            load_ms: 20,
            disk_bytes: 1000,
            bytes_per_key: 100.0,
            cache_bytes: 4096,
            read_p50_us: 1.5,
            read_p99_us: 2.25,
            read_max_us: 9.0,
            peak_rss_bytes: 777,
        };
        assert_eq!(
            report.lines(),
            [
                "engine\tredb",
                "keys\t10",
                "load_ms\t20",
                "disk_bytes\t1000",
                "bytes_per_key\t100.00",
                "cache_bytes\t4096",
                "read_p50_us\t1.500",
                "read_p99_us\t2.250",
                "read_max_us\t9.000",
                "peak_rss_bytes\t777",
            ]
        );
    }

    #[test]
    fn disk_bytes_sums_files_of_nested_directories() {
        let scratch = TestDir::under(std::env::temp_dir(), "measure-sum");
        let root = scratch.path().to_path_buf();
        std::fs::create_dir_all(root.join("a/b")).expect("mkdir");
        std::fs::write(root.join("x"), [0u8; 10]).expect("write");
        std::fs::write(root.join("a/y"), [0u8; 20]).expect("write");
        std::fs::write(root.join("a/b/z"), [0u8; 30]).expect("write");
        let total = disk_bytes(&root);
        let missing = disk_bytes(&root.join("nope"));
        assert_eq!(total.expect("sum"), 60);
        let e = missing.expect_err("missing dir");
        assert!(matches!(e, MeasureError::Io { op: "read dir", .. }));
        assert!(!e.is_usage());
    }
}

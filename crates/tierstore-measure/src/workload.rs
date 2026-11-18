//! The read engine of the raw-drive arms: concurrent streams, each reading fixed-size requests
//! from its own files, adjacently (the next request starts where the last ended) or scattered
//! (each request at a random aligned offset), for a warm-up and then a measured window.

use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::Error;
use crate::clock::Clock;
use crate::io::{AlignedBuf, Mode, open_read, read_full_at};
use crate::run::Span;

/// Where a stream's requests go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Pattern {
    /// Each request follows the previous one in the stream's first file.
    Adjacent,
    /// Each request is at a random request-aligned offset of a random file of the stream.
    Scattered,
}

impl Pattern {
    /// The name used in tables.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Adjacent => "adjacent",
            Self::Scattered => "scattered",
        }
    }
}

/// One stream: its files and how it walks them.
#[derive(Debug, Clone)]
pub struct Stream {
    /// The files it reads (only the first, for [`Pattern::Adjacent`]).
    pub files: Vec<PathBuf>,
    /// How it chooses offsets.
    pub pattern: Pattern,
    /// The seed of its random offsets.
    pub seed: u64,
}

/// A cell of reads.
#[derive(Debug, Clone)]
pub struct ReadSpec {
    /// The concurrent streams.
    pub streams: Vec<Stream>,
    /// Bytes per request (a multiple of 4 KiB for [`Mode::Direct`]).
    pub req: usize,
    /// Direct or buffered.
    pub mode: Mode,
}

/// What a cell of reads did in its measured window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ReadResult {
    /// Bytes of the requests that started and ended inside the window.
    pub bytes: u64,
    /// The window's length in seconds.
    pub secs: f64,
    /// Each such request's latency in milliseconds.
    pub lat_ms: Vec<f64>,
}

/// A small fast generator for offsets (xorshift64*).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed` (any value).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1)
    }

    /// The next value.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value below `n` (0 for `n` 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

struct Opened {
    file: std::fs::File,
    len: u64,
    path: PathBuf,
}

fn open_stream(s: &Stream, mode: Mode) -> Result<Vec<Opened>, Error> {
    let mut out = Vec::new();
    for p in &s.files {
        let file = open_read(p, mode)?;
        let len = file.metadata().map_err(Error::io("stat", p))?.len();
        out.push(Opened {
            file,
            len,
            path: p.clone(),
        });
    }
    Ok(out)
}

/// Runs one cell: every stream on its own thread until the window ends.
///
/// # Errors
/// When a file cannot be opened or read, a stream has no file, or a file is shorter than a
/// request.
pub fn run_reads(spec: &ReadSpec, clock: &dyn Clock, span: Span) -> Result<ReadResult, Error> {
    let req = spec.req as u64;
    let mut opened = Vec::new();
    for s in &spec.streams {
        let files = open_stream(s, spec.mode)?;
        if files.is_empty() || files.iter().any(|f| f.len < req) {
            return Err(Error::Invalid(format!(
                "a stream needs files of at least {req} bytes each"
            )));
        }
        opened.push(files);
    }
    let end = span.end;
    let out = Mutex::new(ReadResult {
        secs: span.secs(),
        ..ReadResult::default()
    });
    let failed: Mutex<Option<Error>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for (files, s) in opened.iter().zip(&spec.streams) {
            let out = &out;
            let failed = &failed;
            scope.spawn(move || {
                let mut rng = Rng::new(s.seed);
                let mut buf = AlignedBuf::new(spec.req);
                let mut cursor = 0u64;
                let mut lat = Vec::new();
                let mut bytes = 0u64;
                loop {
                    let t0 = clock.now();
                    if t0 >= end {
                        break;
                    }
                    let (f, off) = match s.pattern {
                        Pattern::Adjacent => {
                            let f = &files[0];
                            if cursor + req > f.len {
                                cursor = 0;
                            }
                            let off = cursor;
                            cursor += req;
                            (f, off)
                        }
                        Pattern::Scattered => {
                            let f =
                                &files[usize::try_from(rng.below(files.len() as u64)).unwrap_or(0)];
                            (f, rng.below(f.len / req) * req)
                        }
                    };
                    if let Err(e) = read_full_at(&f.file, buf.as_mut_slice(), off) {
                        let mut g = failed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *g = Some(Error::io("read", &f.path)(e));
                        break;
                    }
                    let t1 = clock.now();
                    if span.holds(t0, t1) {
                        lat.push((t1 - t0).as_secs_f64() * 1000.0);
                        bytes += req;
                    }
                }
                let mut g = out
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                g.bytes += bytes;
                g.lat_ms.extend(lat);
            });
        }
    });
    if let Some(e) = failed
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Err(e);
    }
    Ok(out
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::StepClock;
    use crate::io::write_data_file;
    use std::time::Duration;
    use tierstore_testkit::dir::TestDir;

    fn span(c: &StepClock) -> Span {
        Span::starting(c, Duration::from_millis(5), Duration::from_millis(100))
    }

    fn files(d: &TestDir, n: usize, size: u64) -> Vec<PathBuf> {
        (0..n)
            .map(|i| {
                let p = d.path().join(format!("f{i}"));
                write_data_file(&p, size, 4096, i as u64).unwrap();
                p
            })
            .collect()
    }

    fn spec(streams: Vec<Stream>, mode: Mode) -> ReadSpec {
        ReadSpec {
            streams,
            req: 4096,
            mode,
        }
    }

    #[test]
    fn adjacent_and_scattered_streams_count_only_the_window() {
        let d = TestDir::under(std::env::temp_dir(), "workload");
        let f = files(&d, 2, 3 * 4096);
        let streams = vec![
            Stream {
                files: vec![f[0].clone()],
                pattern: Pattern::Adjacent,
                seed: 1,
            },
            Stream {
                files: f.clone(),
                pattern: Pattern::Scattered,
                seed: 2,
            },
        ];
        for mode in [Mode::Direct, Mode::Buffered] {
            let clock = StepClock::new(Duration::from_millis(1), 0);
            let r = run_reads(&spec(streams.clone(), mode), &clock, span(&clock)).unwrap();
            assert!(r.bytes > 0);
            assert_eq!(r.bytes, 4096 * r.lat_ms.len() as u64);
            assert!((r.secs - 0.1).abs() < 1e-9);
            // Every request read took clock steps; none is negative or outside the window.
            assert!(r.lat_ms.iter().all(|&l| l > 0.0));
            assert!(r.lat_ms.iter().all(|&l| l <= 100.0));
            // The window holds at most its share of the clock's reads.
            assert!(r.lat_ms.len() <= 100);
        }
        assert_eq!(Pattern::Adjacent.name(), "adjacent");
        assert_eq!(Pattern::Scattered.name(), "scattered");
    }

    #[test]
    fn refuses_a_stream_without_files_or_with_a_short_file_or_a_missing_one() {
        let d = TestDir::under(std::env::temp_dir(), "workload");
        let clock = StepClock::new(Duration::from_millis(1), 0);
        let none = Stream {
            files: vec![],
            pattern: Pattern::Adjacent,
            seed: 0,
        };
        assert!(run_reads(&spec(vec![none], Mode::Buffered), &clock, span(&clock)).is_err());
        let short = d.path().join("short");
        std::fs::write(&short, b"abc").unwrap();
        let s = Stream {
            files: vec![short],
            pattern: Pattern::Adjacent,
            seed: 0,
        };
        assert!(run_reads(&spec(vec![s], Mode::Buffered), &clock, span(&clock)).is_err());
        let gone = Stream {
            files: vec![d.path().join("gone")],
            pattern: Pattern::Adjacent,
            seed: 0,
        };
        assert!(run_reads(&spec(vec![gone], Mode::Buffered), &clock, span(&clock)).is_err());
    }

    #[test]
    fn a_failed_read_ends_the_cell_with_its_error() {
        // A directory opens for reading but every read of it fails (EISDIR); its length is
        // made large enough by the request size of 0 bytes being refused first, so use a
        // request of one byte against a directory whose metadata length is at least one.
        let d = TestDir::under(std::env::temp_dir(), "workload");
        let s = Stream {
            files: vec![d.path().to_path_buf()],
            pattern: Pattern::Adjacent,
            seed: 0,
        };
        let mut sp = spec(vec![s], Mode::Buffered);
        sp.req = 1;
        let clock = StepClock::new(Duration::from_millis(1), 0);
        let e = run_reads(&sp, &clock, span(&clock)).unwrap_err();
        assert!(e.to_string().starts_with("read "), "{e}");
    }

    #[test]
    fn rng_below_stays_below_and_zero_is_zero() {
        let mut r = Rng::new(9);
        assert!((0..1000).all(|_| r.below(7) < 7));
        assert_eq!(r.below(0), 0);
    }
}

//! Arm D, CPU: MB/s per core of sha256 (the cell digest), blake3 (the placement hash) and
//! crc32c (the footer checksum), on one core and on every core at once, and whether the CPU
//! has the SHA extensions. Decides the hash pool's threads and the service's `cpu.weight`.

use std::time::Duration;

use serde::Serialize;
use sha2::Digest as _;

use crate::clock::Clock;
use crate::report::{Table, num};
use crate::stats::mb_per_s;

/// A hash the arm measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Hash {
    /// SHA-256 of 1 MiB cells.
    Sha256,
    /// BLAKE3.
    Blake3,
    /// CRC-32C.
    Crc32c,
}

impl Hash {
    /// Every hash, in table order.
    pub const ALL: [Self; 3] = [Self::Sha256, Self::Blake3, Self::Crc32c];

    /// The name used in tables.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Blake3 => "blake3",
            Self::Crc32c => "crc32c",
        }
    }

    /// Hashes `buf` and returns one byte of the result (so the work is not optimised away).
    #[must_use]
    pub fn digest(self, buf: &[u8]) -> u8 {
        match self {
            Self::Sha256 => sha2::Sha256::digest(buf)[0],
            Self::Blake3 => blake3::hash(buf).as_bytes()[0],
            Self::Crc32c => crc32c::crc32c(buf).to_le_bytes()[0],
        }
    }
}

/// MB/s of `hash` over 1 MiB buffers on one thread for `secs`.
#[must_use]
pub fn rate(hash: Hash, clock: &dyn Clock, secs: Duration) -> f64 {
    let mut buf = vec![0u8; 1 << 20];
    crate::io::fill(&mut buf, 3);
    let t0 = clock.now();
    let mut bytes = 0u64;
    let mut acc = 0u8;
    while clock.now() - t0 < secs {
        acc ^= hash.digest(&buf);
        bytes += buf.len() as u64;
    }
    std::hint::black_box(acc);
    mb_per_s(bytes, (clock.now() - t0).as_secs_f64())
}

/// One row: a hash on `threads` threads at once, MB/s per thread and in total.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// The hash.
    pub hash: Hash,
    /// Threads run at once.
    pub threads: usize,
    /// Mean MB/s of one thread.
    pub per_thread: f64,
    /// MB/s of all of them.
    pub total: f64,
}

/// Each hash on one thread and on `all` threads at once.
#[must_use]
pub fn run(clock: &dyn Clock, secs: Duration, all: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for hash in Hash::ALL {
        for threads in [1, all.max(1)] {
            let rates: Vec<f64> = std::thread::scope(|scope| {
                let hs: Vec<_> = (0..threads)
                    .map(|_| scope.spawn(move || rate(hash, clock, secs)))
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap_or(0.0)).collect()
            });
            let total: f64 = rates.iter().fold(0.0, |a, b| a + b);
            rows.push(Row {
                hash,
                threads,
                per_thread: total / threads as f64,
                total,
            });
        }
    }
    rows
}

/// Whether the `flags` line of a `/proc/cpuinfo` text names `sha_ni`.
#[must_use]
pub fn has_sha_ni(cpuinfo: &str) -> bool {
    cpuinfo
        .lines()
        .find(|l| l.starts_with("flags"))
        .is_some_and(|l| l.split_whitespace().any(|w| w == "sha_ni"))
}

/// The arm's table, for one node.
#[must_use]
pub fn table(node: &str, sha_ni: bool, rows: &[Row]) -> Table {
    let mut t = Table::new(
        &format!(
            "D, CPU: {node} (sha_ni: {})",
            if sha_ni { "yes" } else { "no" }
        ),
        &["hash", "threads", "MB/s per thread", "MB/s total"],
    );
    for r in rows {
        t.row(vec![
            r.hash.name().into(),
            r.threads.to_string(),
            num(r.per_thread, 0),
            num(r.total, 0),
        ]);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::StepClock;

    #[test]
    fn every_hash_runs_on_one_and_many_threads() {
        let c = StepClock::new(Duration::from_millis(1), 0);
        let rows = run(&c, Duration::from_millis(3), 2);
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|r| r.total > 0.0));
        assert_eq!(rows[1].threads, 2);
        let t = table("n3", true, &rows).markdown();
        assert!(t.starts_with("### D, CPU: n3 (sha_ni: yes)"));
        assert!(t.contains("| crc32c | 2 |"));
        assert!(table("n3", false, &[]).markdown().contains("sha_ni: no"));
        assert_eq!(run(&c, Duration::from_millis(1), 0)[1].threads, 1);
    }

    #[test]
    fn digests_differ_by_input() {
        for h in Hash::ALL {
            let a = (0..8).map(|i| h.digest(&[i])).collect::<Vec<_>>();
            assert!(a.windows(2).any(|w| w[0] != w[1]), "{}", h.name());
        }
    }

    #[test]
    fn sha_ni_flag() {
        assert!(has_sha_ni("processor: 0\nflags\t: fpu sse sha_ni avx\n"));
        assert!(!has_sha_ni("flags\t: fpu sse avx\n"));
        assert!(!has_sha_ni("processor: 0\n"));
    }
}

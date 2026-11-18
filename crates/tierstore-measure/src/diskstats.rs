//! `iostat -x` from two reads of `/proc/diskstats`: what each cell reports per drive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::Error;

/// The cumulative counters of one block device (the first eleven fields after the name).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Reads completed.
    pub reads: u64,
    /// 512-byte sectors read.
    pub read_sectors: u64,
    /// Milliseconds spent by reads.
    pub read_ms: u64,
    /// Writes completed.
    pub writes: u64,
    /// 512-byte sectors written.
    pub write_sectors: u64,
    /// Milliseconds spent by writes.
    pub write_ms: u64,
    /// Milliseconds the device had I/O in flight.
    pub io_ms: u64,
    /// Weighted milliseconds of I/O (the time-queue integral).
    pub weighted_ms: u64,
}

/// The counters of every device in a `/proc/diskstats` text, by name. A line that does not
/// have the fields is skipped.
#[must_use]
pub fn parse(text: &str) -> BTreeMap<String, Counters> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 14 {
            continue;
        }
        let n: Vec<u64> = f[3..14].iter().filter_map(|x| x.parse().ok()).collect();
        if n.len() != 11 {
            continue;
        }
        out.insert(
            f[2].to_string(),
            Counters {
                reads: n[0],
                read_sectors: n[2],
                read_ms: n[3],
                writes: n[4],
                write_sectors: n[6],
                write_ms: n[7],
                io_ms: n[9],
                weighted_ms: n[10],
            },
        );
    }
    out
}

/// What `iostat -x` shows for one device over an interval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct IoStat {
    /// Reads per second.
    pub r_s: f64,
    /// KiB read per second.
    pub rkb_s: f64,
    /// Mean read request size in KiB (`rareq-sz`).
    pub rareq_kb: f64,
    /// Mean read wait in ms (`r_await`).
    pub r_await: f64,
    /// Writes per second.
    pub w_s: f64,
    /// KiB written per second.
    pub wkb_s: f64,
    /// Mean write request size in KiB.
    pub wareq_kb: f64,
    /// Mean write wait in ms.
    pub w_await: f64,
    /// Mean queue length (`aqu-sz`).
    pub aqu_sz: f64,
    /// Percent of the interval with I/O in flight (`%util`).
    pub util: f64,
}

fn per(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

/// The interval statistics from `a` to `b` over `secs` seconds (a counter that went back counts
/// as zero).
#[must_use]
pub fn iostat(a: &Counters, b: &Counters, secs: f64) -> IoStat {
    let d = |x: u64, y: u64| y.saturating_sub(x);
    let reads = d(a.reads, b.reads);
    let writes = d(a.writes, b.writes);
    let rsec = d(a.read_sectors, b.read_sectors);
    let wsec = d(a.write_sectors, b.write_sectors);
    let s = if secs > 0.0 { secs } else { f64::INFINITY };
    IoStat {
        r_s: reads as f64 / s,
        rkb_s: rsec as f64 / 2.0 / s,
        rareq_kb: per(rsec, reads) / 2.0,
        r_await: per(d(a.read_ms, b.read_ms), reads),
        w_s: writes as f64 / s,
        wkb_s: wsec as f64 / 2.0 / s,
        wareq_kb: per(wsec, writes) / 2.0,
        w_await: per(d(a.write_ms, b.write_ms), writes),
        aqu_sz: d(a.weighted_ms, b.weighted_ms) as f64 / 1000.0 / s,
        util: d(a.io_ms, b.io_ms) as f64 / 10.0 / s,
    }
}

/// Reads the counters of a fixed set of devices.
#[derive(Debug, Clone)]
pub struct Sampler {
    path: PathBuf,
    devices: Vec<String>,
}

/// One read of a sampler's devices.
pub type Snapshot = BTreeMap<String, Counters>;

impl Sampler {
    /// A sampler of `devices` from the diskstats file at `path` (`/proc/diskstats` in a run).
    #[must_use]
    pub fn new(path: &Path, devices: &[String]) -> Self {
        Self {
            path: path.to_path_buf(),
            devices: devices.to_vec(),
        }
    }

    /// The devices it samples.
    #[must_use]
    pub fn devices(&self) -> &[String] {
        &self.devices
    }

    /// The counters of its devices now.
    ///
    /// # Errors
    /// When the file cannot be read, or a device is not in it.
    pub fn snapshot(&self) -> Result<Snapshot, Error> {
        let text = std::fs::read_to_string(&self.path).map_err(Error::io("read", &self.path))?;
        let all = parse(&text);
        let mut out = Snapshot::new();
        for dev in &self.devices {
            let c = all.get(dev).ok_or_else(|| {
                Error::Invalid(format!("device {dev} is not in {}", self.path.display()))
            })?;
            out.insert(dev.clone(), *c);
        }
        Ok(out)
    }
}

/// The interval statistics of every device in both snapshots.
#[must_use]
pub fn delta(a: &Snapshot, b: &Snapshot, secs: f64) -> BTreeMap<String, IoStat> {
    a.iter()
        .filter_map(|(k, ca)| b.get(k).map(|cb| (k.clone(), iostat(ca, cb, secs))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tierstore_testkit::dir::TestDir;

    const TEXT: &str = "   8      16 sdb 100 5 2048 50 10 0 160 30 0 120 80 0 0 0 0 0 0\n\
                        7       1 loop1 1 0 8 1 2 0 16 2 0 3 3\n\
                        bad line\n\
                        8 0 sdz x 0 0 0 0 0 0 0 0 0 0\n";

    #[test]
    fn parses_counters_and_skips_short_or_garbled_lines() {
        let m = parse(TEXT);
        assert_eq!(m.len(), 2);
        let c = m["sdb"];
        assert_eq!(
            c,
            Counters {
                reads: 100,
                read_sectors: 2048,
                read_ms: 50,
                writes: 10,
                write_sectors: 160,
                write_ms: 30,
                io_ms: 120,
                weighted_ms: 80
            }
        );
        assert_eq!(m["loop1"].writes, 2);
    }

    #[test]
    fn iostat_matches_the_iostat_formulas() {
        let a = Counters::default();
        let b = Counters {
            reads: 100,
            read_sectors: 204_800,
            read_ms: 1500,
            writes: 4,
            write_sectors: 64,
            write_ms: 8,
            io_ms: 1500,
            weighted_ms: 3000,
        };
        let s = iostat(&a, &b, 2.0);
        assert!((s.r_s - 50.0).abs() < 1e-9);
        assert!((s.rkb_s - 51_200.0).abs() < 1e-9);
        assert!((s.rareq_kb - 1024.0).abs() < 1e-9);
        assert!((s.r_await - 15.0).abs() < 1e-9);
        assert!((s.w_s - 2.0).abs() < 1e-9);
        assert!((s.wkb_s - 16.0).abs() < 1e-9);
        assert!((s.wareq_kb - 8.0).abs() < 1e-9);
        assert!((s.w_await - 2.0).abs() < 1e-9);
        assert!((s.aqu_sz - 1.5).abs() < 1e-9);
        assert!((s.util - 75.0).abs() < 1e-9);
    }

    #[test]
    fn no_time_or_no_requests_gives_zeros_not_nan() {
        let s = iostat(&Counters::default(), &Counters::default(), 0.0);
        assert_eq!(s, IoStat::default());
        let back = Counters {
            reads: 5,
            ..Counters::default()
        };
        assert!(iostat(&back, &Counters::default(), 1.0).r_s.abs() < 1e-12);
    }

    #[test]
    fn sampler_reads_its_devices_and_names_a_missing_one() {
        let dir = TestDir::under(std::env::temp_dir(), "diskstats");
        let p = dir.path().join("diskstats");
        std::fs::write(&p, TEXT).unwrap();
        let s = Sampler::new(&p, &["sdb".into()]);
        assert_eq!(s.devices(), ["sdb".to_string()]);
        let a = s.snapshot().unwrap();
        let d = delta(&a, &a, 1.0);
        assert_eq!(d.len(), 1);
        assert!(d["sdb"].util.abs() < 1e-12);
        let e = Sampler::new(&p, &["sdq".into()]).snapshot().unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("device sdq is not in {}", p.display())
        );
        let e = Sampler::new(&dir.path().join("none"), &[])
            .snapshot()
            .unwrap_err();
        assert!(e.to_string().starts_with("read "));
    }

    #[test]
    fn delta_skips_a_device_missing_from_the_second_snapshot() {
        let mut a = Snapshot::new();
        a.insert("sdb".into(), Counters::default());
        assert!(delta(&a, &Snapshot::new(), 1.0).is_empty());
    }
}

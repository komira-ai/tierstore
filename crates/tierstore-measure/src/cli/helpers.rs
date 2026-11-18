//! Helpers of the verbs: flag parsing into the arms' configurations, the idle check, the
//! runtime and waiting for a start time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Common, Env, Hdd, Write5};
use crate::Error;
use crate::blockdev::{backing_file, device_of};
use crate::clock::Clock;
use crate::io::MIB;
use crate::run::preflight;
use crate::workload::Pattern;

pub(super) fn wait_until(clock: &dyn Clock, at: Option<u64>) {
    let ahead = at.map_or(0, |t| t.saturating_sub(clock.unix_secs()));
    let end = clock.now() + Duration::from_secs(ahead);
    while clock.now() < end {
        clock.sleep(Duration::from_millis(100));
    }
}

pub(super) fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

pub(super) fn patterns(names: &[String]) -> Result<Vec<Pattern>, Error> {
    names
        .iter()
        .map(|n| match n.as_str() {
            "adjacent" => Ok(Pattern::Adjacent),
            "scattered" => Ok(Pattern::Scattered),
            other => Err(Error::Invalid(format!(
                "pattern {other}: want adjacent or scattered"
            ))),
        })
        .collect()
}

pub(super) fn labelled(v: &str) -> Result<(String, PathBuf), Error> {
    v.split_once('=')
        .map(|(l, p)| (l.to_string(), PathBuf::from(p)))
        .ok_or_else(|| Error::Invalid(format!("{v}: want label=path")))
}

/// The devices to sample for a directory: its device, and for a loop the device under its image.
pub(super) fn devices_of(dir: &Path, sys: &Path) -> Result<Vec<String>, Error> {
    let dev = device_of(dir, sys)?;
    let mut v = vec![dev.clone()];
    if let Some(img) = backing_file(sys, &dev) {
        v.push(device_of(&img, sys)?);
    }
    Ok(v)
}

pub(super) fn idle(c: &Common, devs: &[String], env: &Env<'_>) -> Result<(), Error> {
    if c.idle_secs > 0 {
        let s = crate::diskstats::Sampler::new(&c.diskstats(), devs);
        preflight(&s, env.clock, c.idle_secs, c.idle_max_util)?;
    }
    Ok(())
}

pub(super) fn hdd_config(h: &Hdd) -> Result<crate::hdd::Config, Error> {
    let drives = h
        .dir
        .iter()
        .map(|d| {
            std::fs::create_dir_all(d).map_err(Error::io("mkdir", d))?;
            Ok(crate::hdd::Drive {
                dir: d.clone(),
                dev: device_of(d, &h.common.sys_root)?,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(crate::hdd::Config {
        drives,
        sets: h.sets.clone(),
        sizes_mib: h.sizes.clone(),
        streams: h.streams.clone(),
        modes: h
            .modes
            .iter()
            .map(|m| crate::hdd::ReadMode::parse(m))
            .collect::<Result<_, _>>()?,
        patterns: patterns(&h.patterns)?,
        files: h.files.max(h.streams.iter().copied().max().unwrap_or(1)),
        file_size: h.file_mib * MIB,
        warmup: ms(h.warmup_ms),
        measure: ms(h.measure_ms),
        sys: h.common.sys_root.clone(),
    })
}

pub(super) fn write_config(w: &Write5) -> Result<crate::write::Config, Error> {
    std::fs::create_dir_all(&w.dir).map_err(Error::io("mkdir", &w.dir))?;
    Ok(crate::write::Config {
        dir: w.dir.clone(),
        dev: device_of(&w.dir, &w.common.sys_root)?,
        sizes_mib: w.sizes.clone(),
        writers: w.writers.clone(),
        measure: ms(w.measure_ms),
        sustained: w.sustained_mib * MIB,
        pack: w.pack_mib * MIB,
        window: w.window_mib * MIB,
        rewrite_cap: w.rewrite_cap_mib * MIB,
        keep_free: w.keep_free,
        diskstats: w.common.diskstats(),
    })
}

pub(super) fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Error::net)
}

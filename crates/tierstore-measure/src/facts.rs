//! Arm E, node facts, and arm C-6's drive facts: every value is read (a file, a directory of
//! files, a read-only program, a process's limits), never changed. A fact that cannot be read is
//! recorded as such, not skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::run::Runner;

/// Where a fact comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file's text.
    File(PathBuf),
    /// Every file of a directory, as `name=value` lines.
    Dir(PathBuf),
    /// A program's standard output (read-only programs only).
    Cmd(Vec<String>),
    /// The `Max locked memory` limit and the `Seccomp` mode of the processes named so.
    Process(String),
}

/// A named fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Its name in the output.
    pub name: String,
    /// Where it is read.
    pub source: Source,
}

fn file(name: &str, p: impl Into<PathBuf>) -> Probe {
    Probe {
        name: name.into(),
        source: Source::File(p.into()),
    }
}

fn dir(name: &str, p: impl Into<PathBuf>) -> Probe {
    Probe {
        name: name.into(),
        source: Source::Dir(p.into()),
    }
}

fn cmd(name: &str, argv: &[&str]) -> Probe {
    Probe {
        name: name.into(),
        source: Source::Cmd(argv.iter().map(|s| (*s).to_string()).collect()),
    }
}

/// The node facts of arm E, under the roots `proc_` and `sys` (`/proc`, `/sys` in a run).
#[must_use]
pub fn node_probes(proc_: &Path, sys: &Path, ssd: Option<&str>) -> Vec<Probe> {
    let mut v = vec![
        cmd("uname -r", &["uname", "-r"]),
        file("kernel cmdline", proc_.join("cmdline")),
        cmd("nproc", &["nproc"]),
        dir("fuse module parameters", sys.join("module/fuse/parameters")),
        dir("fuse sysctl (request timeouts)", proc_.join("sys/fs/fuse")),
        file(
            "kernel.io_uring_disabled",
            proc_.join("sys/kernel/io_uring_disabled"),
        ),
        file(
            "kernel.io_uring_group",
            proc_.join("sys/kernel/io_uring_group"),
        ),
        Probe {
            name: "js-agent limits and seccomp".into(),
            source: Source::Process("js-agent".into()),
        },
        Probe {
            name: "crun limits and seccomp".into(),
            source: Source::Process("crun".into()),
        },
        cmd(
            "findmnt /",
            &["findmnt", "-n", "-o", "SOURCE,FSTYPE,OPTIONS", "/"],
        ),
        cmd(
            "findmnt cache",
            &[
                "findmnt",
                "-n",
                "-o",
                "SOURCE,FSTYPE,OPTIONS",
                "/var/lib/tierstore-cache",
            ],
        ),
        cmd(
            "losetup",
            &["losetup", "-l", "-O", "NAME,BACK-FILE,DIO,LOG-SEC"],
        ),
        cmd(
            "lsblk",
            &[
                "lsblk",
                "-b",
                "-o",
                "NAME,SIZE,TYPE,FSTYPE,MOUNTPOINT,ROTA,PHY-SEC,LOG-SEC,MIN-IO,OPT-IO,DISC-GRAN,DISC-MAX,MODEL",
            ],
        ),
    ];
    for k in [
        "dirty_ratio",
        "dirty_background_ratio",
        "dirty_bytes",
        "dirty_background_bytes",
        "dirty_expire_centisecs",
        "dirty_writeback_centisecs",
    ] {
        v.push(file(&format!("vm.{k}"), proc_.join("sys/vm").join(k)));
    }
    if let Some(d) = ssd {
        v.push(cmd(
            "ssd partitions and free space",
            &[
                "parted",
                "-s",
                "-m",
                &format!("/dev/{d}"),
                "unit",
                "GiB",
                "print",
                "free",
            ],
        ));
    }
    v
}

/// The drive facts of arm C-6 for a drive mounted at `mnt` on device `dev`, with `parts` silo
/// part files to map.
#[must_use]
pub fn drive_probes(sys: &Path, mnt: &Path, dev: &str, parts: &[PathBuf]) -> Vec<Probe> {
    let m = mnt.to_string_lossy();
    let mut v = vec![
        cmd(&format!("xfs_info {m}"), &["xfs_info", &m]),
        cmd(
            &format!("lsblk -t -D {dev}"),
            &["lsblk", "-t", "-D", &format!("/dev/{dev}")],
        ),
        cmd(
            &format!("free space extents {dev}"),
            &["xfs_db", "-r", "-c", "freesp -s", &format!("/dev/{dev}")],
        ),
        dir(
            &format!("queue {dev}"),
            sys.join("block").join(dev).join("queue"),
        ),
        cmd(&format!("df {m}"), &["df", "-B1", &m]),
    ];
    for p in parts {
        let s = p.to_string_lossy();
        v.push(cmd(
            &format!("fiemap {s}"),
            &["xfs_io", "-r", "-c", "fiemap -v", &s],
        ));
    }
    v
}

/// Up to `n` silo part files (`part.1`) of at least `min` bytes under `root`, found
/// breadth-first, looking at no more than `budget` directory entries.
#[must_use]
pub fn sample_parts(root: &Path, n: usize, min: u64, budget: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut seen = 0;
    while let Some(d) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            seen += 1;
            if seen > budget || out.len() >= n {
                return out;
            }
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                queue.push_back(p);
            } else if e.file_name() == "part.1" && e.metadata().is_ok_and(|m| m.len() >= min) {
                out.push(p);
            }
        }
    }
    out
}

fn read_dir_values(p: &Path) -> Result<String, String> {
    let mut lines = Vec::new();
    let rd = std::fs::read_dir(p).map_err(|e| e.to_string())?;
    let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    names.sort();
    for f in names {
        let v =
            std::fs::read_to_string(&f).map_or_else(|e| format!("<{e}>"), |s| s.trim().to_string());
        lines.push(format!(
            "{}={v}",
            f.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
    }
    Ok(lines.join("\n"))
}

fn read_process(proc_: &Path, comm: &str) -> Result<String, String> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(proc_).map_err(|e| e.to_string())?;
    let mut pids: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| std::fs::read_to_string(p.join("comm")).is_ok_and(|c| c.trim() == comm))
        .collect();
    pids.sort();
    for p in pids {
        let limits = std::fs::read_to_string(p.join("limits")).unwrap_or_default();
        let status = std::fs::read_to_string(p.join("status")).unwrap_or_default();
        let pick = |t: &str, key: &str| {
            t.lines().find(|l| l.starts_with(key)).map_or_else(
                || format!("{key} ?"),
                |l| l.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        };
        out.push(format!(
            "pid {}: {}; {}",
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            pick(&limits, "Max locked memory"),
            pick(&status, "Seccomp:")
        ));
    }
    if out.is_empty() {
        return Err(format!("no process named {comm}"));
    }
    Ok(out.join("\n"))
}

/// Reads every probe; a failure is kept as `error: <why>`.
#[must_use]
pub fn collect(probes: &[Probe], proc_: &Path, run: &dyn Runner) -> BTreeMap<String, String> {
    probes
        .iter()
        .map(|p| {
            let v = match &p.source {
                Source::File(f) => std::fs::read_to_string(f)
                    .map(|s| s.trim().to_string())
                    .map_err(|e| e.to_string()),
                Source::Dir(d) => read_dir_values(d),
                Source::Cmd(argv) => {
                    let a: Vec<&str> = argv.iter().map(String::as_str).collect();
                    run.run(&a)
                        .map(|s| s.trim().to_string())
                        .map_err(|e| e.to_string())
                }
                Source::Process(comm) => read_process(proc_, comm),
            };
            (p.name.clone(), v.unwrap_or_else(|e| format!("error: {e}")))
        })
        .collect()
}

/// The facts as Markdown: a heading per fact and its value in a code block.
#[must_use]
pub fn markdown(title: &str, facts: &BTreeMap<String, String>) -> String {
    let mut s = format!("### {title}\n\n");
    for (k, v) in facts {
        s.push_str(&format!("**{k}**\n\n```\n{v}\n```\n\n"));
    }
    s
}

#[cfg(test)]
mod tests;

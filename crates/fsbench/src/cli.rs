//! The command line of `fsbench`.
//!
//! ```text
//! fsbench list --tree <dir> --out <list>
//! fsbench null-fs --list <list> --mountpoint <dir> [--entry-timeout-ms N] [--attr-timeout-ms N]
//!                 [--open explicit|none] [--threads N] [--allow-other]
//! fsbench tsfuse-null-fs --list <list> --mountpoint <dir> --data <file> [--entry-timeout-ms N]
//!                 [--attr-timeout-ms N] [--open explicit|none] [--readers N]
//!                 [--reply writev|splice] [--splice-min BYTES] [--move-pages] [--allow-other]
//!                 [--transport dev|uring] [--uring-depth N] [--uring-memory-mib N]
//! fsbench run --list <list> --tree <dir> --variant <label> --regime <label> --server-pid <pid>
//!             --out <results> [--runs 6] [--readers 256] [--burners 176] [--secs 60]
//!             [--throughput-rounds 1] ...
//! fsbench report --results <results> [--discard 1]
//! fsbench storm --mountpoint <dir> --out <results> --label <label> [--runs 3]
//!               [--transport dev|uring] [--uring-depth N] [--uring-memory-mib N] [--readers 1]
//!               [--park-us 60000] [--read buffered|direct] [--per-cpu N] [--storm-cpu 0]
//!               [--other-cpu N] [--secs 20] [--max-probes 0] [--sample-ms 100]
//!               [--setting KEY=VALUE]...
//! fsbench storm-report --results <results> [--discard 0]
//! fsbench summary --results <results> [--results <results>]... [--discard 1]
//! fsbench dstate-report --index <runs.json>
//! ```
//!
//! `reader` and `burn` are the processes `run` starts; a person does not run them. Exit status:
//! 0 when the command did what it was asked, 1 when it failed (the error is on standard error),
//! 2 for a command line that is not one.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufReader, Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::Error;
use crate::host::Host;
use crate::nullfs::{OpenMode, Options};
use crate::tsnull::Reply;

/// The FUSE benchmark of tierstore fs.
#[derive(Debug, Parser)]
#[command(name = "fsbench")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Walk a tree once and write the list of its regular files (the fixed input of the runs).
    List {
        /// The tree.
        #[arg(long)]
        tree: PathBuf,
        /// Where the list is written.
        #[arg(long)]
        out: PathBuf,
    },
    /// Serve the files of a list, all zeros, until standard input closes (the null file system).
    NullFs {
        /// The list.
        #[arg(long)]
        list: PathBuf,
        /// Where to mount.
        #[arg(long)]
        mountpoint: PathBuf,
        /// How long the kernel may keep an entry, in milliseconds.
        #[arg(long, default_value_t = 0)]
        entry_timeout_ms: u64,
        /// How long the kernel may keep attributes, in milliseconds.
        #[arg(long, default_value_t = 0)]
        attr_timeout_ms: u64,
        /// How `OPEN` is answered.
        #[arg(long, value_enum, default_value_t = OpenMode::Explicit)]
        open: OpenMode,
        /// The threads that read requests (`tierstore fs`'s default is 16).
        #[arg(long, default_value_t = 16)]
        threads: usize,
        /// Let other users use the mount (`allow_other`).
        #[arg(long)]
        allow_other: bool,
    },
    /// Serve the files of a list from a data file on tsfuse, until standard input closes (the
    /// null file system of the redesign's variants).
    TsfuseNullFs(TsfuseArgs),
    /// The runs of one variant in one regime; each run's record is appended to `--out`.
    Run(RunArgs),
    /// The tables of a results file, for a measurement document.
    Report {
        /// The results file.
        #[arg(long)]
        results: PathBuf,
        /// The runs of each series to leave out (the warm-up).
        #[arg(long, default_value_t = 1)]
        discard: u32,
    },
    /// The storm test of the io_uring transport: parked reads from one CPU and from every CPU,
    /// and the latency of `stat` during them; each run's record is appended to `--out`.
    Storm(StormArgs),
    /// The tables of a results file of the storm, for a measurement document.
    StormReport {
        /// The results file.
        #[arg(long)]
        results: PathBuf,
        /// The runs of each label to leave out.
        #[arg(long, default_value_t = 0)]
        discard: u32,
    },
    /// One table of the series of several results files (the result tables of the design).
    Summary {
        /// The results files, in the order of the table (repeat).
        #[arg(long = "results", required = true)]
        results: Vec<PathBuf>,
        /// The runs of each series to leave out (the warm-up).
        #[arg(long, default_value_t = 1)]
        discard: u32,
    },
    /// The tables of a real build's D state and FUSE waiting, from the index of its runs.
    DstateReport {
        /// The index (`runs.json`): the runs and each node's sampler text, relative to it.
        #[arg(long)]
        index: PathBuf,
    },
    /// A reader process of `run` (not run by hand).
    #[command(hide = true)]
    Reader {
        #[arg(long)]
        list: PathBuf,
        #[arg(long)]
        tree: PathBuf,
    },
    /// A CPU burner of `run` (not run by hand): spins until standard input closes.
    #[command(hide = true)]
    Burn,
}

#[derive(Debug, clap::Args)]
struct TsfuseArgs {
    /// The list.
    #[arg(long)]
    list: PathBuf,
    /// Where to mount.
    #[arg(long)]
    mountpoint: PathBuf,
    /// The file READs are answered from: made, or extended with a hole, to the list's length.
    #[arg(long)]
    data: PathBuf,
    /// How long the kernel may keep an entry, in milliseconds.
    #[arg(long, default_value_t = 0)]
    entry_timeout_ms: u64,
    /// How long the kernel may keep attributes, in milliseconds.
    #[arg(long, default_value_t = 0)]
    attr_timeout_ms: u64,
    /// How `OPEN` and `OPENDIR` are answered.
    #[arg(long, value_enum, default_value_t = OpenMode::Explicit)]
    open: OpenMode,
    /// The readers: the mount's descriptor and its clones, a thread each.
    #[arg(long, default_value_t = NonZeroUsize::MIN)]
    readers: NonZeroUsize,
    /// How READ replies are written.
    #[arg(long, value_enum, default_value_t = Reply::Writev)]
    reply: Reply,
    /// The smallest READ reply that is spliced, in bytes (`--reply splice`).
    #[arg(long, default_value_t = tsfuse::SPLICE_MIN)]
    splice_min: usize,
    /// Splice into the device with `SPLICE_F_MOVE` (`--reply splice`).
    #[arg(long)]
    move_pages: bool,
    /// Let other users use the mount (`allow_other`).
    #[arg(long)]
    allow_other: bool,
    #[command(flatten)]
    transport: TransportArgs,
}

/// The transport a `tsfuse` session asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum TransportKind {
    /// `/dev/fuse` readers alone.
    Dev,
    /// io_uring (where the session cannot use it, `/dev/fuse`, and it says why).
    Uring,
}

#[derive(Debug, clap::Args)]
struct TransportArgs {
    /// The transport the session asks for.
    #[arg(long, value_enum, default_value_t = TransportKind::Dev)]
    transport: TransportKind,
    /// The ring entries of each queue (`--transport uring`).
    #[arg(long, default_value_t = tsfuse::UringConfig::DEPTH)]
    uring_depth: std::num::NonZeroU16,
    /// The most memory the ring entries may register, MiB; past it the session serves on
    /// `/dev/fuse` (`--transport uring`).
    #[arg(long, default_value_t = tsfuse::UringConfig::MEMORY_LIMIT >> 20)]
    uring_memory_mib: u64,
}

impl TransportArgs {
    fn transport(&self) -> crate::tsnull::Transport {
        match self.transport {
            TransportKind::Dev => crate::tsnull::Transport::Dev,
            TransportKind::Uring => crate::tsnull::Transport::Uring(tsfuse::UringConfig {
                depth: self.uring_depth,
                memory_limit: self.uring_memory_mib.saturating_mul(1 << 20),
            }),
        }
    }
}

#[derive(Debug, clap::Args)]
struct StormArgs {
    /// Where to mount (an empty directory).
    #[arg(long)]
    mountpoint: PathBuf,
    /// The results file (appended to).
    #[arg(long)]
    out: PathBuf,
    /// The series' label.
    #[arg(long)]
    label: String,
    /// How many runs.
    #[arg(long, default_value_t = 3)]
    runs: u32,
    #[command(flatten)]
    transport: TransportArgs,
    /// The `/dev/fuse` readers: the mount's descriptor and its clones.
    #[arg(long, default_value_t = NonZeroUsize::MIN)]
    readers: NonZeroUsize,
    /// How long a cold READ is parked before it is answered, in microseconds.
    #[arg(long, default_value_t = 60_000)]
    park_us: u64,
    /// How the storm threads read their cold files.
    #[arg(long, value_enum, default_value_t = crate::stormfs::ColdRead::Buffered)]
    read: crate::stormfs::ColdRead,
    /// The storm threads on each stormed CPU (default: `ceil(1.5 x --uring-depth)`).
    #[arg(long)]
    per_cpu: Option<u32>,
    /// The CPU of the one-CPU storm.
    #[arg(long, default_value_t = 0)]
    storm_cpu: usize,
    /// The CPU of the probe the one-CPU storm leaves alone (default: the last online CPU).
    #[arg(long)]
    other_cpu: Option<usize>,
    /// How long each phase's probes run, in seconds.
    #[arg(long, default_value_t = 20)]
    secs: u64,
    /// The most `stat`s of each probe in a phase (0: no limit).
    #[arg(long, default_value_t = 0)]
    max_probes: u64,
    /// The interval of the samples, in milliseconds.
    #[arg(long, default_value_t = 100)]
    sample_ms: u64,
    /// What to record, `KEY=VALUE` (repeat).
    #[arg(long = "setting", value_parser = setting)]
    settings: Vec<(String, String)>,
}

impl StormArgs {
    /// The plan of the series on `host`.
    fn plan(self, host: Host) -> Result<crate::storm::Plan, Error> {
        let cpus = crate::storm::usable_cpus(&host)?;
        let other_cpu = crate::storm::other_cpu(&cpus, self.storm_cpu, self.other_cpu)?;
        Ok(crate::storm::Plan {
            mountpoint: self.mountpoint,
            label: self.label,
            runs: self.runs,
            transport: self.transport.transport(),
            readers: self.readers,
            park: Duration::from_micros(self.park_us),
            read: self.read,
            per_cpu: self
                .per_cpu
                .unwrap_or_else(|| crate::storm::storm_per_cpu(self.transport.uring_depth.get())),
            cpus,
            storm_cpu: self.storm_cpu,
            other_cpu,
            window: Duration::from_secs(self.secs),
            max_probes: self.max_probes,
            sample_every: Duration::from_millis(self.sample_ms),
            settings: self.settings.into_iter().collect(),
            host,
        })
    }
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// The list.
    #[arg(long)]
    list: PathBuf,
    /// The directory the list's paths are below.
    #[arg(long)]
    tree: PathBuf,
    /// The variant's label (`v1`, `v2`, `null`).
    #[arg(long)]
    variant: String,
    /// The regime's label (`caches-on`, `caches-off`, `no-open`).
    #[arg(long)]
    regime: String,
    /// The process that serves the mount: its CPU time is measured.
    #[arg(long)]
    server_pid: u32,
    /// The results file (appended to).
    #[arg(long)]
    out: PathBuf,
    /// How many runs (the report leaves out the first).
    #[arg(long, default_value_t = 6)]
    runs: u32,
    /// The reader processes of the cycles.
    #[arg(long, default_value_t = 256)]
    readers: u32,
    /// The CPU burners beside them.
    #[arg(long, default_value_t = 176)]
    burners: u32,
    /// How long the cycles last, in seconds.
    #[arg(long, default_value_t = 60)]
    secs: u64,
    /// The most cycles per reader (0: no limit).
    #[arg(long, default_value_t = 0)]
    max_cycles: u64,
    /// The bytes a cycle reads from the start of its file.
    #[arg(long, default_value_t = 4096)]
    read_bytes: u32,
    /// The MiB of the largest files that are read whole, cold and warm.
    #[arg(long, default_value_t = 256)]
    throughput_mib: u64,
    /// The readers of the whole-file reads.
    #[arg(long, default_value_t = 16)]
    throughput_readers: u32,
    /// How many times each whole-file phase (cold, then warm) reads its files.
    #[arg(long, default_value_t = 1)]
    throughput_rounds: u32,
    /// The interval of the samples of D state and waiting, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    sample_ms: u64,
    /// The user and group the readers run as, `UID:GID`.
    #[arg(long, value_parser = user)]
    as_user: Option<(u32, u32)>,
    /// What to record of the server, `KEY=VALUE` (repeat): `server=tierstore 0.3.121`, ...
    #[arg(long = "setting", value_parser = setting)]
    settings: Vec<(String, String)>,
}

/// `UID:GID`.
fn user(text: &str) -> Result<(u32, u32), String> {
    let (uid, gid) = text.split_once(':').ok_or("not UID:GID")?;
    Ok((
        uid.parse().map_err(|_| "the UID is not a number")?,
        gid.parse().map_err(|_| "the GID is not a number")?,
    ))
}

/// `KEY=VALUE`.
fn setting(text: &str) -> Result<(String, String), String> {
    let (key, value) = text.split_once('=').ok_or("not KEY=VALUE")?;
    Ok((key.to_owned(), value.to_owned()))
}

/// Opens `path` to append to, making it if it is not there.
fn append(path: &Path) -> Result<std::fs::File, Error> {
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(Error::at("open", path))
}

fn dispatch(
    command: Command,
    exe: &Path,
    stdin: Box<dyn Read + Send>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), Error> {
    let written = Error::at("write", Path::new("standard output"));
    match command {
        Command::List { tree, out } => {
            let list = crate::list::write(&tree, &out)?;
            writeln!(
                stdout,
                "{} files, {} bytes, sha256 {}",
                list.files.len(),
                list.bytes(),
                list.sha256
            )
            .map_err(written)
        }
        Command::NullFs {
            list,
            mountpoint,
            entry_timeout_ms,
            attr_timeout_ms,
            open,
            threads,
            allow_other,
        } => {
            let options = Options {
                entry_timeout: Duration::from_millis(entry_timeout_ms),
                attr_timeout: Duration::from_millis(attr_timeout_ms),
                open,
                threads,
                allow_other,
            };
            crate::nullfs::serve(
                &crate::list::read(&list)?,
                &mountpoint,
                options,
                stdin,
                stdout,
            )
        }
        Command::TsfuseNullFs(args) => {
            let options = crate::tsnull::Options {
                entry_timeout: Duration::from_millis(args.entry_timeout_ms),
                attr_timeout: Duration::from_millis(args.attr_timeout_ms),
                open: args.open,
                readers: args.readers,
                reply: args.reply,
                splice_min: args.splice_min,
                move_pages: args.move_pages,
                allow_other: args.allow_other,
                transport: args.transport.transport(),
            };
            crate::tsnull::serve(
                &crate::list::read(&args.list)?,
                &args.mountpoint,
                &args.data,
                options,
                stdin,
                stdout,
            )
        }
        Command::Run(args) => {
            let plan = crate::run::Plan {
                exe: exe.to_path_buf(),
                list: args.list,
                tree: args.tree,
                variant: args.variant,
                regime: args.regime,
                runs: args.runs,
                readers: args.readers,
                burners: args.burners,
                cycle_ms: args.secs.saturating_mul(1000),
                max_cycles: args.max_cycles,
                read_bytes: args.read_bytes,
                throughput_budget: args.throughput_mib.saturating_mul(1 << 20),
                throughput_readers: args.throughput_readers,
                throughput_rounds: args.throughput_rounds,
                sample_every: Duration::from_millis(args.sample_ms),
                server_pid: args.server_pid,
                as_user: args.as_user,
                settings: args.settings.into_iter().collect::<BTreeMap<_, _>>(),
                host: Host::real(),
            };
            crate::run::run(&plan, append(&args.out)?, stderr).map(drop)
        }
        Command::Report { results, discard } => {
            let records = crate::report::read(&results)?;
            write!(stdout, "{}", crate::report::render(&records, discard)).map_err(written)
        }
        Command::Storm(args) => {
            let out = args.out.clone();
            let plan = args.plan(Host::real())?;
            crate::storm::run(&plan, &mut append(&out)?, stderr).map(drop)
        }
        Command::StormReport { results, discard } => {
            let records = crate::stormrec::read(&results)?;
            write!(stdout, "{}", crate::stormrec::render(&records, discard)).map_err(written)
        }
        Command::Summary { results, discard } => {
            let mut files = Vec::new();
            for path in results {
                let name = path
                    .file_stem()
                    .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                files.push((name, crate::report::read(&path)?));
            }
            write!(stdout, "{}", crate::summary::render(&files, discard)).map_err(written)
        }
        Command::DstateReport { index } => {
            let runs = crate::dstate::load(&index)?;
            write!(stdout, "{}", crate::dstaterep::render(&runs)).map_err(written)
        }
        Command::Reader { list, tree } => crate::reader::serve(
            &crate::list::read(&list)?,
            &tree,
            BufReader::new(stdin),
            stdout,
        ),
        Command::Burn => {
            let _ = crate::burn::spin(stdin);
            Ok(())
        }
    }
}

/// Runs the command line `args` (the program's name first): `exe` is the executable `run`
/// starts its readers and burners from. What the command prints goes to `stdout`, the errors
/// and the lines about the runs to `stderr`.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    exe: &Path,
    stdin: Box<dyn Read + Send>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let text = e.render().to_string();
            let _ = if e.use_stderr() {
                write!(stderr, "{text}")
            } else {
                write!(stdout, "{text}")
            };
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    match dispatch(cli.command, exe, stdin, stdout, stderr) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(stderr, "fsbench: {e}");
            ExitCode::from(1)
        }
    }
}

/// The program: [`run`] on the process's own arguments and streams.
#[must_use]
pub fn main() -> ExitCode {
    // Without it, `run` fails at the start of its first reader, and says so.
    let exe = std::env::current_exe().unwrap_or_default();
    run(
        std::env::args_os(),
        &exe,
        Box::new(std::io::stdin()),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;

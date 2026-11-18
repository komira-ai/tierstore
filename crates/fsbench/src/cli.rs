//! The command line of `fsbench`.
//!
//! ```text
//! fsbench list --tree <dir> --out <list>
//! fsbench null-fs --list <list> --mountpoint <dir> [--entry-timeout-ms N] [--attr-timeout-ms N]
//!                 [--open explicit|none] [--threads N] [--allow-other]
//! fsbench run --list <list> --tree <dir> --variant <label> --regime <label> --server-pid <pid>
//!             --out <results> [--runs 6] [--readers 256] [--burners 176] [--secs 60] ...
//! fsbench report --results <results> [--discard 1]
//! ```
//!
//! `reader` and `burn` are the processes `run` starts; a person does not run them. Exit status:
//! 0 when the command did what it was asked, 1 when it failed (the error is on standard error),
//! 2 for a command line that is not one.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::Error;
use crate::host::Host;
use crate::nullfs::{OpenMode, Options};

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

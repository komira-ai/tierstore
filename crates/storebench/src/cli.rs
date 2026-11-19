//! The command line of `storebench`.
//!
//! ```text
//! storebench list --endpoint <url> --credentials <file> --bucket <b>... --out <listing>
//! storebench drive-list --bucket-dir <dir> [--share i/n] --out <listing> [--meta-out <file>]
//! storebench footers --endpoint <url> --credentials <file> --listing <listing> --out <footers>
//!     [--conc 16] [--share i/n] [--min-pack-bytes 0] [--tree-every 1]
//! storebench du --root <drive> --out <du.json> [--depth 2]
//! storebench c6 --listing <listing> [--footers <f>]... [--du <du.json>]... [--erasure <f>]...
//!     [--drives 12] [--data-shards 8] [--parity-shards 4] [--dead-out <file>] --out <c6.json>
//! storebench c7 --listing <listing> [--shared-bytes-per-drive 0] --out <c7.json>
//! storebench c10 --listing <listing> [--dead-keys <file>] --out <c10.json>
//! storebench report --data <dir> --arm c6|c7|c10
//! ```
//!
//! Exit status: 0 when the command did what it was asked, 1 when it failed (the error is on
//! standard error), 2 for a command line that is not one.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::Error;
use crate::s3::Endpoint;

/// The storage-tier measurements that need no tier code.
#[derive(Debug, Parser)]
#[command(name = "storebench")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write the listing of silo's buckets (every object: bucket, key, size, time, tag).
    List {
        #[command(flatten)]
        endpoint: Endpoint,
        /// A bucket to list; repeat for several.
        #[arg(long, required = true)]
        bucket: Vec<String>,
        /// Where the listing is written.
        #[arg(long)]
        out: PathBuf,
    },
    /// Write the listing of one bucket from one of silo's drives (read-only), or one share of it.
    DriveList(crate::drivewalk::Args),
    /// Read the footer of every pack and the first byte of every tree list of a listing.
    Footers(crate::footers::Args),
    /// Count the files and the allocated bytes under each directory of a drive.
    Du(crate::du::Args),
    /// C-6: silo's objects by kind and size, L_silo, L_live and the raw-to-logical gap.
    C6(crate::c6::Args),
    /// C-7: the key set under the storage nodes' rank, and stripes with nodes down.
    C7(crate::placement::Args),
    /// C-10: garbage collection replayed against the tier's sealing and stripe rules.
    C10(crate::gc::Args),
    /// Print the tables of one arm from the summaries committed in `data/`.
    Report {
        /// The directory of the summaries.
        #[arg(long)]
        data: PathBuf,
        /// The arm.
        #[arg(long, value_enum)]
        arm: crate::report::Arm,
    },
}

/// A runtime for the commands that talk to silo.
fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Builder::new_multi_thread()
        // The work is requests to silo: a few threads keep them in flight, and a farm node has
        // 88 CPUs, which is not the number of threads this needs.
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(|e| Error::io("start", "the async runtime", e))
}

fn dispatch(command: Command, stdout: &mut dyn Write) -> Result<(), Error> {
    match command {
        Command::List {
            endpoint,
            bucket,
            out,
        } => {
            let credentials = crate::s3::credentials(&endpoint.credentials)?;
            let rt = runtime()?;
            let mut file = crate::listing::create(&out)?;
            let mut total = 0;
            for b in &bucket {
                let store = crate::s3::store(
                    &endpoint.endpoint,
                    b,
                    credentials.clone(),
                    std::time::Duration::from_secs(endpoint.request_timeout_secs),
                )?;
                total += rt.block_on(crate::listing::list(&store, b, &mut file, &out))?;
            }
            file.flush().map_err(|e| Error::io("write", &out, e))?;
            writeln!(stdout, "{total} objects in {}", out.display())
                .map_err(|e| Error::io("write", "standard output", e))
        }
        Command::DriveList(args) => crate::drivewalk::run(&args, stdout),
        Command::Footers(args) => crate::footers::run(&args, stdout),
        Command::Du(args) => crate::du::run(&args, stdout),
        Command::C6(args) => crate::c6::run(&args, stdout),
        Command::C7(args) => crate::placement::run(&args, stdout),
        Command::C10(args) => crate::gc::run(&args, stdout),
        Command::Report { data, arm } => {
            let text = crate::report::render(&data, arm)?;
            write!(stdout, "{text}").map_err(|e| Error::io("write", "standard output", e))
        }
    }
}

/// Runs the command line `args` (the program's name first). What the command prints goes to
/// `stdout`, the errors to `stderr`.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
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
    match dispatch(cli.command, stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(stderr, "storebench: {e}");
            ExitCode::from(1)
        }
    }
}

/// The program: [`run`] on the process's own arguments and streams.
#[must_use]
pub fn main() -> ExitCode {
    run(
        std::env::args_os(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;

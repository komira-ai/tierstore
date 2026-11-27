//! The command line of `tierstore-example`.
//!
//! ```text
//! tierstore-example run <example> --tierstore <binary> --work <dir> [--tmp <dir>]
//!                       [--tierstore-config <file>]
//! tierstore-example check-page <example>
//! ```
//!
//! `run` prints the report on standard output and exits 0 when it is the example's
//! `expected.txt` and no expected failure passes; otherwise it prints what differs on standard
//! error and exits 1. `check-page` exits 0 when the page shows the example's files as they are,
//! and 1 with the problems on standard error. A run or check that cannot finish exits 1 with its
//! error; a usage error exits 2.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::page;
use crate::run::{Options, run};

/// Runs an example of `examples/` against tierstore, or checks its page.
#[derive(Debug, Parser)]
#[command(name = "tierstore-example")]
struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Debug, Subcommand)]
enum Verb {
    /// Runs an example and compares its report with its expected.txt.
    Run {
        /// The example's directory.
        example: PathBuf,
        /// The tierstore binary to start.
        #[arg(long)]
        tierstore: PathBuf,
        /// The work directory: made if it is not there; it must be empty.
        #[arg(long)]
        work: PathBuf,
        /// The TMPDIR of the commands (default: tmp in the work directory); a short path where a
        /// client binds a Unix socket there.
        #[arg(long)]
        tmp: Option<PathBuf>,
        /// A configuration to run tierstore with in place of the example's tierstore.toml.
        #[arg(long)]
        tierstore_config: Option<PathBuf>,
    },
    /// Checks that the example's page shows each of its files as it is.
    CheckPage {
        /// The example's directory.
        example: PathBuf,
    },
}

/// Runs the command line `args` (the program name first), writing to `out` and `err`, and gives
/// the exit status.
pub fn main(
    args: impl IntoIterator<Item = OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(usage) => {
            let text = usage.render().to_string();
            let _ = if usage.use_stderr() {
                err.write_all(text.as_bytes())
            } else {
                out.write_all(text.as_bytes())
            };
            return u8::try_from(usage.exit_code()).unwrap_or(2);
        }
    };
    let result = match cli.verb {
        Verb::Run {
            example,
            tierstore,
            work,
            tmp,
            tierstore_config,
        } => {
            let config = tierstore_config;
            run(&Options {
                example,
                tierstore,
                work,
                tmp,
                config,
            })
            .map(|report| {
                if report.passed() {
                    let _ = write!(out, "{}", report.actual);
                    0
                } else {
                    let _ = write!(err, "{}", report.describe());
                    1
                }
            })
        }
        Verb::CheckPage { example } => page::check(&example).map(|()| 0),
    };
    result.unwrap_or_else(|error| {
        let _ = writeln!(err, "tierstore-example: {error}");
        1
    })
}

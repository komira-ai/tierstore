//! The command line of `ts-measure` on a system other than Linux: every arm reads Linux block
//! devices, /proc and /sys, so the binary says so and exits 2.

use std::io::Write as _;
use std::process::ExitCode;

/// The line `ts-measure` writes to stderr on a system other than Linux.
pub const UNSUPPORTED: &str = "ts-measure: Linux only (it reads block devices, /proc and /sys)";

/// Writes [`UNSUPPORTED`] to stderr and exits 2.
#[must_use]
pub fn main() -> ExitCode {
    let _ = writeln!(std::io::stderr(), "{UNSUPPORTED}");
    ExitCode::from(2)
}

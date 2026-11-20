//! The command line on a system other than Linux: the harness fills the Linux file entry store
//! and reads /proc/diskstats, so it says it is Linux only and exits 2.

use std::io::Write as _;

/// Writes why the harness does not run here and returns exit code 2.
pub(crate) fn main() -> std::process::ExitCode {
    let _ = writeln!(
        std::io::stderr(),
        "readbench: Linux only (the file entry store it fills is, and it reads /proc/diskstats)"
    );
    std::process::ExitCode::from(2)
}

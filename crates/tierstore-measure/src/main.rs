//! `ts-measure`: forwards to the library (`tierstore_measure::cli`).

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    tierstore_measure::cli::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    use std::io::Write as _;
    let _ = writeln!(
        std::io::stderr(),
        "ts-measure: Linux only (it reads block devices, /proc and /sys)"
    );
    std::process::ExitCode::from(2)
}

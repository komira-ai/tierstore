//! `readbench`: forwards to the library's command line (`readbench::cli`).

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    readbench::cli::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    use std::io::Write as _;
    let _ = writeln!(
        std::io::stderr(),
        "readbench: Linux only (the file entry store it fills is, and it reads /proc/diskstats)"
    );
    std::process::ExitCode::from(2)
}

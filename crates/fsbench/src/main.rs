//! `fsbench`: forwards to the library's command line (`fsbench::cli`).

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    fsbench::cli::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    use std::io::Write as _;
    let _ = writeln!(
        std::io::stderr(),
        "fsbench: Linux only (it reads /proc and /sys and mounts FUSE)"
    );
    std::process::ExitCode::from(2)
}

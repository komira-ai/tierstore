//! `tierstore-blackbox`: forwards to the library's driver (`tierstore_blackbox::driver_main`).

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    tierstore_blackbox::driver_main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    use std::io::Write as _;
    let _ = writeln!(
        std::io::stderr(),
        "tierstore-blackbox: Linux only (the mount is FUSE, the memory is read from /proc)"
    );
    std::process::ExitCode::from(2)
}

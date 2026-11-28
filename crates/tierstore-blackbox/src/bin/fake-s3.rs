//! `fake-s3`: forwards to the library's fake upstream (`tierstore_blackbox::fake_main`).

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    tierstore_blackbox::fake_main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    use std::io::Write as _;
    let _ = writeln!(
        std::io::stderr(),
        "fake-s3: Linux only, as the rig it serves"
    );
    std::process::ExitCode::from(2)
}

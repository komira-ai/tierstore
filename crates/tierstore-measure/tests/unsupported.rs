//! On a system other than Linux, `ts-measure` measures nothing: it writes one line saying it is
//! Linux only to stderr and exits 2, whatever its arguments.

#![cfg(not(target_os = "linux"))]

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ts-measure");

#[test]
fn off_linux_ts_measure_says_it_is_linux_only_and_exits_2() {
    let out = Command::new(BIN).output().expect("ts-measure runs");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "ts-measure: Linux only (it reads block devices, /proc and /sys)\n"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn off_linux_an_arm_is_refused_the_same_way() {
    let out = Command::new(BIN)
        .args(["hdd", "--help"])
        .output()
        .expect("ts-measure runs");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("{}\n", tierstore_measure::cli::UNSUPPORTED)
    );
    assert!(out.stdout.is_empty());
}

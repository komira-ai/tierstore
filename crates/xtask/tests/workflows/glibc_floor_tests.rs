//! The glibc floor of the Linux builds in `artifacts.yml` ([`super::glibc_floor`]): the file passes,
//! and each way of building the binary against the runner's glibc, of not checking its symbol
//! versions, or of not pinning the toolchain is named.

use super::platforms_tests::{artifacts_problems_of, each_named};
use super::text_of;

const X86: &str = "linux-x86_64";
const ARM: &str = "linux-arm64";

/// Changes to the Linux build jobs that leave a valid workflow whose binary is linked against the
/// runner's glibc, or checked against nothing, or whose toolchain is not the pinned one.
const LINUX_GLIBC_DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        X86,
        "      - run: PATH=\"$HOME/zig:$PATH\" cargo zigbuild --locked --release --target \"$TARGET.$GLIBC_FLOOR\" -p tierstore",
        "      - run: cargo build --locked --release --target \"$TARGET\" -p tierstore",
        "linux-x86_64: does not run `PATH=\"$HOME/zig:$PATH\" cargo zigbuild --locked",
    ),
    (
        ARM,
        "--target \"$TARGET.$GLIBC_FLOOR\" -p tierstore",
        "--target \"$TARGET\" -p tierstore",
        "linux-arm64: does not run `PATH=\"$HOME/zig:$PATH\" cargo zigbuild --locked",
    ),
    (
        X86,
        "      - name: tierstore needs no glibc newer than the floor\n        run: cargo xtask release check-glibc \"target/$TARGET/release/tierstore\" --max \"$GLIBC_FLOOR\"\n",
        "",
        "linux-x86_64: does not run `cargo xtask release check-glibc",
    ),
    (
        ARM,
        "check-glibc \"target/$TARGET/release/tierstore\" --max \"$GLIBC_FLOOR\"",
        "check-glibc \"target/$TARGET/release/tierstore\" --max 2.39",
        "linux-arm64: does not run `cargo xtask release check-glibc",
    ),
    (
        X86,
        "--sha256 \"$ZIG_SHA256_X86_64_LINUX\"",
        "--sha256 \"$ZIG_SHA256_AARCH64_LINUX\"",
        "linux-x86_64: does not run `cargo xtask fetch --url \"https://ziglang.org",
    ),
    (
        ARM,
        "      - name: Install zig (pinned version and checksum)\n",
        "      - name: Install zig (pinned version and checksum)\n        env:\n          ZIG_VERSION: 0.1.0\n",
        "the step `Install zig (pinned version and checksum)`: sets ZIG_VERSION",
    ),
    (
        X86,
        "      TARGET: x86_64-unknown-linux-gnu\n",
        "      TARGET: x86_64-unknown-linux-gnu\n      GLIBC_FLOOR: \"2.39\"\n",
        "linux-x86_64: sets GLIBC_FLOOR",
    ),
];

/// Changes to the file's `env` that leave the Linux builds linked against another glibc than the
/// packages require, or toolchains that are not pinned.
const FILE_ENV_DEFECTS: &[(&str, &str, &str)] = &[
    (
        "  GLIBC_FLOOR: \"2.28\"\n",
        "  GLIBC_FLOOR: \"2.39\"\n",
        "linux-x86_64: GLIBC_FLOOR in the file's env is Some(\"2.39\"), not 2.28",
    ),
    (
        "  ZIGBUILD_SHA256_AARCH64_LINUX: 70bb4c9cc71a6d16211b9079996e3cd588671053312adeff4d043499ed887601\n",
        "  ZIGBUILD_SHA256_AARCH64_LINUX: 70bb\n",
        "linux-arm64: the zig pin ZIGBUILD_SHA256_AARCH64_LINUX is not 64 lowercase hex digits",
    ),
    (
        "  ZIG_SHA256_X86_64_LINUX: 02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239\n",
        "",
        "linux-x86_64: the zig pin ZIG_SHA256_X86_64_LINUX is not 64 lowercase hex digits",
    ),
    (
        "  ZIG_VERSION: 0.15.2\n",
        "",
        "linux-x86_64: ZIG_VERSION in the file's env is None, not a version",
    ),
];

#[test]
fn each_wrong_input_of_the_glibc_floor_build_of_the_linux_jobs_is_named() {
    // Catches: a Linux build that goes back to `cargo build` (linked against the runner's
    // glibc 2.39, the failure of the first release's install checks), skips or loosens the
    // check of the binary's glibc symbol versions, takes the other architecture's zig or a
    // pin that a job shadows, or whose floor, versions or checksums in the file's `env` are
    // not the ones the packages require and the downloads are checked against.
    each_named("artifacts.yml", artifacts_problems_of, LINUX_GLIBC_DEFECTS);
    let text = text_of("artifacts.yml");
    for (replace, with, says) in FILE_ENV_DEFECTS {
        assert!(text.contains(replace), "{replace:?} moved");
        let problems = artifacts_problems_of(&text.replace(replace, with));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

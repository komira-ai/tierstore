//! The glibc floor of the Linux builds in `artifacts.yml`: the binaries of linux-x86_64 and
//! linux-arm64 are linked by `cargo zigbuild` against glibc 2.28 (`xtask::release::glibc::FLOOR`)
//! so they start on every distribution from Debian 10 and RHEL 8 on, as the .deb
//! (`libc6 (>= 2.28)`) and the .rpm (`glibc >= 2.28`) require. [`super::platforms`] pins the
//! commands of a build job whole; the Linux ones come from [`splice_zig_commands`]:
//!
//! - zig and cargo-zigbuild are fetched by URL with a SHA-256 of the file's `env`
//!   ([`pin_problems`] refuses a pin that is not 64 lowercase hex digits, a missing version, and
//!   a `GLIBC_FLOOR` other than the floor the packages require), zig as a whole directory;
//! - the binary is linked by `cargo zigbuild` for `<target>.<GLIBC_FLOOR>`;
//! - `release check-glibc` then reads the symbol versions of the binary against the same floor;
//! - no job or step sets a variable of the pins of its own ([`shadows`]), which would change a
//!   command without changing its text.

use super::platforms::{BUILD, Build, at, is_sha256};
use super::yaml::Node;
use xtask::release::glibc;

/// The zig toolchain of a Linux build job: the architecture in the name of zig's release file
/// (`zig-<arch>-linux-<version>.tar.xz`), and the variables of the file's `env` that pin the
/// SHA-256 of that file and of cargo-zigbuild's.
pub struct Zig {
    pub arch: &'static str,
    pub zig_pin: &'static str,
    pub zigbuild_pin: &'static str,
}

/// Whether the `env` key `key` is one of the pins of this file, which no job or step may set.
pub fn shadows(key: &str) -> bool {
    ["CYCLONEDX_", "ZIG_", "ZIGBUILD_", "GLIBC_"]
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

/// Replaces the plain build in `commands` of a job that links with zig by the fetch of zig and
/// cargo-zigbuild, the linking and the check of the binary's glibc symbol versions.
pub fn splice_zig_commands(b: &Build, commands: &mut Vec<String>) {
    let Some(zig) = &b.zig else {
        return;
    };
    let (arch, zig_pin, zigbuild_pin) = (zig.arch, zig.zig_pin, zig.zigbuild_pin);
    let at = commands.iter().position(|c| c == BUILD).expect("the build");
    commands.splice(
        at..=at,
        [
            format!(
                "cargo xtask fetch --url \"https://ziglang.org/download/$ZIG_VERSION/zig-{arch}-linux-$ZIG_VERSION.tar.xz\" --sha256 \"${zig_pin}\" --out \"$HOME/zig\" --extract tar.xz --tree"
            ),
            format!(
                "cargo xtask fetch --url \"https://github.com/rust-cross/cargo-zigbuild/releases/download/v$ZIGBUILD_VERSION/cargo-zigbuild-$TARGET.tar.xz\" --sha256 \"${zigbuild_pin}\" --out \"$HOME/.cargo/bin/cargo-zigbuild\" --extract tar.xz --member \"cargo-zigbuild-$TARGET/cargo-zigbuild\""
            ),
            "PATH=\"$HOME/zig:$PATH\" cargo zigbuild --version".to_owned(),
            "PATH=\"$HOME/zig:$PATH\" cargo zigbuild --locked --release --target \"$TARGET.$GLIBC_FLOOR\" -p tierstore --features server --bin tierstore".to_owned(),
            "cargo xtask release check-glibc \"target/$TARGET/release/tierstore\" --max \"$GLIBC_FLOOR\"".to_owned(),
        ],
    );
}

/// One line per pin or version of the file's `env` (`doc`) that the Linux build `b` reads and
/// that is missing, not a SHA-256, or (the floor) not the one the packages require.
pub fn pin_problems(doc: &Node, b: &Build) -> Vec<String> {
    let Some(zig) = &b.zig else {
        return Vec::new();
    };
    let platform = b.platform;
    let mut problems = Vec::new();
    for pin in [zig.zig_pin, zig.zigbuild_pin] {
        if !at(doc, &["env", pin]).is_some_and(is_sha256) {
            problems.push(format!(
                "{platform}: the zig pin {pin} is not 64 lowercase hex digits in the file's env"
            ));
        }
    }
    for (key, want) in [
        ("ZIG_VERSION", None),
        ("ZIGBUILD_VERSION", None),
        ("GLIBC_FLOOR", Some(glibc::FLOOR)),
    ] {
        let got = at(doc, &["env", key]);
        if got.is_none() || want.is_some_and(|w| got != Some(w)) {
            problems.push(format!(
                "{platform}: {key} in the file's env is {got:?}, not {}",
                want.unwrap_or("a version")
            ));
        }
    }
    problems
}

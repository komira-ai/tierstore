//! The glibc floor of the Linux builds in `artifacts.yml`: the binaries of linux-x86_64 and
//! linux-arm64 are linked by `cargo zigbuild` against glibc 2.28 (`xtask::release::glibc::FLOOR`)
//! so they start on every distribution from Debian 10 and RHEL 8 on, as the .deb
//! (`libc6 (>= 2.28)`) and the .rpm (`glibc >= 2.28`) require. [`super::platforms`] pins the
//! commands of a build job whole; the Linux ones come from [`splice_zig_commands`]:
//!
//! - zig and cargo-zigbuild are fetched by URL with a SHA-256 of the file's `env`
//!   ([`pin_problems`] refuses a pin that is not 64 lowercase hex digits, a missing version, and
//!   a `GLIBC_FLOOR` other than the floor the packages require), zig as a whole directory,
//!   its tarball kept in the actions cache under a key of its architecture, version and pin
//!   ([`cache_problems`] refuses a restore or save of another key or path, or a save that is not
//!   main's miss);
//! - the binary is linked by `cargo zigbuild` for `<target>.<GLIBC_FLOOR>`;
//! - `release check-glibc` then reads the symbol versions of the binary against the same floor;
//! - no job or step sets a variable of the pins of its own ([`shadows`]), which would change a
//!   command without changing its text.

use super::platforms::{BUILD, Build, at, is_sha256, using};
use super::yaml::{self, Node};
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
                "cargo xtask fetch --cache \"$RUNNER_TEMP/zig-cache/zig-{arch}-linux-$ZIG_VERSION.tar.xz\" --url \"https://pkg.earth/zig/zig-{arch}-linux-$ZIG_VERSION.tar.xz\" --url \"https://zig.linus.dev/zig/zig-{arch}-linux-$ZIG_VERSION.tar.xz\" --url \"https://ziglang.org/download/$ZIG_VERSION/zig-{arch}-linux-$ZIG_VERSION.tar.xz\" --sha256 \"${zig_pin}\" --out \"$HOME/zig\" --extract tar.xz --tree"
            ),
            format!(
                "cargo xtask fetch --url \"https://github.com/rust-cross/cargo-zigbuild/releases/download/v$ZIGBUILD_VERSION/cargo-zigbuild-$TARGET.tar.xz\" --sha256 \"${zigbuild_pin}\" --out \"$HOME/.cargo/bin/cargo-zigbuild\" --extract tar.xz --member \"cargo-zigbuild-$TARGET/cargo-zigbuild\""
            ),
            "PATH=\"$HOME/zig:$PATH\" zig version".to_owned(),
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

/// The directory the zig tarball is restored into and saved from.
const ZIG_CACHE_PATH: &str = "${{ runner.temp }}/zig-cache";

/// One line per way the Linux build job `build` of `b` does not keep its zig tarball in the
/// actions cache as `cargo xtask fetch --cache` reads it: one restore, of the key that names the
/// architecture, `ZIG_VERSION` and the job's zig pin (a key without the pin would restore a
/// tarball the fetch then refuses, for good), into [`ZIG_CACHE_PATH`]; and one save of that
/// path under the restore's key, on main and only when the restore missed.
pub fn cache_problems(b: &Build, build: &yaml::Node) -> Vec<String> {
    let Some(zig) = &b.zig else {
        return Vec::new();
    };
    let platform = b.platform;
    let key = format!(
        "zig-{}-linux-${{{{ env.ZIG_VERSION }}}}-${{{{ env.{} }}}}",
        zig.arch, zig.zig_pin
    );
    let mut problems = Vec::new();
    let restores: Vec<_> = using(Some(build), "actions/cache/restore@")
        .into_iter()
        .filter(|s| at(s, &["id"]) == Some("zig-cache"))
        .collect();
    let restored = restores
        .iter()
        .map(|s| (at(s, &["with", "key"]), at(s, &["with", "path"])));
    if restored.collect::<Vec<_>>() != [(Some(key.as_str()), Some(ZIG_CACHE_PATH))] {
        problems.push(format!(
            "{platform}: does not restore the zig tarball once, as `zig-cache`, under {key:?} into {ZIG_CACHE_PATH:?}"
        ));
    }
    let saved: Vec<_> = using(Some(build), "actions/cache/save@")
        .into_iter()
        .filter(|s| at(s, &["with", "path"]) == Some(ZIG_CACHE_PATH))
        .map(|s| (at(s, &["with", "key"]), at(s, &["if"])))
        .collect();
    let want = (
        Some("${{ steps.zig-cache.outputs.cache-primary-key }}"),
        Some(
            "${{ github.ref == 'refs/heads/main' && steps.zig-cache.outputs.cache-hit != 'true' }}",
        ),
    );
    if saved != [want] {
        problems.push(format!(
            "{platform}: does not save the zig tarball once, under the restore's key, on main when it missed"
        ));
    }
    problems
}

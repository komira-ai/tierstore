//! `cargo xtask mac-surface --changed <file> [--output <file>]`: whether a pull request changes
//! the macOS surface of this repository, the paths whose change runs the macOS checks of
//! `.github/workflows/macos.yml` on the pull request.
//!
//! `<file>` is the list of the paths the pull request changes, as `git diff --name-only -z
//! --no-renames` writes it: each path ended by a NUL byte (a rename is its old path and its new
//! one). A path is on the surface when it starts with one of [`SURFACE`]; the paths are compared
//! as bytes, so a path that is not UTF-8 is judged too. The verb prints each path on the surface
//! and then `mac=true` or `mac=false`, and appends that last line to `--output` (on Actions,
//! `GITHUB_OUTPUT`). A list with no path (a pull request that changes no file) is `mac=false`.
//!
//! The surface is the code that behaves differently on macOS, or that builds, packages or checks
//! the macOS binary: the launchd and systemd arms of the update hook and its step runner, the
//! listener hand-off and the socket binds and locks with a macOS path, the packaging (the launchd
//! plist among it), the xtask verbs the macOS build job runs and the release code that checks
//! their files, the workflows that build or check the macOS binary and their lint, and the
//! toolchain and the root manifest (which pins the hostkeeper crates the package is unpacked
//! with), and the manifests of the binary's crates. The workflow lint
//! (`tests/workflows/macos_pr_tests.rs`) refuses a tracked file that names macOS, launchd,
//! `darwin` or a `target_os` and is not on the surface, unless it is listed there as exempt with
//! its reason. Code that is POSIX alone (`cfg(unix)`, libc calls) and names none of those is not
//! on the surface by that test; a change to it is checked on macOS after the merge.

use std::path::Path;

use crate::Error;

/// The prefixes of the paths on the macOS surface: a changed path that starts with one of them
/// runs the macOS checks. An entry that ends in `/` is a directory; one that does not is a file,
/// or the start of the names of a family of files (`packaging` takes `packaging.rs`,
/// `packaging/` and `packaging_macos.rs`).
pub const SURFACE: &[&str] = &[
    // The workflows that build or check the macOS binary, and their lint.
    ".github/workflows/artifacts.yml",
    ".github/workflows/macos.yml",
    "crates/xtask/tests/workflows/",
    // The toolchain, and the root manifest: it pins the hostkeeper crates the package is unpacked
    // and checked with.
    "Cargo.toml",
    "rust-toolchain.toml",
    // The manifests of the binary's crates: their features choose the platform code built.
    "crates/tierstore/Cargo.toml",
    "crates/objstore-s3/Cargo.toml",
    // The packaging: the launchd plist, the systemd units, the hostkeeper manifest.
    "packaging/",
    "crates/tierstore/tests/packaging",
    // The update hook: its launchd and systemd arms and the step runner they share.
    "crates/tierstore/src/server/update_hook/",
    "crates/tierstore/tests/stored_formats/hook_records.rs",
    "crates/compat/tests/hook_record.rs",
    // The listener hand-off between two processes, and the binds and locks with a macOS path.
    "crates/tierstore/src/server/handoff/",
    "crates/tierstore/src/server/config_handoff",
    "crates/tierstore/src/server/cache_handoff_",
    "crates/tierstore/src/server/door",
    "crates/tierstore/src/server/listen.rs",
    "crates/tierstore/src/server/lock.rs",
    "crates/tierstore/src/server/mod.rs",
    "crates/tierstore/src/server/verbs/mod.rs",
    "crates/tierstore/src/server/testing.rs",
    "crates/tierstore/src/server/metrics_tests.rs",
    "crates/tierstore/src/server/oci/listener_tests.rs",
    "crates/tierstore/tests/s3_front/handoff_",
    // Keys on a case-insensitive file system (the default APFS volume).
    "crates/tierstore/src/dir.rs",
    // Loopback behaviour that differs on macOS, and the test helper's libc calls.
    "crates/objstore-s3/src/s3/read_limit_tests.rs",
    "crates/objstore-s3/src/s3/tests.rs",
    "crates/tierstore-testkit/",
    // The xtask verbs the macOS build job runs, and the release code that checks its files.
    "crates/xtask/Cargo.toml",
    "crates/xtask/src/lib.rs",
    "crates/xtask/src/build_commit",
    "crates/xtask/src/fetch",
    "crates/xtask/src/licenses",
    "crates/xtask/src/mac_surface",
    "crates/xtask/src/package",
    "crates/xtask/src/release",
    "crates/xtask/src/sums",
    "crates/xtask/tests/attestations.rs",
    "crates/xtask/tests/cli",
    "crates/xtask/tests/hostkeeper_package/",
];

/// Whether the path `path` is on the macOS surface.
pub fn on_surface(path: &[u8]) -> bool {
    SURFACE.iter().any(|p| path.starts_with(p.as_bytes()))
}

/// The paths of the NUL-ended list `changed` that are on the macOS surface, in the list's
/// order. An empty entry (two NULs in a row, or no NUL after the last path) is not a path.
pub fn touched(changed: &[u8]) -> Vec<&[u8]> {
    changed
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty() && on_surface(p))
        .collect()
}

/// The verb: prints each changed path on the surface and the answer, `mac=true` or `mac=false`,
/// and appends the answer to `output`.
pub fn mac_surface(changed: &Path, output: Option<&Path>) -> Result<Vec<String>, Error> {
    let list =
        std::fs::read(changed).map_err(Error::spawn(format!("read {}", changed.display())))?;
    let found = touched(&list);
    let answer = format!("mac={}", !found.is_empty());
    if let Some(output) = output {
        crate::release::append_output(output, std::slice::from_ref(&answer))?;
    }
    let mut lines: Vec<String> = found
        .iter()
        .map(|p| format!("on the macOS surface: {}", String::from_utf8_lossy(p)))
        .collect();
    lines.push(answer);
    Ok(lines)
}

#[cfg(test)]
mod tests;

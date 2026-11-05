//! `release hostkeeper-package --binary <path> --version <X.Y.Z+commit> --min-hostkeeper <x.y.z>
//! --platform <platform> --out <dir>`: the hostkeeper package of one platform's binary,
//! `<out>/tierstore-<version>-<platform>.tar.zst`, whose line it appends to
//! `<out>/SHA256SUMS.<platform>` ([`crate::sums`]), the file `package` writes the tarball's and
//! the SBOM's lines to. The package may not exist already, and the sums file may not list it yet.
//!
//! The package is the format of hostkeeper's `docs/package-format.md`, which hostkeeper's
//! unpacker and manifest loader read:
//!
//! - `manifest.toml`: `packaging/hostkeeper/manifest.toml.in` with its placeholders filled in
//!   ([`manifest`]): the version, the platform, the oldest hostkeeper that runs the package, and
//!   the platform's `[units] run` entries and `[params]` ([`Target`]);
//! - `bin/tierstore`: the binary, the exact bytes given (on macOS, the binary the linker signed;
//!   nothing here signs it again);
//! - `units/`: `packaging/hostkeeper/units/tierstore@.service` on Linux, the template of the two
//!   instances the update hook alternates between; `packaging/hostkeeper/units/tierstore.plist`
//!   on macOS, the one launchd job.
//!
//! The archive is the same bytes for the same inputs on every runner ([`archive`]): a ustar
//! archive with a directory entry before its contents and the entries sorted by name, regular
//! files and directories only, mode 0755 for the binary and the directories and 0644 for every
//! other file, owner and group 0 with no names, time 0; in exactly one zstd frame with a content
//! checksum, no dictionary, and a window far below the 8 MiB hostkeeper accepts. Nothing of the
//! input file but its bytes is used: not its name, mode, owner or time.
//!
//! `--version` is refused unless it is `<major>.<minor>.<patch>+<12 lowercase hex digits>` and
//! the binary holds that text, which is what `tierstore --version` prints after the program's name
//! and what the build gauge names: the version hostkeeper installs is then the version the update
//! hook finds the new process reporting. The binary is not run, so a binary of any platform is
//! packaged on any runner. `--min-hostkeeper` is refused unless it is a plain `x.y.z`.

use std::io::Write;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::Error;
use crate::fetch;
use crate::package::{Platform, open_sums, read, write_new, write_sums};
use crate::sums;

/// The service the package is of: its name in the manifest and in the archive's name.
pub const SERVICE: &str = "tierstore";

/// The manifest, before [`manifest`] fills in its placeholders.
pub const MANIFEST_TEMPLATE: &str =
    include_str!("../../../../packaging/hostkeeper/manifest.toml.in");

/// The placeholders of [`MANIFEST_TEMPLATE`], each there once.
pub const PLACEHOLDERS: [&str; 5] = [
    "@VERSION@",
    "@PLATFORM@",
    "@MIN_HOSTKEEPER@",
    "@RUN@",
    "@PARAMS@",
];

/// The unit template of the Linux packages, `units/tierstore@.service`.
pub const LINUX_UNIT: &str =
    include_str!("../../../../packaging/hostkeeper/units/tierstore@.service");

/// The launchd job of the macOS package, `units/tierstore.plist`.
pub const MACOS_PLIST: &str =
    include_str!("../../../../packaging/hostkeeper/units/tierstore.plist");

/// The label of the launchd job: the service name, the `run` entry of the macOS manifest.
pub const LAUNCHD_LABEL: &str = "tierstore";

/// The path of the binary in the package.
pub const BINARY: &str = "bin/tierstore";

/// The mode of the binary and of the directories in the archive.
pub const EXECUTABLE: u32 = 0o755;

/// The mode of every other file in the archive.
pub const FILE: u32 = 0o644;

/// What a platform's package holds besides the manifest's common keys and the binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// The files of `units/`: name and text.
    pub units: &'static [(&'static str, &'static str)],
    /// `[units] run`.
    pub run: &'static [&'static str],
    /// `[params]`, in the order written: key and string value.
    pub params: &'static [(&'static str, &'static str)],
}

/// The two Linux platforms: one template, two instances, systemd.
pub const LINUX: Target = Target {
    units: &[("tierstore@.service", LINUX_UNIT)],
    run: &["tierstore@a.service", "tierstore@b.service"],
    params: &[
        ("config_file", "/etc/tierstore/config.toml"),
        ("service_manager", "systemd"),
    ],
};

/// macOS: one launchd job, by its label.
pub const MACOS: Target = Target {
    units: &[("tierstore.plist", MACOS_PLIST)],
    run: &[LAUNCHD_LABEL],
    params: &[
        ("config_file", "/usr/local/tierstore/etc/tierstore.toml"),
        ("service_manager", "launchd"),
        ("launchd_label", LAUNCHD_LABEL),
    ],
};

impl Target {
    /// The target of `platform`.
    pub fn of(platform: Platform) -> Target {
        match platform {
            Platform::LinuxX86_64 | Platform::LinuxArm64 => LINUX,
            Platform::MacosArm64 => MACOS,
        }
    }
}

/// One entry of the archive: its name, mode and content; a directory has `None`.
pub type Entry = (String, u32, Option<Vec<u8>>);

/// Whether `part` is a whole number as a version writes it: digits, no leading zero but in `0`.
fn is_number(part: &str) -> bool {
    let digits = !part.is_empty() & part.bytes().all(|b| b.is_ascii_digit());
    digits & ((part.len() == 1) | !part.starts_with('0'))
}

/// Whether `text` is `<major>.<minor>.<patch>`: three whole numbers and nothing else.
pub fn is_plain_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    (parts.len() == 3) & parts.iter().all(|part| is_number(part))
}

/// Whether `text` is a release version as `tierstore --version` prints it:
/// `<major>.<minor>.<patch>+<12 lowercase hex digits>`.
pub fn is_release_version(text: &str) -> bool {
    text.split_once('+').is_some_and(|(plain, commit)| {
        let hex = commit
            .bytes()
            .all(|b| b.is_ascii_digit() | (b'a'..=b'f').contains(&b));
        is_plain_version(plain) & (commit.len() == 12) & hex
    })
}

/// A TOML basic string of `text`, which holds no quote, backslash or control character (every
/// value written here is a constant or a version that keeps [`is_release_version`]).
fn quoted(text: &str) -> String {
    format!("\"{text}\"")
}

/// The manifest of `version` for `platform`, which needs hostkeeper `min_hostkeeper` or later.
/// The two versions are written as given: [`hostkeeper_package`] checks them first.
pub fn manifest(platform: Platform, version: &str, min_hostkeeper: &str) -> String {
    let target = Target::of(platform);
    let run: Vec<String> = target.run.iter().map(|unit| quoted(unit)).collect();
    let params: Vec<String> = target
        .params
        .iter()
        .map(|(key, value)| format!("{key} = {}", quoted(value)))
        .collect();
    MANIFEST_TEMPLATE
        .replace("@VERSION@", version)
        .replace("@PLATFORM@", platform.name())
        .replace("@MIN_HOSTKEEPER@", min_hostkeeper)
        .replace("@RUN@", &format!("[{}]", run.join(", ")))
        .replace("@PARAMS@", &params.join("\n"))
}

/// The entries of the package of `binary` as `version` for `platform`, sorted by name: each
/// directory before what is in it.
pub fn entries(
    platform: Platform,
    version: &str,
    min_hostkeeper: &str,
    binary: &[u8],
) -> Vec<Entry> {
    let target = Target::of(platform);
    let mut entries: Vec<Entry> = vec![
        (
            "manifest.toml".to_owned(),
            FILE,
            Some(manifest(platform, version, min_hostkeeper).into_bytes()),
        ),
        ("bin".to_owned(), EXECUTABLE, None),
        (BINARY.to_owned(), EXECUTABLE, Some(binary.to_vec())),
        ("units".to_owned(), EXECUTABLE, None),
    ];
    for (name, text) in target.units {
        entries.push((
            format!("units/{name}"),
            FILE,
            Some(text.as_bytes().to_vec()),
        ));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// The ustar archive of `entries`, in order, each with owner and group 0, no owner names and
/// time 0, written to `sink`, which is given back.
///
/// # Errors
/// A name does not fit a ustar header, or `sink` cannot be written.
pub fn tar_to<W: Write>(sink: W, entries: &[Entry]) -> Result<W, Error> {
    let mut tar = tar::Builder::new(sink);
    for (name, mode, content) in entries {
        let mut header = tar::Header::new_ustar();
        header
            .set_path(name)
            .map_err(Error::spawn(format!("the tar entry {name:?}")))?;
        let kind = match content {
            Some(_) => tar::EntryType::Regular,
            None => tar::EntryType::Directory,
        };
        let bytes = content.as_deref().unwrap_or_default();
        header.set_entry_type(kind);
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        tar.append(&header, bytes)
            .map_err(Error::spawn(format!("write the tar entry {name:?}")))?;
    }
    tar.into_inner()
        .map_err(Error::spawn("finish the tar archive".to_owned()))
}

/// The package of `binary` as `version` for `platform`: the tar of [`entries`] in one zstd
/// frame.
///
/// # Errors
/// The archive cannot be written (a name too long for ustar).
pub fn archive(
    platform: Platform,
    version: &str,
    min_hostkeeper: &str,
    binary: &[u8],
) -> Result<Vec<u8>, Error> {
    let tar = tar_to(
        Vec::new(),
        &entries(platform, version, min_hostkeeper, binary),
    )?;
    Ok(ruzstd::encoding::compress_to_vec(
        &tar[..],
        ruzstd::encoding::CompressionLevel::Fastest,
    ))
}

/// `tierstore-<version>-<platform>.tar.zst`.
pub fn file_name(version: &str, platform: Platform) -> String {
    format!("{SERVICE}-{version}-{}.tar.zst", platform.name())
}

/// Whether `haystack` holds `needle` somewhere.
fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The `release hostkeeper-package` verb: the line to print.
///
/// # Errors
/// A version that is not of its form, a binary that cannot be read or does not hold `version`,
/// an `out` that cannot be created, a package that is there already or that the sums file lists
/// already.
pub fn hostkeeper_package(
    binary: &Path,
    version: &str,
    min_hostkeeper: &str,
    platform: Platform,
    out: &Path,
) -> Result<Vec<String>, Error> {
    if !is_release_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>+<12 lowercase hex digits>, what `tierstore --version` prints"
        )));
    }
    if !is_plain_version(min_hostkeeper) {
        return Err(Error::Usage(format!(
            "--min-hostkeeper {min_hostkeeper:?} is not <major>.<minor>.<patch>"
        )));
    }
    let bytes = read(binary)?;
    if !holds(&bytes, version.as_bytes()) {
        return Err(Error::Failed(format!(
            "{} does not hold the version {version:?}, so it does not report it",
            binary.display()
        )));
    }
    let package = archive(platform, version, min_hostkeeper, &bytes)?;
    let name = file_name(version, platform);
    let (sums_path, mut lines) = open_sums(out, platform)?;
    if lines.iter().any(|l| l.name == name) {
        return Err(Error::Failed(format!(
            "{} already lists {name}",
            sums_path.display()
        )));
    }
    write_new(&out.join(&name), &package)?;
    lines.push(sums::Line {
        sha256: fetch::Sha256::of(&package),
        name: name.clone(),
    });
    write_sums(&sums_path, &lines)?;
    Ok(vec![format!(
        "{name}: sha256 {}, {} bytes",
        hex::encode(Sha256::digest(&package)),
        package.len()
    )])
}

#[cfg(test)]
mod tests;

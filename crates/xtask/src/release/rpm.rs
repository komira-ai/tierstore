//! `release rpm --binary <path> --version <X.Y.Z> --platform <platform> --licenses <path> --out
//! <dir>`: the RPM package of one Linux platform's binary, `<out>/tierstore-<version>-1.<arch>.rpm`
//! (`x86_64` for `linux-x86_64`, `aarch64` for `linux-arm64`), whose line it appends to
//! `<out>/SHA256SUMS.<platform>` ([`crate::sums`]), the file `package` and `deb` write the
//! tarball's, the SBOM's and the Debian package's lines to. The package may not exist already, and
//! the sums file may not list it yet. macOS has no package. Nothing here runs `rpm`; the build job
//! of `artifacts.yml` reads the result back with [`inspect`] (`release rpm-info`).
//!
//! The package has the files of the Debian package ([`deb::files`]) at the same paths and modes,
//! with the two differences the RPM conventions ask for: the licence is installed as a `%license`
//! file, `/usr/share/licenses/tierstore/LICENSE`, and not as `/usr/share/doc/tierstore/copyright`,
//! and the example configuration is `%config(noreplace)`, so an upgrade keeps the host's edits
//! (and writes a new example beside it as `.rpmnew`). Every file is owned by `root`, the
//! directories this package owns (`/etc/tierstore` is 0750, the others 0755) are in the package,
//! and the package requires `systemd`, which provides `systemd-sysusers`, and `glibc >= 2.28`
//! ([`glibc::FLOOR`]), the glibc the binary is linked against, so a host with an older one refuses
//! the install instead of failing to start the service.
//!
//! The scriptlets ([`POST`], [`POSTUN`]) do what the Debian maintainer scripts do. `%post` runs
//! on every install and upgrade: it creates the account with `systemd-sysusers`, gives
//! `/etc/tierstore` and the example file to its group and tells a running systemd about the unit,
//! each step only where its tool or the group is there, so a container without systemd still
//! installs. `%postun` tells systemd the unit is gone, when the last version is removed. Nothing
//! starts the unit and nothing enables it; removing the package leaves the account and
//! `/etc/tierstore`.
//!
//! The file is the same bytes for the same inputs on every runner: the build time, the time of
//! every file and the build host are fixed ([`deb::MTIME`], [`BUILD_HOST`]), the payload is gzip
//! at a fixed level (the one every `rpm` since 4.x reads), and the package is not signed (the
//! signature header holds the digests only, so the attested bytes are the unsigned ones).

use std::collections::BTreeSet;
use std::path::Path;

use rpm::{
    BuildConfig, CompressionWithLevel, Dependency, DependencyFlags, FileFlags, FileMode,
    FileOptions, Package, PackageBuilder, Scriptlet,
};

use crate::Error;
use crate::fetch::Sha256;
use crate::package::{Platform, open_sums, read, write_new, write_sums};
use crate::release::deb::{self, File};
use crate::release::glibc;
use crate::release::hostkeeper_package::is_plain_version;
use crate::sums;

/// The release field: the first build of the version.
pub const RELEASE: &str = "1";

/// The build host the header records, so no runner's name is in the package.
pub const BUILD_HOST: &str = "localhost";

/// The gzip level of the payload, fixed so the bytes do not depend on a default.
pub const GZIP_LEVEL: u32 = 9;

/// The licence's path in the package: the `%license` directory of the package.
pub const LICENSE_PATH: &str = "usr/share/licenses/tierstore/LICENSE";

/// The licence's path in the Debian package, which this package does not have.
const DEB_LICENSE_PATH: &str = "usr/share/doc/tierstore/copyright";

/// The directories the package owns, with their mode.
const DIRECTORIES: [(&str, u16); 4] = [
    ("/etc/tierstore", 0o750),
    ("/usr/libexec/tierstore", 0o755),
    ("/usr/share/doc/tierstore", 0o755),
    ("/usr/share/licenses/tierstore", 0o755),
];

/// The `%post` scriptlet, run with `/bin/sh` on every install and upgrade.
pub const POST: &str =
    "# Creates the tierstore account, gives /etc/tierstore to its group and tells systemd about the
# unit. It starts nothing and enables nothing; each step is skipped where its tool is missing.
if command -v systemd-sysusers >/dev/null 2>&1; then
    systemd-sysusers /usr/lib/sysusers.d/tierstore.conf
fi
if getent group tierstore >/dev/null 2>&1; then
    chgrp tierstore /etc/tierstore /etc/tierstore/tierstore.toml
    chmod 0750 /etc/tierstore
    chmod 0640 /etc/tierstore/tierstore.toml
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
fi
exit 0
";

/// The `%postun` scriptlet, run with `/bin/sh` after every removal and upgrade.
pub const POSTUN: &str = "# Tells systemd the unit is gone when the last version is removed. The account and /etc/tierstore,
# which holds the keys, stay.
if [ \"$1\" -eq 0 ] && [ -d /run/systemd/system ]; then
    systemctl daemon-reload
fi
exit 0
";

/// The RPM architecture of `platform`, or none when the platform has no package.
pub fn architecture(platform: Platform) -> Option<&'static str> {
    match platform {
        Platform::LinuxX86_64 => Some("x86_64"),
        Platform::LinuxArm64 => Some("aarch64"),
        Platform::MacosArm64 => None,
    }
}

/// `tierstore-<version>-1.<architecture>.rpm`.
pub fn file_name(version: &str, architecture: &str) -> String {
    format!("{}-{version}-{RELEASE}.{architecture}.rpm", deb::NAME)
}

/// The files of the package of `binary`, with `notice` as the third-party licence notice: those of
/// the Debian package, the licence at [`LICENSE_PATH`].
pub fn files(binary: &[u8], notice: &[u8]) -> Vec<File> {
    let mut files = deb::files(binary, notice);
    for file in &mut files {
        if file.0 == DEB_LICENSE_PATH {
            LICENSE_PATH.clone_into(&mut file.0);
        }
    }
    files
}

/// The package file of `version` for `architecture` holding `files`.
///
/// # Errors
/// The `rpm` crate refuses an input (a path or a field with a control character).
pub fn build(version: &str, architecture: &str, files: &[File]) -> Result<Vec<u8>, Error> {
    let config = BuildConfig::v4()
        .compression(CompressionWithLevel::Gzip(GZIP_LEVEL))
        .source_date(deb::MTIME);
    let mut builder = PackageBuilder::new(
        deb::NAME,
        version,
        "Apache-2.0",
        architecture,
        deb::SYNOPSIS,
    );
    builder
        .using_config(config)
        .release(RELEASE)
        .description(description())
        .url(deb::HOMEPAGE)
        .packager(deb::MAINTAINER)
        .build_host(BUILD_HOST)
        .requires(Dependency::any("systemd"))
        .requires(Dependency::greater_eq("glibc", glibc::FLOOR))
        .post_install_script(Scriptlet::new(POST).prog(vec!["/bin/sh"]))
        .post_uninstall_script(Scriptlet::new(POSTUN).prog(vec!["/bin/sh"]));
    for (path, permissions) in DIRECTORIES {
        builder
            .with_dir_entry(FileOptions::dir(path).permissions(permissions))
            .map_err(failed("add a directory"))?;
    }
    let mut sorted: Vec<&File> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, mode, content) in sorted {
        let absolute = format!("/{path}");
        let mut options = FileOptions::new(absolute)
            .mode(FileMode::regular(*mode as u16))
            .user("root")
            .group("root");
        if path == deb::CONFFILE.trim_start_matches('/') {
            options = options.config().noreplace();
        } else if path == LICENSE_PATH {
            options = options.license();
        }
        builder
            .with_file_contents(content.clone(), options)
            .map_err(failed("add a file"))?;
    }
    let package = builder.build().map_err(failed("build the package"))?;
    let mut bytes = Vec::new();
    package
        .write(&mut bytes)
        .map_err(failed("write the package"))?;
    Ok(bytes)
}

/// The description: the Debian package's, without its blank-line marker.
fn description() -> String {
    deb::DESCRIPTION
        .iter()
        .map(|line| (*line).to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A requirement as `rpm -qR` writes it: `name`, or `name >= version`.
fn requirement(dependency: rpm::Dependency) -> String {
    let op: String = [
        (DependencyFlags::LESS, "<"),
        (DependencyFlags::GREATER, ">"),
        (DependencyFlags::EQUAL, "="),
    ]
    .into_iter()
    .filter(|(flag, _)| dependency.flags.contains(*flag))
    .map(|(_, text)| text)
    .collect();
    if op.is_empty() {
        return dependency.name;
    }
    format!("{} {op} {}", dependency.name, dependency.version)
}

/// The error for a failure of the `rpm` crate while it does `what`.
fn failed(what: &'static str) -> impl FnOnce(rpm::Error) -> Error {
    move |e| Error::Failed(format!("{what}: {e}"))
}

/// One file of a package as `rpm -qlv` lists it.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// The absolute path.
    pub path: String,
    /// The permission bits.
    pub mode: u16,
    /// Whether it is a directory.
    pub directory: bool,
    /// The owner and the group.
    pub owner: (String, String),
    /// Whether `%config`, `%config(noreplace)` and `%license` are set.
    pub config: bool,
    /// See `config`.
    pub noreplace: bool,
    /// See `config`.
    pub license: bool,
}

/// What a package says about itself, as `rpm -qip`, `rpm -qlv` and `rpm -q --scripts` show it.
#[derive(Debug, PartialEq, Eq)]
pub struct Info {
    /// Name, version, release and architecture.
    pub nevra: (String, String, String, String),
    /// The build time, seconds since the epoch.
    pub build_time: u64,
    /// The build host.
    pub build_host: String,
    /// The compression of the payload (`gzip`).
    pub compression: String,
    /// The names the package requires, sorted, besides the `rpmlib(...)` ones.
    pub requires: Vec<String>,
    /// The files, sorted by path.
    pub entries: Vec<Entry>,
    /// `%post`, `%postun` and their interpreters.
    pub post: (String, Vec<String>),
    /// See `post`.
    pub postun: (String, Vec<String>),
}

/// Reads `bytes` as a package and checks its digests.
///
/// # Errors
/// The bytes are not a package, a digest does not match, or a field is missing.
pub fn inspect(bytes: &[u8]) -> Result<Info, Error> {
    let package = Package::parse(&mut &bytes[..]).map_err(failed("read the package"))?;
    package
        .verify_digests()
        .map_err(failed("check the digests"))?;
    let meta = &package.metadata;
    // The payload decompresses and holds every file the header lists.
    package
        .files()
        .and_then(|mut files| files.try_for_each(|file| file.map(drop)))
        .map_err(failed("unpack the payload"))?;
    let requires: Vec<String> = meta
        .get_requires()
        .map_err(failed("read the requirements"))?
        .into_iter()
        .filter(|d| !d.name.starts_with("rpmlib("))
        .map(requirement)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut entries: Vec<Entry> = meta
        .get_file_entries()
        .map_err(failed("read the files"))?
        .iter()
        .map(|e| Entry {
            path: e.path().to_string_lossy().into_owned(),
            mode: e.permissions(),
            directory: e.file_type() == rpm::FileType::Dir,
            owner: (e.user().to_owned(), e.group().to_owned()),
            config: e.flags().contains(FileFlags::CONFIG),
            noreplace: e.flags().contains(FileFlags::NOREPLACE),
            license: e.flags().contains(FileFlags::LICENSE),
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let script = |s: Scriptlet| (s.script, s.program.unwrap_or_default());
    Ok(Info {
        nevra: (
            meta.get_name().map_err(failed("read the name"))?.to_owned(),
            meta.get_version()
                .map_err(failed("read the version"))?
                .to_owned(),
            meta.get_release()
                .map_err(failed("read the release"))?
                .to_owned(),
            meta.get_arch()
                .map_err(failed("read the architecture"))?
                .to_owned(),
        ),
        build_time: meta
            .get_build_time()
            .map_err(failed("read the build time"))?,
        build_host: meta
            .get_build_host()
            .map_err(failed("read the build host"))?
            .to_owned(),
        compression: meta
            .get_payload_compressor()
            .map_err(failed("read the compression"))?
            .to_string(),
        requires,
        entries,
        post: script(
            meta.get_post_install_script()
                .map_err(failed("read %post"))?,
        ),
        postun: script(
            meta.get_post_uninstall_script()
                .map_err(failed("read %postun"))?,
        ),
    })
}

/// The `release rpm` verb: the line to print.
///
/// # Errors
/// A version that is not `<major>.<minor>.<patch>`, a platform without a package, a binary or
/// notice that cannot be read (or a notice that is empty), an `out` that cannot be created, a
/// package that is there already or that the sums file lists already.
pub fn rpm(
    binary: &Path,
    version: &str,
    platform: Platform,
    licenses: &Path,
    out: &Path,
) -> Result<Vec<String>, Error> {
    if !is_plain_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )));
    }
    let Some(architecture) = architecture(platform) else {
        return Err(Error::Usage(format!(
            "--platform {}: only the Linux platforms have an RPM package",
            platform.name()
        )));
    };
    let bytes = read(binary)?;
    let notice = read(licenses)?;
    if notice.is_empty() {
        return Err(Error::Failed("the licence notice is empty".to_owned()));
    }
    let package = build(version, architecture, &files(&bytes, &notice))?;
    let name = file_name(version, architecture);
    let (sums_path, mut lines) = open_sums(out, platform)?;
    if lines.iter().any(|l| l.name == name) {
        return Err(Error::Failed(format!(
            "{} already lists {name}",
            sums_path.display()
        )));
    }
    write_new(&out.join(&name), &package)?;
    let sha256 = Sha256::of(&package);
    lines.push(sums::Line {
        sha256,
        name: name.clone(),
    });
    write_sums(&sums_path, &lines)?;
    Ok(vec![format!(
        "{name}: sha256 {sha256}, {} bytes",
        package.len()
    )])
}

/// The `release rpm-info` verb: the lines `rpm -qip` and `rpm -qlv` would show for the package
/// `path`, after the check of its digests.
///
/// # Errors
/// The file cannot be read or is not a package.
pub fn rpm_info(path: &Path) -> Result<Vec<String>, Error> {
    let info = inspect(&read(path)?)?;
    let (name, version, release, arch) = &info.nevra;
    let mut lines = vec![
        format!("Name: {name}"),
        format!("Version: {version}"),
        format!("Release: {release}"),
        format!("Architecture: {arch}"),
        format!("Compression: {}", info.compression),
        format!("Requires: {}", info.requires.join(", ")),
    ];
    for e in &info.entries {
        lines.push(format!(
            "{}{:04o} {}:{} {}{}",
            if e.directory { "d" } else { "-" },
            e.mode,
            e.owner.0,
            e.owner.1,
            e.path,
            match (e.config, e.noreplace, e.license) {
                (true, true, _) => " %config(noreplace)",
                (false, _, true) => " %license",
                _ => "",
            }
        ));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests;

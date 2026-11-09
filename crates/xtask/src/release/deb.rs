//! `release deb --binary <path> --version <X.Y.Z> --platform <platform> --licenses <path> --out
//! <dir>`: the Debian package of one Linux platform's binary, `<out>/tierstore_<version>_<arch>.deb`
//! (`amd64` for `linux-x86_64`, `arm64` for `linux-arm64`), whose line it appends to
//! `<out>/SHA256SUMS.<platform>` ([`crate::sums`]), the file `package` writes the tarball's and the
//! SBOM's lines to. The package may not exist already, and the sums file may not list it yet.
//! macOS has no package. Nothing here runs `dpkg`; the build job of `artifacts.yml` runs
//! `dpkg-deb --info` and `dpkg-deb --contents` on the result.
//!
//! The package installs, as `docs/install.md` of the `tierstore` crate places them by hand:
//!
//! | path | mode | what |
//! |---|---|---|
//! | `/usr/libexec/tierstore/tierstore` | 0755 | the binary, the exact bytes given |
//! | `/usr/lib/systemd/system/tierstore.service` | 0644 | `packaging/tierstore.service` |
//! | `/usr/lib/sysusers.d/tierstore.conf` | 0644 | `packaging/tierstore.conf`, the account of the unit |
//! | `/etc/tierstore/tierstore.toml` | 0640 | `packaging/tierstore.toml`, an example; a conffile, so an upgrade keeps the host's edits |
//! | `/usr/share/doc/tierstore/copyright` | 0644 | the repository's `LICENSE` |
//! | `/usr/share/doc/tierstore/THIRD-PARTY-LICENSES.txt` | 0644 | the notice of the crates linked into the binary (`--licenses`) |
//!
//! The package depends on `systemd` and on `libc6 (>= 2.28)` ([`glibc::FLOOR`]), the glibc the
//! binary is linked against, so `apt` and `dpkg` refuse to install it on a host with an older one.
//!
//! The maintainer scripts ([`POSTINST`], [`POSTRM`]) create the account with `systemd-sysusers`,
//! give `/etc/tierstore` and the example file to its group (mode 0750 and 0640, as `docs/install.md`
//! has them) and run `systemctl daemon-reload`. Nothing starts the unit and nothing enables it: the
//! unit has no `[Install]` section and `ConditionPathExists=/etc/tierstore/access.key` keeps it
//! from starting before the keys are placed. Removing the package leaves the account and
//! `/etc/tierstore` (which holds the keys) in place.
//!
//! The file is the same bytes for the same inputs on every runner: an `ar` archive of
//! `debian-binary`, `control.tar.gz` and `data.tar.gz`, in that order, every member and entry
//! owned by `root`, dated [`MTIME`] (not 1970, which `lintian` refuses), the entries sorted by
//! path with each directory before what is in it, in gzip streams with no file name and the
//! same time.

use std::collections::BTreeSet;
use std::path::Path;

use flate2::Compression;
use md5::{Digest, Md5};

use crate::Error;
use crate::fetch::Sha256;
use crate::package::{Platform, open_sums, read, write_new, write_sums};
use crate::release::glibc;
use crate::release::hostkeeper_package::is_plain_version;
use crate::sums;

/// The package's name, and the directory of its files below `usr/share/doc` and `etc`.
pub const NAME: &str = "tierstore";

/// The time of every member and entry: 2026-09-01T00:00:00Z. A fixed date so the same inputs give
/// the same bytes, and not the epoch, which `lintian` refuses (`package-contains-ancient-file`).
pub const MTIME: u32 = 1_788_220_800;

/// The maintainer field: the project, as a noreply address.
pub const MAINTAINER: &str = "tierstore authors <tierstore@users.noreply.github.com>";

/// The project's home page.
pub const HOMEPAGE: &str = "https://tierstore.dev";

/// The first line of the description.
pub const SYNOPSIS: &str = "S3 server over a hot and a cold tier of S3-compatible stores";

/// The description after its first line, one string per line; an empty one is written ` .`.
pub const DESCRIPTION: [&str; 5] = [
    "New objects are written to the hot tier, copied to the cold tier in the background,",
    "and freed from the hot tier when it fills.",
    "",
    "The package installs the binary, the systemd unit and an example configuration. It starts",
    "and enables nothing: write the key files under /etc/tierstore, then start tierstore.service.",
];

/// The unit, as `packaging/tierstore.service`.
pub const UNIT: &str = include_str!("../../../../packaging/tierstore.service");

/// The sysusers.d entry of the unit's account, as `packaging/tierstore.conf`.
pub const SYSUSERS: &str = include_str!("../../../../packaging/tierstore.conf");

/// The example configuration, as `packaging/tierstore.toml`.
pub const EXAMPLE_CONFIG: &str = include_str!("../../../../packaging/tierstore.toml");

/// The licence of the project, as `LICENSE`: the package's `copyright` file.
pub const LICENCE: &str = include_str!("../../../../LICENSE");

/// The configuration file, the one conffile (an absolute path, as `conffiles` lists it).
pub const CONFFILE: &str = "/etc/tierstore/tierstore.toml";

/// The directories whose mode is not 0755, below the root, with their mode.
const DIRECTORY_MODES: [(&str, u32); 1] = [("etc/tierstore", 0o750)];

/// The `postinst` script.
pub const POSTINST: &str = "#!/bin/sh
# Creates the tierstore account, gives /etc/tierstore to its group and tells systemd about the
# unit. It starts nothing and enables nothing.
set -e
if [ \"$1\" = configure ]; then
    systemd-sysusers /usr/lib/sysusers.d/tierstore.conf
    chgrp tierstore /etc/tierstore /etc/tierstore/tierstore.toml
    chmod 0750 /etc/tierstore
    chmod 0640 /etc/tierstore/tierstore.toml
    if [ -d /run/systemd/system ]; then
        systemctl daemon-reload
    fi
fi
exit 0
";

/// The `postrm` script.
pub const POSTRM: &str = "#!/bin/sh
# Tells systemd the unit is gone. The account and /etc/tierstore, which holds the keys, stay.
set -e
if [ \"$1\" = remove ] || [ \"$1\" = purge ]; then
    if [ -d /run/systemd/system ]; then
        systemctl daemon-reload
    fi
fi
exit 0
";

/// One file of the package: its path from the root with no leading slash, mode and content.
pub type File = (String, u32, Vec<u8>);

/// The Debian architecture of `platform`, or none when the platform has no package.
pub fn architecture(platform: Platform) -> Option<&'static str> {
    match platform {
        Platform::LinuxX86_64 => Some("amd64"),
        Platform::LinuxArm64 => Some("arm64"),
        Platform::MacosArm64 => None,
    }
}

/// `tierstore_<version>_<architecture>.deb`.
pub fn file_name(version: &str, architecture: &str) -> String {
    format!("{NAME}_{version}_{architecture}.deb")
}

/// The files of the package of `binary`, with `notice` as the third-party licence notice.
pub fn files(binary: &[u8], notice: &[u8]) -> Vec<File> {
    let file = |path: &str, mode: u32, bytes: &[u8]| (path.to_owned(), mode, bytes.to_vec());
    vec![
        file("usr/libexec/tierstore/tierstore", 0o755, binary),
        file(
            "usr/lib/systemd/system/tierstore.service",
            0o644,
            UNIT.as_bytes(),
        ),
        file(
            "usr/lib/sysusers.d/tierstore.conf",
            0o644,
            SYSUSERS.as_bytes(),
        ),
        file(
            "etc/tierstore/tierstore.toml",
            0o640,
            EXAMPLE_CONFIG.as_bytes(),
        ),
        file(
            "usr/share/doc/tierstore/copyright",
            0o644,
            LICENCE.as_bytes(),
        ),
        file(
            "usr/share/doc/tierstore/THIRD-PARTY-LICENSES.txt",
            0o644,
            notice,
        ),
    ]
}

/// The `control` file of `version` for `architecture` holding `files`.
pub fn control(version: &str, architecture: &str, files: &[File]) -> String {
    let bytes: usize = files.iter().map(|(_, _, content)| content.len()).sum();
    let mut text = format!(
        "Package: {NAME}\nVersion: {version}\nArchitecture: {architecture}\n\
         Maintainer: {MAINTAINER}\nInstalled-Size: {}\nDepends: systemd, libc6 (>= {})\n\
         Section: admin\nPriority: optional\nHomepage: {HOMEPAGE}\nDescription: {SYNOPSIS}\n",
        bytes.div_ceil(1024),
        glibc::FLOOR
    );
    for line in DESCRIPTION {
        if line.is_empty() {
            text.push_str(" .\n");
        } else {
            text.push_str(&format!(" {line}\n"));
        }
    }
    text
}

/// `md5sums`: the MD5 of each file, in path order, as `md5sum` writes them.
pub fn md5sums(files: &[File]) -> String {
    let mut sorted: Vec<&File> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted
        .iter()
        .map(|(path, _, content)| format!("{}  {path}\n", hex::encode(Md5::digest(content))))
        .collect()
}

/// One entry of a tar: path, mode and content; a directory has none.
type Entry = (String, u32, Option<Vec<u8>>);

/// The directories above `files` (every prefix of a path, each once) as `./a/b/` entries, and
/// the files as `./a/b/c`, sorted by path, each directory before what is in it.
fn data_entries(files: &[File]) -> Vec<Entry> {
    let mut dirs = BTreeSet::new();
    for (path, _, _) in files {
        let mut end = 0;
        while let Some(slash) = path[end..].find('/') {
            end += slash;
            dirs.insert(path[..end].to_owned());
            end += 1;
        }
    }
    let mut entries: Vec<Entry> = vec![("./".to_owned(), 0o755, None)];
    for dir in dirs {
        let mode = DIRECTORY_MODES
            .iter()
            .find(|(name, _)| *name == dir)
            .map_or(0o755, |(_, mode)| *mode);
        entries.push((format!("./{dir}/"), mode, None));
    }
    for (path, mode, content) in files {
        entries.push((format!("./{path}"), *mode, Some(content.clone())));
    }
    entries.sort_by(|a, b| a.0.trim_end_matches('/').cmp(b.0.trim_end_matches('/')));
    entries
}

/// The gzip-compressed tar of `entries`, in order, each owned by `root` and dated [`MTIME`].
///
/// # Errors
/// A path is longer than the name field of a tar header.
fn tar_gz(entries: &[Entry]) -> Result<Vec<u8>, Error> {
    let gzip = flate2::GzBuilder::new()
        .mtime(MTIME)
        .write(Vec::new(), Compression::best());
    let mut tar = tar::Builder::new(gzip);
    for (path, mode, content) in entries {
        let mut header = tar::Header::new_ustar();
        // The name is written as it is, with its `./`, which `set_path` would drop. Its field
        // is 100 bytes and the prefix field is not used.
        let field = &mut header.as_old_mut().name;
        if path.len() > field.len() {
            return Err(Error::Failed(format!(
                "{path:?} is longer than the {} bytes of a tar entry name",
                field.len()
            )));
        }
        field[..path.len()].copy_from_slice(path.as_bytes());
        let bytes = content.as_deref().unwrap_or_default();
        header.set_entry_type(if content.is_some() {
            tar::EntryType::Regular
        } else {
            tar::EntryType::Directory
        });
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        header.set_uid(0);
        header.set_gid(0);
        // "root" always fits the 32 bytes ustar has for a name, so these cannot fail.
        let _ = header.set_username("root");
        let _ = header.set_groupname("root");
        header.set_mtime(u64::from(MTIME));
        header.set_cksum();
        tar.append(&header, bytes)
            .map_err(Error::spawn(format!("write the tar entry {path:?}")))?;
    }
    tar.into_inner()
        .and_then(flate2::write::GzEncoder::finish)
        .map_err(Error::spawn("finish the tar archive".to_owned()))
}

/// The 60-byte header of an `ar` member `name` holding `len` bytes.
///
/// # Errors
/// `len` does not fit the header's 10 digits.
pub fn ar_header(name: &str, len: u64) -> Result<String, Error> {
    if len > 9_999_999_999 {
        return Err(Error::Failed(format!(
            "{name} is {len} bytes, more than an ar header holds"
        )));
    }
    Ok(format!(
        "{name:<16}{MTIME:<12}{:<6}{:<6}{:<8}{len:<10}`\n",
        0, 0, "100644"
    ))
}

/// The `ar` archive of the members `(name, content)`, in order, each padded to an even length.
///
/// # Errors
/// A member does not fit an `ar` header.
pub fn ar(members: &[(&str, &[u8])]) -> Result<Vec<u8>, Error> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, content) in members {
        out.extend_from_slice(ar_header(name, content.len() as u64)?.as_bytes());
        out.extend_from_slice(content);
        if content.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    Ok(out)
}

/// The package file of `version` for `architecture` holding `files`.
///
/// # Errors
/// A path is longer than the name field of a tar header, or a member does not fit an `ar`
/// header.
pub fn build(version: &str, architecture: &str, files: &[File]) -> Result<Vec<u8>, Error> {
    let script = |text: &str| Some(text.as_bytes().to_vec());
    let control_tar = tar_gz(&[
        ("./".to_owned(), 0o755, None),
        (
            "./control".to_owned(),
            0o644,
            script(&control(version, architecture, files)),
        ),
        (
            "./conffiles".to_owned(),
            0o644,
            script(&format!("{CONFFILE}\n")),
        ),
        ("./md5sums".to_owned(), 0o644, script(&md5sums(files))),
        ("./postinst".to_owned(), 0o755, script(POSTINST)),
        ("./postrm".to_owned(), 0o755, script(POSTRM)),
    ])?;
    let data_tar = tar_gz(&data_entries(files))?;
    ar(&[
        ("debian-binary", b"2.0\n"),
        ("control.tar.gz", &control_tar),
        ("data.tar.gz", &data_tar),
    ])
}

/// The `release deb` verb: the line to print.
///
/// # Errors
/// A version that is not `<major>.<minor>.<patch>`, a platform without a package, a binary or
/// notice that cannot be read (or a notice that is empty), an `out` that cannot be created, a
/// package that is there already or that the sums file lists already.
pub fn deb(
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
            "--platform {}: only the Linux platforms have a Debian package",
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

#[cfg(test)]
mod tests;

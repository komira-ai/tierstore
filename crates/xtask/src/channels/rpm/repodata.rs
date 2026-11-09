//! The metadata of a yum/dnf repository, as `createrepo_c` writes it: `primary.xml`,
//! `filelists.xml` and `other.xml`, each gzipped and named by the SHA-256 of the gzip, and the
//! `repomd.xml` that lists them. Everything is a pure function of its inputs: the revision and the
//! timestamps are a parameter, nothing reads a clock, and the gzip has no name and no time.
//!
//! The repository is shared with other products. Their packages are kept as the text they were
//! written as: a package of a repository that is read is three chunks of XML (its `<package>`
//! element in each of the three files), found by the `pkgid` they share, and a merge keeps the
//! chunks of every package it does not replace, in their order. Only a package this tool builds is
//! rendered here ([`package_of`], from the bytes of the `.rpm`).
//!
//! `<location href>` is relative to the repository's `baseurl` (`rpm/<suite>/<arch>`): the package
//! is in the pool, `../../pool/<product>/<file>`, which both suites and both architectures share.

use std::cmp::Ordering;
use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rpm::{DependencyFlags, Package as Rpm};

use super::layout;
use super::state::POOL_DIR;
use crate::Error;
use crate::channels::numbers;
use crate::fetch::Sha256;

/// `<major>.<minor>.<patch>` as numbers.
pub type Version = (u64, u64, u64);

/// One package of a repository: its name, version and digest, and its three chunks of XML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub arch: String,
    /// `<major>.<minor>.<patch>` of `ver`, when it is one.
    pub version: Option<Version>,
    /// The SHA-256 of the file, which is the `pkgid`.
    pub pkgid: String,
    /// The `href` of its `<location>`.
    pub href: String,
    pub primary: String,
    pub filelists: String,
    pub other: String,
}

/// `text` escaped for XML content and attribute values.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn failed(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Failed(format!("{what}: {e}"))
}

/// A field of the package, or the failure to read it.
fn field<T>(what: &str, r: Result<T, rpm::Error>) -> Result<T, Error> {
    r.map_err(|e| failed(&format!("read the {what}"), e))
}

/// The files of a package that `primary.xml` lists besides the directories: those `createrepo`
/// lists, which a dependency may name.
fn in_primary(path: &str) -> bool {
    ["/etc/", "/bin/", "/sbin/", "/usr/bin/", "/usr/sbin/"]
        .iter()
        .any(|p| path.starts_with(p))
}

/// `epoch`, `ver` and `rel` of a dependency's version: `[<epoch>:]<ver>[-<rel>]`.
fn evr(version: &str) -> (&str, &str, Option<&str>) {
    let (epoch, rest) = version.split_once(':').unwrap_or(("0", version));
    match rest.split_once('-') {
        Some((ver, rel)) => (epoch, ver, Some(rel)),
        None => (epoch, rest, None),
    }
}

/// The `<rpm:entry>` of a dependency.
fn entry(dep: &rpm::Dependency) -> String {
    let flags = &dep.flags;
    let op = [
        (DependencyFlags::LE, "LE"),
        (DependencyFlags::GE, "GE"),
        (DependencyFlags::EQUAL, "EQ"),
        (DependencyFlags::LESS, "LT"),
        (DependencyFlags::GREATER, "GT"),
    ]
    .into_iter()
    .find(|(f, _)| flags.contains(*f))
    .map(|(_, op)| op);
    let name = escape(&dep.name);
    match op {
        Some(op) if !dep.version.is_empty() => {
            let (epoch, ver, rel) = evr(&dep.version);
            let rel = rel.map_or(String::new(), |r| format!(" rel=\"{}\"", escape(r)));
            format!(
                "      <rpm:entry name=\"{name}\" flags=\"{op}\" epoch=\"{}\" ver=\"{}\"{rel}/>\n",
                escape(epoch),
                escape(ver)
            )
        }
        _ => format!("      <rpm:entry name=\"{name}\"/>\n"),
    }
}

/// The entries of one kind of dependency, once each, as an element; none when there are none. The
/// `rpmlib(...)` requirements are left out: they name what the installed `rpm` itself provides.
fn dependencies(tag: &str, deps: Vec<rpm::Dependency>) -> String {
    let mut entries: Vec<String> = Vec::new();
    for dep in deps.iter().filter(|d| !d.name.starts_with("rpmlib(")) {
        let line = entry(dep);
        if !entries.contains(&line) {
            entries.push(line);
        }
    }
    let body: String = entries.concat();
    if body.is_empty() {
        String::new()
    } else {
        format!("    <rpm:{tag}>\n{body}    </rpm:{tag}>\n")
    }
}

/// The three chunks of the package whose file is `bytes`, to be listed under the pool file name
/// `file`.
///
/// # Errors
/// Bytes that are not a package, a file name other than the package's own, or a field missing.
pub fn package_of(file: &str, bytes: &[u8]) -> Result<Package, Error> {
    let parts = layout::split(bytes)?;
    let package = Rpm::parse(&mut &bytes[..]).map_err(|e| failed("read the package", e))?;
    let meta = &package.metadata;
    let get = |what: &str, r: Result<&str, rpm::Error>| field(what, r).map(escape);
    let (name, arch) = (
        get("name", meta.get_name())?,
        get("architecture", meta.get_arch())?,
    );
    let (version, release) = (
        get("version", meta.get_version())?,
        get("release", meta.get_release())?,
    );
    let want = format!("{name}-{version}-{release}.{arch}.rpm");
    if file != want {
        return Err(Error::Failed(format!(
            "{file} is the package {want}: its header does not say the name it has"
        )));
    }
    let pkgid = Sha256::of(bytes).to_string();
    let (start, end) = parts.header_range();
    let build = field("build time", meta.get_build_time())?;
    let installed = field("installed size", meta.get_installed_size())?;
    let files = field("files", meta.get_file_entries())?;
    let mut listed: Vec<(String, bool)> = files
        .iter()
        .map(|f| {
            (
                f.path().to_string_lossy().into_owned(),
                f.file_type() == rpm::FileType::Dir,
            )
        })
        .collect();
    listed.sort();
    let in_primary_files: String = listed
        .iter()
        .filter(|(p, dir)| !dir && in_primary(p))
        .map(|(p, _)| format!("    <file>{}</file>\n", escape(p)))
        .collect();
    let in_filelists: String = listed
        .iter()
        .map(|(p, dir)| {
            let kind = if *dir { " type=\"dir\"" } else { "" };
            format!("  <file{kind}>{}</file>\n", escape(p))
        })
        .collect();
    let evr = format!("<version epoch=\"0\" ver=\"{version}\" rel=\"{release}\"/>");
    let provides = field("provides", meta.get_provides())?;
    let requires = field("requirements", meta.get_requires())?;
    let primary = format!(
        "<package type=\"rpm\">\n  <name>{name}</name>\n  <arch>{arch}</arch>\n  {evr}\n  <checksum type=\"sha256\" pkgid=\"YES\">{pkgid}</checksum>\n  <summary>{}</summary>\n  <description>{}</description>\n  <packager>{}</packager>\n  <url>{}</url>\n  <time file=\"{build}\" build=\"{build}\"/>\n  <size package=\"{}\" installed=\"{installed}\" archive=\"{}\"/>\n  <location href=\"../../{POOL_DIR}/{file}\"/>\n  <format>\n    <rpm:license>{}</rpm:license>\n    <rpm:vendor>{}</rpm:vendor>\n    <rpm:group>{}</rpm:group>\n    <rpm:buildhost>{}</rpm:buildhost>\n    <rpm:sourcerpm>{}</rpm:sourcerpm>\n    <rpm:header-range start=\"{start}\" end=\"{end}\"/>\n{}{}{in_primary_files}  </format>\n</package>\n",
        get("summary", meta.get_summary())?,
        get("description", meta.get_description())?,
        get("packager", meta.get_packager())?,
        get("url", meta.get_url())?,
        bytes.len(),
        parts.payload.len(),
        get("license", meta.get_license())?,
        meta.get_vendor().map(escape).unwrap_or_default(),
        meta.get_group().map(escape).unwrap_or_default(),
        get("build host", meta.get_build_host())?,
        meta.get_source_rpm()
            .map(escape)
            .unwrap_or_default()
            .replace("(none)", ""),
        dependencies("provides", provides),
        dependencies("requires", requires),
    );
    let head = format!("<package pkgid=\"{pkgid}\" name=\"{name}\" arch=\"{arch}\">\n  {evr}\n");
    Ok(Package {
        version: numbers(&version),
        name,
        arch,
        pkgid,
        href: format!("../../{POOL_DIR}/{file}"),
        primary,
        filelists: format!("{head}{in_filelists}</package>\n"),
        other: format!("{head}</package>\n"),
    })
}

/// The text between `<tag>` (or `<tag attribute...>`) and `</tag>` in `chunk`.
fn text_of<'a>(chunk: &'a str, tag: &str) -> Option<&'a str> {
    let at = chunk
        .match_indices(&format!("<{tag}"))
        .find(|(i, _)| matches!(chunk.as_bytes().get(i + tag.len() + 1), Some(b' ' | b'>')))?
        .0;
    let open = chunk[at..].find('>')? + at + 1;
    let close = chunk[open..].find(&format!("</{tag}>"))?;
    Some(&chunk[open..open + close])
}

/// The value of attribute `name` of the first element `<element ...>` in `chunk`.
fn attribute<'a>(chunk: &'a str, element: &str, name: &str) -> Option<&'a str> {
    let at = chunk.find(&format!("<{element} "))?;
    let end = chunk[at..].find('>')? + at;
    let tag = &chunk[at..end];
    let from = tag.find(&format!(" {name}=\""))? + name.len() + 3;
    let len = tag[from..].find('"')?;
    Some(&tag[from..from + len])
}

/// The `<package>` elements of a metadata file, each with its line break: the text between the
/// root element's start tag and its end.
fn chunks<'a>(xml: &'a str, root: &str) -> Result<Vec<&'a str>, Error> {
    let bad = |why: &str| Error::Failed(format!("{root}.xml: {why}"));
    let mut rest = xml
        .split_once(&format!("<{root} "))
        .and_then(|(_, r)| r.split_once(">\n"))
        .ok_or_else(|| bad("no root element"))?
        .1;
    let mut out = Vec::new();
    while !rest.starts_with(&format!("</{root}>")) {
        let end = rest
            .find("</package>\n")
            .ok_or_else(|| bad("a package without its end"))?
            + 11;
        if !rest.starts_with("<package ") {
            return Err(bad("something other than a package"));
        }
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    Ok(out)
}

/// The packages of the three files of a repository.
///
/// # Errors
/// A file that is not a metadata file of the form `createrepo` writes, or a package of `primary.xml`
/// that `filelists.xml` or `other.xml` has no chunk for.
pub fn parse(primary: &str, filelists: &str, other: &str) -> Result<Vec<Package>, Error> {
    let find = |all: &[&str], pkgid: &str| -> Option<String> {
        all.iter()
            .find(|c| attribute(c, "package", "pkgid") == Some(pkgid))
            .map(|c| (*c).to_owned())
    };
    let (files, others) = (chunks(filelists, "filelists")?, chunks(other, "otherdata")?);
    let mut out = Vec::new();
    for chunk in chunks(primary, "metadata")? {
        let missing =
            |what: &str| Error::Failed(format!("primary.xml: a package without its {what}"));
        let pkgid = text_of(chunk, "checksum").ok_or_else(|| missing("checksum"))?;
        let ver = attribute(chunk, "version", "ver").ok_or_else(|| missing("version"))?;
        out.push(Package {
            name: text_of(chunk, "name")
                .ok_or_else(|| missing("name"))?
                .to_owned(),
            arch: text_of(chunk, "arch")
                .ok_or_else(|| missing("architecture"))?
                .to_owned(),
            version: numbers(ver),
            href: attribute(chunk, "location", "href")
                .ok_or_else(|| missing("location"))?
                .to_owned(),
            primary: chunk.to_owned(),
            filelists: find(&files, pkgid).ok_or_else(|| {
                Error::Failed(format!(
                    "filelists.xml has no package {pkgid}, which primary.xml lists"
                ))
            })?,
            other: find(&others, pkgid).ok_or_else(|| {
                Error::Failed(format!(
                    "other.xml has no package {pkgid}, which primary.xml lists"
                ))
            })?,
            pkgid: pkgid.to_owned(),
        });
    }
    Ok(out)
}

/// The three metadata files of `packages`.
pub fn render(packages: &[Package]) -> [String; 3] {
    let n = packages.len();
    let file = |root: &str, chunk: fn(&Package) -> &str| {
        let body: String = packages.iter().map(chunk).collect();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{root} packages=\"{n}\">\n{body}</{}>\n",
            root.split(' ').next().unwrap_or(root)
        )
    };
    [
        file(
            "metadata xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\"",
            |p| &p.primary,
        ),
        file(
            "filelists xmlns=\"http://linux.duke.edu/metadata/filelists\"",
            |p| &p.filelists,
        ),
        file(
            "otherdata xmlns=\"http://linux.duke.edu/metadata/other\"",
            |p| &p.other,
        ),
    ]
}

/// What a merge came to.
#[derive(Debug)]
pub struct Merged {
    pub packages: Vec<Package>,
    pub added: usize,
    pub dropped: usize,
}

/// Merges `add` into `existing`: monotonic, bounded, and only for the packages of `add`.
///
/// A package at a version older than the newest listed under its name and architecture is refused
/// (the channel does not move down); the same version with the same digest is a no-op and with
/// another is refused. Of each name and architecture in `add`, only the newest `keep` versions
/// stay listed; every other package, and its place in the order, is left as it is.
///
/// # Errors
/// `keep` is 0, a version moves down or is listed with other bytes, or a listed version of a
/// package in `add` is not `<major>.<minor>.<patch>`.
pub fn merge(existing: Vec<Package>, add: &[Package], keep: usize) -> Result<Merged, Error> {
    if keep == 0 {
        return Err(Error::Usage("--keep is 0; it must be 1 or more".to_owned()));
    }
    let same = |a: &Package, b: &Package| a.name == b.name && a.arch == b.arch;
    let mut all = existing;
    let mut added = 0;
    for new in add {
        let mut newest = None;
        for listed in all.iter().filter(|p| same(p, new)) {
            let version = listed.version.ok_or_else(|| {
                Error::Failed(format!(
                    "{} {} lists the version of {} as other than X.Y.Z",
                    listed.arch, listed.href, listed.name
                ))
            })?;
            newest = newest.max(Some(version));
            if Some(version) == new.version && listed.pkgid != new.pkgid {
                return Err(Error::Failed(format!(
                    "refused: {} is listed at this version with other bytes",
                    new.name
                )));
            }
        }
        match newest.map(|v| new.version.cmp(&Some(v))) {
            Some(Ordering::Less) => {
                return Err(Error::Failed(format!(
                    "refused: {} ({}) is older than the newest listed, which would move the channel down",
                    new.name, new.arch
                )));
            }
            Some(Ordering::Equal) => {}
            _ => {
                all.push(new.clone());
                added += 1;
            }
        }
    }
    let mut dropped = 0;
    for new in add {
        let mut versions: Vec<(Option<Version>, String)> = all
            .iter()
            .filter(|p| same(p, new))
            .map(|p| (p.version, p.pkgid.clone()))
            .collect();
        versions.sort_by(|a, b| b.cmp(a));
        let old: Vec<String> = versions.into_iter().skip(keep).map(|(_, id)| id).collect();
        let before = all.len();
        all.retain(|p| !(same(p, new) && old.contains(&p.pkgid)));
        dropped += before - all.len();
    }
    Ok(Merged {
        packages: all,
        added,
        dropped,
    })
}

/// The gzip of `bytes`: no name, no time, so the same bytes give the same file.
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    // Writing to a vector does not fail.
    let _ = encoder.write_all(bytes);
    encoder.finish().unwrap_or_default()
}

/// The bytes a gzip holds.
///
/// # Errors
/// Bytes that are not gzip.
pub fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| failed("gunzip", e))?;
    Ok(out)
}

/// The files of the `repodata` directory of `packages`: the three gzipped files, named by their
/// digest, and `repomd.xml`, as `(file name, bytes)`; `date` is the revision and the timestamp.
pub fn files(packages: &[Package], date: u64) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut repomd = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<repomd xmlns=\"http://linux.duke.edu/metadata/repo\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\">\n  <revision>{date}</revision>\n"
    );
    for (kind, xml) in ["primary", "filelists", "other"]
        .into_iter()
        .zip(render(packages))
    {
        let gz = gzip(xml.as_bytes());
        let (sum, open) = (Sha256::of(&gz), Sha256::of(xml.as_bytes()));
        let name = format!("{sum}-{kind}.xml.gz");
        repomd.push_str(&format!(
            "  <data type=\"{kind}\">\n    <checksum type=\"sha256\">{sum}</checksum>\n    <open-checksum type=\"sha256\">{open}</open-checksum>\n    <location href=\"repodata/{name}\"/>\n    <timestamp>{date}</timestamp>\n    <size>{}</size>\n    <open-size>{}</open-size>\n  </data>\n",
            gz.len(),
            xml.len()
        ));
        out.push((name, gz));
    }
    repomd.push_str("</repomd>\n");
    out.push(("repomd.xml".to_owned(), repomd.into_bytes()));
    out
}

/// What a `repomd.xml` lists: `(type, file name, sha256 of the file)`.
///
/// # Errors
/// A `repomd.xml` with a `<data>` element that has no type, location or checksum, or a location
/// outside `repodata/`.
pub fn listed(repomd: &str) -> Result<Vec<(String, String, String)>, Error> {
    let mut out = Vec::new();
    for part in repomd.split("<data type=\"").skip(1) {
        let bad =
            |what: &str| Error::Failed(format!("repomd.xml: a data element without its {what}"));
        let kind = part.split('"').next().ok_or_else(|| bad("type"))?;
        let href = attribute(part, "location", "href").ok_or_else(|| bad("location"))?;
        let sum = text_of(part, "checksum").ok_or_else(|| bad("checksum"))?;
        let name = href
            .strip_prefix("repodata/")
            .filter(|n| !n.is_empty() && !n.contains('/') && !n.starts_with('.'))
            .ok_or_else(|| {
                Error::Failed(format!(
                    "repomd.xml: the location {href:?} is not a file of repodata/"
                ))
            })?;
        out.push((kind.to_owned(), name.to_owned(), sum.to_owned()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;

/// A package of another product, as `createrepo_c` writes it, for the tests of the verbs.
#[cfg(test)]
pub(crate) fn tests_foreign(name: &str, ver: &str) -> Package {
    tests::foreign(name, ver)
}

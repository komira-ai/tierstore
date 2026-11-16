//! The signed-installable `.pkg` of tierstore for a Mac, which hostkeeper's `macos-pkg` installer
//! installs (hostkeeper's `docs/design/packages-repositories.md`, section 4.7): `release macos-pkg`
//! builds it and `release check-macos-pkg` reads it back; `release check-pkg-signature`
//! ([`super::pkg_signature`]) holds the signed file to its signer.
//!
//! The file is `tierstore-<version>-macos-arm64.pkg`, a distribution pkg: `pkgbuild` builds the
//! component `tierstore.pkg` from the payload with no scripts directory, and `productbuild` wraps
//! it as the one package of the product, unsigned (the release machinery signs it afterwards). The
//! component's identifier is [`IDENTIFIER`], its version is the release's `<X.Y.Z>`, its install
//! location is `/`, and every entry is owned by root (`--ownership recommended`). The tools run
//! through the port [`BuildPkg`], which [`PkgTools`] implements with the three programs of macOS
//! and tests fake, as none of them exists on Linux.
//!
//! The payload is exactly these entries, each owned by root and group wheel, directories 0755:
//! the binary `/Library/Application Support/tierstore/bin/tierstore` (0755, the given binary's
//! bytes) and the job `/Library/LaunchDaemons/tierstore.plist` (0644, the file
//! `packaging/macos-pkg/tierstore.plist`), with the directories above them. There is no
//! `postinstall` and no script at all: hostkeeper's update hook (`tierstore update-hook`)
//! bootstraps the job at its `cutover`.
//!
//! The check takes the directory `pkgutil --expand` made of the product and refuses anything but
//! what the build writes: it holds exactly the file `Distribution` (the one component, no
//! installer script or check) and the directory `tierstore.pkg`, which holds exactly `Bom`,
//! `PackageInfo` (the identifier, the version, the install location `/`, no script, no bundle) and
//! `Payload` (exactly the entries above, with their modes and bytes, owned by root, group wheel
//! or admin; the plist parses with the label `tierstore` and a program that is a file of the
//! payload).
//!
//! [`manifest_json`] is the hostkeeper manifest of this pkg, as the coordinator's targets name
//! it for a Mac.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use super::hostkeeper_package::is_plain_version;
use super::{BINARY, Error};
use crate::fetch::Sha256;
use crate::package::Platform;

pub(crate) mod distribution;

use distribution::{COMPONENT, DISTRIBUTION, open_tag, product_problems};

/// The component's package identifier: the name `pkgutil` knows the receipt by.
pub const IDENTIFIER: &str = "com.komira.tierstore";

/// The job's label, the file stem of its plist, and the manifest's run entry.
pub const LABEL: &str = "tierstore";

/// The binary's path from `/`, in the payload and in the manifest's hook command.
pub const BINARY_PATH: &str = "/Library/Application Support/tierstore/bin/tierstore";

/// The job's plist path from `/`.
pub const PLIST_PATH: &str = "/Library/LaunchDaemons/tierstore.plist";

/// The configuration file the update hook has the staged release check: the Mac's, which the
/// pkg does not install.
pub const CONFIG_FILE: &str = "/usr/local/tierstore/etc/tierstore.toml";

/// The plist the pkg installs.
pub const PLIST: &str = include_str!("../../../../packaging/macos-pkg/tierstore.plist");

const DIR_MODE: u32 = 0o755;
const BINARY_MODE: u32 = 0o755;
const PLIST_MODE: u32 = 0o644;

/// The install location of the component.
const INSTALL_LOCATION: &str = "/";

/// The files of an expanded component: no `Scripts`.
const COMPONENT_FILES: [&str; 3] = ["Bom", "PackageInfo", "Payload"];

/// The groups an entry may have: wheel (0) and admin (80).
const GROUPS: [u32; 2] = [0, 80];

/// The file-type bits of a cpio mode, and the two types a payload holds.
const TYPE_BITS: u32 = 0o170_000;
const TYPE_DIR: u32 = 0o040_000;
const TYPE_FILE: u32 = 0o100_000;

/// The odc cpio header's magic, its length, and the name of its last entry.
const ODC_MAGIC: &[u8; 6] = b"070707";
const ODC_HEADER: usize = 76;
const TRAILER: &str = "TRAILER!!!";

/// The file name of the `.pkg` of the release `version`.
pub fn file_name(version: &str) -> String {
    format!("{BINARY}-{version}-{}.pkg", Platform::MacosArm64.name())
}

/// The hostkeeper manifest of the pkg, in the proto3 JSON form of hostkeeper's release proto, as
/// [`super::desired_apt::entry`] writes the manifest of the apt entry: the update hook is the
/// installed binary's `update-hook`, with the bounds and deadlines of the archive manifest
/// (`packaging/hostkeeper/manifest.toml.in`), the one run label, the health URL and the params
/// the hook reads (`service_manager` `launchd` and its `launchd_label`).
pub fn manifest_json(min_hostkeeper: &str) -> Value {
    json!({
        "package": IDENTIFIER,
        "hook": {
            "command": [BINARY_PATH, "update-hook"],
            "boot": "restore",
            "timeoutSecs": {
                "prepare": "0", "cutover": "300", "verify": "1000",
                "rollback": "600", "cleanup": "1300", "uninstall": "300",
            },
            "verifyDeadlineSecs": "1600",
            "promotionCeilingSecs": "3600",
            "cleanupRetries": 3,
        },
        "run": [LABEL],
        "healthUrl": "http://127.0.0.1:9300/readyz",
        "params": {
            "config_file": {"text": CONFIG_FILE},
            "service_manager": {"text": "launchd"},
            "launchd_label": {"text": LABEL},
        },
        "minHostkeeper": min_hostkeeper,
        "installerBounds": {"stageSecs": "600", "activateSecs": "300", "removeSecs": "300"},
    })
}

/// What one entry of the payload must be.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Want {
    Dir,
    File { mode: u32, bytes: Vec<u8> },
}

/// Every entry of the payload by its path from `/` without the leading `/` (`""` is `/`), for
/// the program `binary`.
fn payload(binary: Vec<u8>) -> BTreeMap<String, Want> {
    let mut want = BTreeMap::new();
    want.insert(String::new(), Want::Dir);
    let files = [
        (BINARY_PATH, BINARY_MODE, binary),
        (PLIST_PATH, PLIST_MODE, PLIST.as_bytes().to_vec()),
    ];
    for (path, mode, bytes) in files {
        let path = path.trim_start_matches('/').to_owned();
        let mut parent = path.as_str();
        while let Some((dir, _)) = parent.rsplit_once('/') {
            want.insert(dir.to_owned(), Want::Dir);
            parent = dir;
        }
        want.insert(path, Want::File { mode, bytes });
    }
    want
}

/// The tools that make a pkg: the port to `pkgbuild`, `productbuild` and `pkgutil`.
pub trait BuildPkg {
    /// `pkgbuild --root <root> --identifier <identifier> --version <version> --install-location /
    /// --ownership recommended <pkg>`: the component pkg, with no scripts.
    fn build_pkg(
        &self,
        root: &Path,
        identifier: &str,
        version: &str,
        pkg: &Path,
    ) -> Result<(), Error>;

    /// `productbuild --package <component> --identifier <identifier> --version <version>
    /// <product>`: the unsigned product.
    fn build_product(
        &self,
        component: &Path,
        identifier: &str,
        version: &str,
        product: &Path,
    ) -> Result<(), Error>;

    /// What `pkgutil --check-signature <pkg>` prints; an error when it exits other than 0.
    fn check_signature(&self, pkg: &Path) -> Result<String, Error>;
}

/// The programs of [`BuildPkg`]: each is run by the name it has, or by a path.
pub struct PkgTools {
    pub pkgbuild: PathBuf,
    pub productbuild: PathBuf,
    pub pkgutil: PathBuf,
}

impl PkgTools {
    /// The three programs of macOS, found on `PATH`.
    pub fn system() -> PkgTools {
        PkgTools {
            pkgbuild: "pkgbuild".into(),
            productbuild: "productbuild".into(),
            pkgutil: "pkgutil".into(),
        }
    }
}

/// `path` as UTF-8, for the command line of `program`.
fn utf8(path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Failed(format!("{} is not UTF-8", path.display())))
}

/// Runs `program` with `args`: its stdout, or the failure naming the command line and what it
/// printed.
fn run(program: &Path, args: &[String]) -> Result<String, Error> {
    let what = format!("{} {}", program.display(), args.join(" "));
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(Error::spawn(what.clone()))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(Error::Failed(format!(
        "{what}: {}\n{}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

/// The arguments of `pkgbuild` for [`BuildPkg::build_pkg`].
fn pkgbuild_args(
    root: &Path,
    identifier: &str,
    version: &str,
    pkg: &Path,
) -> Result<Vec<String>, Error> {
    Ok(vec![
        "--root".to_owned(),
        utf8(root)?,
        "--identifier".to_owned(),
        identifier.to_owned(),
        "--version".to_owned(),
        version.to_owned(),
        "--install-location".to_owned(),
        INSTALL_LOCATION.to_owned(),
        "--ownership".to_owned(),
        "recommended".to_owned(),
        utf8(pkg)?,
    ])
}

/// The arguments of `productbuild` for [`BuildPkg::build_product`].
fn productbuild_args(
    component: &Path,
    identifier: &str,
    version: &str,
    product: &Path,
) -> Result<Vec<String>, Error> {
    Ok(vec![
        "--package".to_owned(),
        utf8(component)?,
        "--identifier".to_owned(),
        identifier.to_owned(),
        "--version".to_owned(),
        version.to_owned(),
        utf8(product)?,
    ])
}

impl BuildPkg for PkgTools {
    fn build_pkg(
        &self,
        root: &Path,
        identifier: &str,
        version: &str,
        pkg: &Path,
    ) -> Result<(), Error> {
        run(
            &self.pkgbuild,
            &pkgbuild_args(root, identifier, version, pkg)?,
        )
        .map(drop)
    }

    fn build_product(
        &self,
        component: &Path,
        identifier: &str,
        version: &str,
        product: &Path,
    ) -> Result<(), Error> {
        let args = productbuild_args(component, identifier, version, product)?;
        run(&self.productbuild, &args).map(drop)
    }

    fn check_signature(&self, pkg: &Path) -> Result<String, Error> {
        run(&self.pkgutil, &["--check-signature".to_owned(), utf8(pkg)?])
    }
}

/// What `release macos-pkg` builds the pkg from.
pub struct Request<'a> {
    pub version: &'a str,
    /// The macos-arm64 tierstore binary.
    pub binary: &'a Path,
    /// A directory to lay the payload out in (`root`) and to write the component to; it must not
    /// exist.
    pub stage: &'a Path,
    /// The directory the product is written to.
    pub out: &'a Path,
}

/// Makes the directory `at` with [`DIR_MODE`].
fn make_dir(at: &Path) -> Result<(), Error> {
    std::fs::create_dir(at).map_err(Error::spawn(format!("create {}", at.display())))?;
    set_mode(at, DIR_MODE)
}

/// Sets the mode of `at`.
fn set_mode(at: &Path, mode: u32) -> Result<(), Error> {
    std::fs::set_permissions(at, std::fs::Permissions::from_mode(mode))
        .map_err(Error::spawn(format!("set the mode of {}", at.display())))
}

/// Refuses a `--version` that is not `<major>.<minor>.<patch>`.
fn version_usage(version: &str) -> Result<(), Error> {
    if is_plain_version(version) {
        return Ok(());
    }
    Err(Error::Usage(format!(
        "--version {version:?} is not <major>.<minor>.<patch>"
    )))
}

/// Lays the payload out under `request.stage`'s `root`, builds the component from it with no
/// scripts, and wraps it as the product in `request.out`; the lines to print.
pub fn build(tools: &dyn BuildPkg, request: &Request) -> Result<Vec<String>, Error> {
    version_usage(request.version)?;
    let binary = std::fs::read(request.binary)
        .map_err(Error::spawn(format!("read {}", request.binary.display())))?;
    let stage = request.stage;
    if std::fs::symlink_metadata(stage).is_ok() {
        return Err(Error::Failed(format!(
            "{} exists: the pkg's tree is laid out in a new directory, so it holds nothing else",
            stage.display()
        )));
    }
    make_dir(stage)?;
    let root = stage.join("root");
    // In path order, so each directory is made before what it holds; `""` is the payload's root
    // itself (the components drop the `/` that joining an empty path leaves).
    for (path, want) in payload(binary) {
        let at: PathBuf = root.join(&path).components().collect();
        let mode = match &want {
            Want::Dir => {
                make_dir(&at)?;
                continue;
            }
            Want::File { mode, bytes } => {
                std::fs::write(&at, bytes)
                    .map_err(Error::spawn(format!("write {}", at.display())))?;
                *mode
            }
        };
        set_mode(&at, mode)?;
    }
    std::fs::create_dir_all(request.out)
        .map_err(Error::spawn(format!("create {}", request.out.display())))?;
    let component = stage.join(COMPONENT);
    tools.build_pkg(&root, IDENTIFIER, request.version, &component)?;
    let name = file_name(request.version);
    let product = request.out.join(&name);
    tools.build_product(&component, IDENTIFIER, request.version, &product)?;
    let bytes =
        std::fs::read(&product).map_err(Error::spawn(format!("read {}", product.display())))?;
    Ok(vec![format!("{name}: sha256 {}", Sha256::of(&bytes))])
}

/// One entry of a cpio archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpioEntry {
    pub(crate) name: String,
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) data: Vec<u8>,
}

/// The number written as `len` octal digits at `at` of `bytes`, which holds them.
fn octal(bytes: &[u8], at: usize, len: usize) -> Result<u64, String> {
    let field = &bytes[at..at + len];
    let digits = field.iter().all(|b| matches!(b, b'0'..=b'7'));
    let text = std::str::from_utf8(field).ok().filter(|_| digits);
    text.and_then(|t| u64::from_str_radix(t, 8).ok())
        .ok_or_else(|| format!("the header field at byte {at} is not {len} octal digits"))
}

/// The bytes `bytes[at..at + len]`, or why the archive ends before them.
fn take(bytes: &[u8], at: usize, len: u64, what: &str) -> Result<Vec<u8>, String> {
    usize::try_from(len)
        .ok()
        .and_then(|len| at.checked_add(len))
        .and_then(|end| bytes.get(at..end))
        .map(<[u8]>::to_vec)
        .ok_or_else(|| format!("the archive ends inside the {what} that starts at byte {at}"))
}

/// The entries of the portable (`odc`) cpio archive `bytes`, up to its trailer; anything after
/// the trailer (the padding of a block) is not read.
pub(crate) fn read_odc(bytes: &[u8]) -> Result<Vec<CpioEntry>, String> {
    let mut entries = Vec::new();
    let mut at = 0;
    loop {
        if bytes.len() - at < ODC_HEADER {
            return Err(format!(
                "the archive ends at byte {} without its trailer",
                bytes.len()
            ));
        }
        if &bytes[at..at + 6] != ODC_MAGIC {
            return Err(format!(
                "the header at byte {at} starts {:?}, not the odc magic 070707",
                String::from_utf8_lossy(&bytes[at..at + 6])
            ));
        }
        let field = |offset: usize, len: usize| octal(bytes, at + offset, len);
        let mode = field(18, 6)?;
        let uid = field(24, 6)?;
        let gid = field(30, 6)?;
        let name_size = field(59, 6)?;
        let file_size = field(65, 11)?;
        let name_at = at + ODC_HEADER;
        let name = take(bytes, name_at, name_size, "name")?;
        let name = match name.split_last() {
            Some((0, name)) => String::from_utf8(name.to_vec())
                .map_err(|_| format!("the name at byte {name_at} is not UTF-8"))?,
            _ => return Err(format!("the name at byte {name_at} does not end in NUL")),
        };
        if name == TRAILER {
            return Ok(entries);
        }
        let data_at = name_at + name.len() + 1;
        let data = take(bytes, data_at, file_size, "data")?;
        at = data_at + data.len();
        // Six octal digits hold at most 0o777777, so each fits a u32.
        let small = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        entries.push(CpioEntry {
            name,
            mode: small(mode),
            uid: small(uid),
            gid: small(gid),
            data,
        });
    }
}

/// The path of a payload entry from the install location: `.` is `""`, `./x` is `x`; any other
/// name is kept, and so matches no entry of the payload.
fn payload_path(name: &str) -> &str {
    match name {
        "." => "",
        _ => name.strip_prefix("./").unwrap_or(name),
    }
}

/// The attributes of the `<pkg-info ...>` element of `text`, or none when it has no such
/// element or its attributes are not `name="value"` pairs.
fn pkg_info_attributes(text: &str) -> Option<BTreeMap<String, String>> {
    let start = text.find("<pkg-info")? + "<pkg-info".len();
    open_tag(&text[start..]).map(|(attributes, ..)| attributes)
}

/// What is wrong with the `PackageInfo` text `text` of the build `version`, one line each.
fn package_info_problems(text: &str, version: &str, problems: &mut Vec<String>) {
    let Some(attributes) = pkg_info_attributes(text) else {
        return problems.push("PackageInfo has no readable <pkg-info> element".to_owned());
    };
    for (name, want) in [
        ("identifier", IDENTIFIER),
        ("version", version),
        ("install-location", INSTALL_LOCATION),
    ] {
        match attributes.get(name) {
            Some(value) if value == want => {}
            Some(value) => {
                problems.push(format!("PackageInfo: {name} is {value:?}, not {want:?}"));
            }
            None => problems.push(format!("PackageInfo has no {name}")),
        }
    }
    for script in ["scripts", "preinstall", "postinstall"] {
        if text.contains(&format!("<{script}")) {
            problems.push(format!(
                "PackageInfo names a {script} element: tierstore's pkg has no script"
            ));
        }
    }
    for bundle in ["<bundle ", "<bundle>", "<bundle/"] {
        if text.contains(bundle) {
            problems.push("PackageInfo names a bundle: tierstore's pkg holds none".to_owned());
            break;
        }
    }
}

/// The value element after the `<key>` named `key` in the plist dictionary `dict`.
fn dict_values<'a>(dict: roxmltree::Node<'a, 'a>, key: &str) -> Option<roxmltree::Node<'a, 'a>> {
    let items: Vec<_> = dict.children().filter(|n| n.is_element()).collect();
    items
        .chunks(2)
        .find(|pair| pair[0].tag_name().name() == "key" && pair[0].text() == Some(key))
        .and_then(|pair| pair.get(1).copied())
}

/// What is wrong with the plist `bytes` of `/path`, one line each: it parses as a dictionary
/// with the label [`LABEL`] and a program that is a file of the payload (`files`, the payload's
/// file paths from `/`).
fn plist_problems(path: &str, bytes: &[u8], files: &[String], problems: &mut Vec<String>) {
    let what = format!("/{path}");
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let text = String::from_utf8_lossy(bytes);
    let doc = match roxmltree::Document::parse_with_options(&text, options) {
        Ok(doc) => doc,
        Err(e) => return problems.push(format!("{what} does not parse as a plist: {e}")),
    };
    let dict = doc
        .root_element()
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "dict");
    let Some(dict) = dict else {
        return problems.push(format!("{what} is not a plist dictionary"));
    };
    let label = dict_values(dict, "Label").and_then(|n| n.text());
    if label != Some(LABEL) {
        problems.push(format!("{what}: the label is {label:?}, not {LABEL:?}"));
    }
    let program = dict_values(dict, "ProgramArguments")
        .and_then(|array| array.children().find(roxmltree::Node::is_element))
        .and_then(|n| n.text());
    if !program.is_some_and(|p| files.iter().any(|f| *f == p.trim_start_matches('/'))) {
        problems.push(format!(
            "{what}: the job runs {program:?}, which is not a file of the payload"
        ));
    }
}

/// What is wrong with the payload `entries` against `want`, one line each.
fn payload_problems(
    entries: &[CpioEntry],
    mut want: BTreeMap<String, Want>,
    problems: &mut Vec<String>,
) {
    let files: Vec<String> = want
        .iter()
        .filter(|(_, w)| matches!(w, Want::File { .. }))
        .map(|(p, _)| p.clone())
        .collect();
    for entry in entries {
        let path = payload_path(&entry.name);
        let what = format!("/{path}");
        let (kind, mode) = (entry.mode & TYPE_BITS, entry.mode & !TYPE_BITS);
        let Some(wanted) = want.remove(path) else {
            problems.push(format!(
                "{what} ({:?}) is in the payload: not a file of tierstore's pkg",
                entry.name
            ));
            continue;
        };
        if entry.uid != 0 || !GROUPS.contains(&entry.gid) {
            problems.push(format!(
                "{what}: owned by uid {} and gid {}, not root and wheel or admin",
                entry.uid, entry.gid
            ));
        }
        let (want_kind, want_mode) = match &wanted {
            Want::Dir => (TYPE_DIR, DIR_MODE),
            Want::File { mode, .. } => (TYPE_FILE, *mode),
        };
        if kind != want_kind {
            problems.push(format!("{what}: of type {kind:o}, not {want_kind:o}"));
            continue;
        }
        if mode != want_mode {
            problems.push(format!("{what}: mode {mode:o}, not {want_mode:o}"));
        }
        if let Want::File { bytes, .. } = &wanted {
            if entry.data != *bytes {
                problems.push(format!("{what}: not the bytes release macos-pkg writes"));
            }
            if path == PLIST_PATH.trim_start_matches('/') {
                plist_problems(path, &entry.data, &files, problems);
            }
        }
    }
    problems.extend(
        want.keys()
            .map(|path| format!("missing /{path} in the payload")),
    );
}

/// The entries of the `Payload` of the expanded pkg `expanded`, a gzip-compressed portable cpio
/// archive; the inner `Err` says why its bytes are not such an archive, the outer one that the
/// file could not be opened.
fn read_payload(expanded: &Path) -> Result<Result<Vec<CpioEntry>, String>, Error> {
    let name = "Payload";
    let path = expanded.join(name);
    let file =
        std::fs::File::open(&path).map_err(Error::spawn(format!("read {}", path.display())))?;
    let mut cpio = Vec::new();
    Ok(flate2::read::GzDecoder::new(file)
        .read_to_end(&mut cpio)
        .map_err(|e| format!("{name} is not gzip data: {e}"))
        .and_then(|_| read_odc(&cpio).map_err(|e| format!("{name}: {e}"))))
}

/// One entry of a directory of an expanded pkg: its name and whether it is a regular file and
/// whether it is a directory; a symbolic link is neither (`DirEntry::file_type` does not follow
/// it).
struct Listed {
    name: String,
    file: bool,
    dir: bool,
}

/// The entries of `dir`, sorted by name.
fn list(dir: &Path) -> Result<Vec<Listed>, Error> {
    let mut listed: Vec<Listed> = std::fs::read_dir(dir)
        .and_then(|read| read.collect::<Result<Vec<_>, _>>())
        .map_err(Error::spawn(format!("read {}", dir.display())))?
        .iter()
        .map(|e| {
            let kind = e.file_type().ok();
            Listed {
                name: e.file_name().to_string_lossy().into_owned(),
                file: kind.is_some_and(|k| k.is_file()),
                dir: kind.is_some_and(|k| k.is_dir()),
            }
        })
        .collect();
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(listed)
}

/// Checks that the directory `dir` of an expanded pkg holds exactly the regular files `files` and
/// the directories `dirs`: every other entry is a problem that says the directory `holds only`
/// `only`, and every one missing is a problem. The names that are there, as they must be.
fn layer(
    dir: &Path,
    files: &[&str],
    dirs: &[&str],
    only: &str,
    problems: &mut Vec<String>,
) -> Result<Vec<String>, Error> {
    let listed = list(dir)?;
    let present = |want: &str| {
        listed.iter().any(|e| {
            (e.name == want) & ((e.file & files.contains(&want)) | (e.dir & dirs.contains(&want)))
        })
    };
    for entry in &listed {
        if !present(&entry.name) {
            problems.push(format!("{}: {only}", entry.name));
        }
    }
    let wanted = files.iter().chain(dirs);
    for missing in wanted.clone().filter(|name| !present(name)) {
        problems.push(format!("missing {missing}"));
    }
    Ok(wanted
        .filter(|name| present(name))
        .map(|name| (*name).to_owned())
        .collect())
}

/// The `release check-macos-pkg` verb: checks the product `pkgutil --expand` expanded into
/// `expanded` against the build `version` and the program `binary`, listing every problem.
pub fn check(expanded: &Path, version: &str, binary: &Path) -> Result<Vec<String>, Error> {
    version_usage(version)?;
    let bytes =
        std::fs::read(binary).map_err(Error::spawn(format!("read {}", binary.display())))?;
    let want = payload(bytes);
    let mut problems = Vec::new();
    let only = format!(
        "an expanded product of tierstore's pkg holds only the file {DISTRIBUTION} and the \
         directory {COMPONENT}"
    );
    let present = layer(
        expanded,
        &[DISTRIBUTION],
        &[COMPONENT],
        &only,
        &mut problems,
    )?;
    let read = |path: PathBuf| {
        std::fs::read(&path).map_err(Error::spawn(format!("read {}", path.display())))
    };
    if present.iter().any(|p| p == DISTRIBUTION) {
        let text = read(expanded.join(DISTRIBUTION))?;
        product_problems(
            &String::from_utf8_lossy(&text),
            IDENTIFIER,
            COMPONENT,
            version,
            &mut problems,
        );
    }
    if present.iter().any(|p| p == COMPONENT) {
        let component = expanded.join(COMPONENT);
        let only = "an expanded component of tierstore's pkg holds only the files Bom, \
                    PackageInfo and Payload: no Scripts, since it has no script";
        let parts = layer(&component, &COMPONENT_FILES, &[], only, &mut problems)?;
        if parts.iter().any(|p| p == "PackageInfo") {
            let text = read(component.join("PackageInfo"))?;
            package_info_problems(&String::from_utf8_lossy(&text), version, &mut problems);
        }
        if parts.iter().any(|p| p == "Payload") {
            match read_payload(&component)? {
                Err(why) => problems.push(why),
                Ok(entries) => payload_problems(&entries, want, &mut problems),
            }
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(vec![format!(
        "check-macos-pkg: {} is a product of the one component {COMPONENT} ({IDENTIFIER} \
         {version}), with no script, which installs exactly {BINARY_PATH} ({BINARY_MODE:o}) \
         and {PLIST_PATH} ({PLIST_MODE:o}), owned by root",
        expanded.display(),
    )])
}

#[cfg(test)]
pub(crate) mod tests;

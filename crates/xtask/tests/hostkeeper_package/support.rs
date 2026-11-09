//! What the tests of the hostkeeper package share: a stand-in binary, scratch directories, the
//! package of each platform, and readers of its bytes (the zstd frame header, the tar entries) and
//! of the texts inside it (the manifest as TOML, the plist as XML, the unit's assignments).

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use hostkeeper_config::daemon::Sha256Hex;
use hostkeeper_config::names::{ServiceName, Version};
use hostkeeper_unpack::{HostFs, Limits, Owner, UnpackError, UnpackOrder, Unpacked};
use sha2::{Digest, Sha256};
use tierstore_testkit::dir::TestDir;
use xtask::package::Platform;
use xtask::release::hostkeeper_package::archive;

/// The version every package of the tests is built as: what `tierstore --version` prints after
/// the program's name.
pub const VERSION: &str = "0.4.0+0123456789ab";

/// The oldest hostkeeper the tests' packages need: the version of the hostkeeper crates the tests
/// read them with, so their loader takes it.
pub const MIN_HOSTKEEPER: &str = "0.1.0";

/// Where hostkeeper keeps every package on a host; a unit names the package's files only through
/// `<PACKAGES>/tierstore/current/`.
pub const PACKAGES: &str = "/var/lib/hostkeeper/packages";

/// The program every unit runs: the package's binary through hostkeeper's `current` link.
pub const CURRENT_BINARY: &str = "/var/lib/hostkeeper/packages/tierstore/current/bin/tierstore";

/// The two Linux platforms.
pub const LINUX: [Platform; 2] = [Platform::LinuxX86_64, Platform::LinuxArm64];

/// A stand-in binary: bytes that hold [`VERSION`] as a build holds its version constant, and the
/// magic of a macOS code signature, so a test sees the bytes go through untouched.
pub fn binary() -> Vec<u8> {
    let mut bytes = b"\xcf\xfa\xed\xfe stand-in tierstore\0".to_vec();
    bytes.extend_from_slice(VERSION.as_bytes());
    bytes.extend_from_slice(b"\0read-only data\0\xfa\xde\x0c\xc0 signature blob\0");
    bytes
}

/// The package of [`binary`] for `platform`.
pub fn package(platform: Platform) -> Vec<u8> {
    archive(platform, VERSION, MIN_HOSTKEEPER, &binary()).expect("the package")
}

/// A scratch directory of one test. hostkeeper leaves an unpacked version read-only (0555), so
/// the drop opens every directory below it up first, or it could not be removed.
pub struct Scratch(TestDir);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        Scratch(TestDir::under(env!("CARGO_TARGET_TMPDIR"), name))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

/// Gives `path` and every directory below it mode 0700.
fn open_up(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            open_up(&entry.path());
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        open_up(self.path());
    }
}

/// hostkeeper's spelling of `platform`.
pub fn hk_platform(platform: Platform) -> hostkeeper_config::platform::Platform {
    platform.name().parse().expect("a hostkeeper platform")
}

/// Unpacks `archive` with hostkeeper's unpacker, as the daemon does for an order of tierstore
/// [`VERSION`] on `platform`, into `<dir>/packages`, owned by the test's own user (the daemon's is
/// root). The unpack is the one of a Linux host, which makes no macOS check of the tree (a Mac's
/// daemon also checks quarantine attributes and code signatures, through an adapter of its own).
pub fn unpack(dir: &Path, archive: &[u8], platform: Platform) -> Result<Unpacked, UnpackError> {
    let file = dir.join("package.tar.zst");
    std::fs::create_dir_all(dir).expect("the scratch directory");
    std::fs::write(&file, archive).expect("write the package");
    let packages = dir.join("packages");
    std::fs::create_dir_all(&packages).expect("the packages directory");
    let meta = std::fs::metadata(dir).expect("the scratch directory");
    let owner = Owner {
        uid: meta.uid(),
        gid: meta.gid(),
    };
    let fs = HostFs::open(&packages, owner).expect("open the packages directory");
    let order = UnpackOrder {
        service: ServiceName::new("tierstore").expect("a service name"),
        version: Version::new(VERSION).expect("a version"),
        platform: hk_platform(platform),
        package_digest: Sha256Hex::parse(&hex::encode(Sha256::digest(archive))).expect("a digest"),
    };
    let limits = Limits {
        max_unpacked_bytes: 1 << 30,
        max_entries: 1000,
    };
    hostkeeper_unpack::unpack(
        &file,
        &order,
        &limits,
        &fs,
        hostkeeper_unpack::Platform::Linux,
    )
}

/// The fields of a zstd frame header (RFC 8878, section 3.1.1.1).
#[derive(Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub single_segment: bool,
    pub reserved: bool,
    pub checksum: bool,
    pub dictionary: bool,
    /// The window the frame declares, in bytes.
    pub window: u64,
}

/// The header of the zstd frame `archive` starts with.
pub fn frame_header(archive: &[u8]) -> FrameHeader {
    assert_eq!(archive[..4], [0x28, 0xb5, 0x2f, 0xfd], "the zstd magic");
    let descriptor = archive[4];
    let single_segment = descriptor & 0x20 != 0;
    assert!(
        !single_segment,
        "a single-segment frame has no window descriptor"
    );
    let window_descriptor = archive[5];
    let base = 1u64 << (10 + u64::from(window_descriptor >> 3));
    FrameHeader {
        single_segment,
        reserved: descriptor & 0x08 != 0,
        checksum: descriptor & 0x04 != 0,
        dictionary: descriptor & 0x03 != 0,
        window: base + base / 8 * u64::from(window_descriptor & 0x07),
    }
}

/// The tar `archive` holds, decoded from its first zstd frame, and the number of bytes after
/// that frame.
pub fn decode(archive: &[u8]) -> (Vec<u8>, usize) {
    let mut rest = archive;
    let mut tar = Vec::new();
    {
        let mut decoder =
            ruzstd::decoding::StreamingDecoder::new(&mut rest).expect("a zstd frame header");
        decoder.read_to_end(&mut tar).expect("the frame decodes");
    }
    (tar, rest.len())
}

/// One entry of the tar in a package, as its header says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub kind: tar::EntryType,
    pub ustar: bool,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub owner_names: (Vec<u8>, Vec<u8>),
    pub content: Vec<u8>,
}

/// The entries of the package `archive`, in order.
pub fn listing(archive: &[u8]) -> Vec<Listed> {
    let (tar, _) = decode(archive);
    let mut reader = tar::Archive::new(&tar[..]);
    let mut listed = Vec::new();
    for entry in reader.entries().expect("a tar") {
        let mut entry = entry.expect("an entry");
        let header = entry.header().clone();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).expect("the content");
        listed.push(Listed {
            name: String::from_utf8(entry.path_bytes().into_owned()).expect("UTF-8"),
            kind: header.entry_type(),
            ustar: header.as_ustar().is_some(),
            mode: header.mode().expect("a mode"),
            uid: header.uid().expect("a uid"),
            gid: header.gid().expect("a gid"),
            mtime: header.mtime().expect("a time"),
            owner_names: (
                header.username_bytes().unwrap_or_default().to_vec(),
                header.groupname_bytes().unwrap_or_default().to_vec(),
            ),
            content,
        });
    }
    listed
}

/// The content of the file `name` of the package `archive`.
pub fn file(archive: &[u8], name: &str) -> Vec<u8> {
    listing(archive)
        .into_iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("no {name} in the package"))
        .content
}

/// The content of the file `name` of the package `archive`, as text.
pub fn text(archive: &[u8], name: &str) -> String {
    String::from_utf8(file(archive, name)).expect("UTF-8")
}

/// The manifest of the package `archive`, as TOML.
pub fn manifest_toml(archive: &[u8]) -> toml::Table {
    text(archive, "manifest.toml")
        .parse()
        .expect("the manifest is TOML")
}

/// Writes the files of the package `archive` below `dir` (the directories they are in made),
/// each through `edit` (name, text) first.
pub fn write_tree(dir: &Path, archive: &[u8], edit: impl Fn(&str, Vec<u8>) -> Vec<u8>) {
    for entry in listing(archive) {
        let path = dir.join(&entry.name);
        if entry.kind == tar::EntryType::Directory {
            std::fs::create_dir_all(&path).expect("a directory");
            continue;
        }
        std::fs::write(&path, edit(&entry.name, entry.content)).expect("a file");
    }
}

/// `<root>/var/lib/hostkeeper/packages/tierstore/<VERSION>`: where a host keeps the version, below
/// the scratch directory `root` that stands for `/`.
pub fn version_dir(root: &Path) -> PathBuf {
    root.join(PACKAGES.trim_start_matches('/'))
        .join("tierstore")
        .join(VERSION)
}

/// The `(section, key, value)` of each assignment of a systemd unit, its continuation lines
/// joined and its comments left out.
pub fn assignments(unit: &str) -> Vec<(String, String, String)> {
    let mut joined = Vec::new();
    let mut open = String::new();
    for line in unit.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.strip_suffix('\\') {
            Some(head) => {
                open.push_str(head);
                open.push(' ');
            }
            None => {
                open.push_str(line);
                joined.push(std::mem::take(&mut open));
            }
        }
    }
    let mut section = String::new();
    let mut out = Vec::new();
    for line in joined {
        if line.starts_with('[') {
            section = line;
        } else if let Some((key, value)) = line.split_once('=') {
            out.push((section.clone(), key.to_owned(), value.to_owned()));
        }
    }
    out
}

/// The value of the one assignment of `key` in `section` of `unit`.
pub fn unit_value(unit: &str, section: &str, key: &str) -> String {
    let found: Vec<String> = assignments(unit)
        .into_iter()
        .filter(|(s, k, _)| (s == section) & (k == key))
        .map(|(_, _, value)| value)
        .collect();
    assert_eq!(found.len(), 1, "{section} {key}= in {unit}");
    found[0].clone()
}

/// A value of a launchd plist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    String(String),
    Integer(i64),
    Bool(bool),
    Array(Vec<Value>),
    Dict(BTreeMap<String, Value>),
}

fn value(node: roxmltree::Node<'_, '_>) -> Value {
    let children = || node.children().filter(roxmltree::Node::is_element);
    match node.tag_name().name() {
        "string" => Value::String(node.text().unwrap_or_default().to_owned()),
        "integer" => Value::Integer(node.text().expect("a number").parse().expect("an integer")),
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "array" => Value::Array(children().map(value).collect()),
        "dict" => {
            let items: Vec<_> = children().collect();
            Value::Dict(
                items
                    .chunks(2)
                    .map(|pair| {
                        assert_eq!(pair[0].tag_name().name(), "key", "a key, then its value");
                        (
                            pair[0].text().unwrap_or_default().to_owned(),
                            value(pair[1]),
                        )
                    })
                    .collect(),
            )
        }
        other => panic!("a plist element this test does not read: {other}"),
    }
}

/// The top dictionary of the launchd plist `text`.
pub fn plist(text: &str) -> BTreeMap<String, Value> {
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(text, options).expect("XML");
    let dict = doc
        .root_element()
        .children()
        .find(roxmltree::Node::is_element)
        .expect("the top dict");
    match value(dict) {
        Value::Dict(map) => map,
        other => panic!("the plist's top value is {other:?}"),
    }
}

/// The text of the string `value`.
pub fn string(value: &Value) -> &str {
    match value {
        Value::String(text) => text,
        other => panic!("not a string: {other:?}"),
    }
}

/// The number of the integer `value`.
pub fn integer(value: &Value) -> i64 {
    match value {
        Value::Integer(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

/// The word after `flag` in `words`.
pub fn flag<'a>(words: &[&'a str], flag: &str) -> &'a str {
    let at = words
        .iter()
        .position(|word| *word == flag)
        .unwrap_or_else(|| panic!("no {flag} in {words:?}"));
    words[at + 1]
}

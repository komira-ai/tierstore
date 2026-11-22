//! The builder of the pkg through a fake of `pkgbuild` and `productbuild`, the argument lists of
//! the two programs, the manifest, and the fixtures the checker's tests share.

use std::cell::RefCell;
use std::io::Write;

use super::*;
use crate::testdir::TestDir;

mod check;
mod distribution;
mod odc;
mod postinstall;

/// The binary the pkg holds; nothing runs it.
pub(super) const BINARY_BYTES: &[u8] = b"the macos-arm64 tierstore binary";
pub(super) const VERSION: &str = "1.2.3";

pub(crate) fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-macos-pkg-unit"), name)
}

/// One entry of a tree read back: its path from the root (`""` is the root), its mode and, for a
/// file, its bytes.
pub(super) type Seen = (String, u32, Option<Vec<u8>>);

/// Every entry under `root`, sorted by path.
pub(super) fn tree_of(root: &Path) -> Vec<Seen> {
    fn walk(root: &Path, at: &Path, seen: &mut Vec<Seen>) {
        let meta = std::fs::symlink_metadata(at).expect("stat");
        let path = at
            .strip_prefix(root)
            .expect("under the root")
            .to_str()
            .expect("UTF-8")
            .to_owned();
        let mode = meta.permissions().mode() & 0o7777;
        if meta.is_dir() {
            seen.push((path, mode, None));
            for entry in std::fs::read_dir(at).expect("read_dir") {
                walk(root, &entry.expect("entry").path(), seen);
            }
        } else {
            seen.push((path, mode, Some(std::fs::read(at).expect("read"))));
        }
    }
    let mut seen = Vec::new();
    walk(root, root, &mut seen);
    seen.sort();
    seen
}

/// What `build_pkg` was called with: the identifier, the version, the component's path, and the
/// trees of the root and of the scripts directory at the call.
type PkgCall = (String, String, PathBuf, Vec<Seen>, Vec<Seen>);

/// The calls a [`FakeTools`] was made.
#[derive(Default)]
pub(crate) struct FakeTools {
    pub(super) pkg_calls: RefCell<Vec<PkgCall>>,
    /// `(component path, identifier, version, product path)`.
    pub(super) product_calls: RefCell<Vec<(PathBuf, String, String, PathBuf)>>,
    pub(crate) signature_calls: RefCell<Vec<PathBuf>>,
    signature: RefCell<Option<Result<String, Error>>>,
    /// A failure `build_pkg` or `build_product` gives instead of its work.
    fail_pkg: Option<String>,
    fail_product: Option<String>,
}

impl FakeTools {
    /// Tools whose `pkgutil` answers `signature`.
    pub(crate) fn signing(signature: Result<String, Error>) -> FakeTools {
        FakeTools {
            signature: RefCell::new(Some(signature)),
            ..FakeTools::default()
        }
    }
}

impl BuildPkg for FakeTools {
    fn build_pkg(
        &self,
        root: &Path,
        scripts: &Path,
        identifier: &str,
        version: &str,
        pkg: &Path,
    ) -> Result<(), Error> {
        if let Some(why) = &self.fail_pkg {
            return Err(Error::Failed(why.clone()));
        }
        self.pkg_calls.borrow_mut().push((
            identifier.to_owned(),
            version.to_owned(),
            pkg.to_owned(),
            tree_of(root),
            tree_of(scripts),
        ));
        std::fs::write(pkg, b"component").expect("the component");
        Ok(())
    }

    fn build_product(
        &self,
        component: &Path,
        identifier: &str,
        version: &str,
        product: &Path,
    ) -> Result<(), Error> {
        if let Some(why) = &self.fail_product {
            return Err(Error::Failed(why.clone()));
        }
        self.product_calls.borrow_mut().push((
            component.to_owned(),
            identifier.to_owned(),
            version.to_owned(),
            product.to_owned(),
        ));
        std::fs::write(product, b"product").expect("the product");
        Ok(())
    }

    fn check_signature(&self, pkg: &Path) -> Result<String, Error> {
        self.signature_calls.borrow_mut().push(pkg.to_owned());
        self.signature
            .borrow_mut()
            .take()
            .expect("a scripted signature")
    }
}

/// The request to build the pkg of `dir`: the binary written there, the stage and out beside it.
fn request_in<'a>(binary: &'a Path, stage: &'a Path, out: &'a Path) -> Request<'a> {
    Request {
        version: VERSION,
        binary,
        stage,
        out,
    }
}

#[test]
fn the_builder_lays_out_the_payload_and_the_postinstall_and_builds_the_product() {
    // Catches: a payload with a file, mode or byte other than the binary at its path (0755) and
    // the two plists (0644) under root-readable 0755 directories, a scripts directory handed to
    // pkgbuild that holds anything but the postinstall (0755, its bytes) or a script the installer
    // cannot run, an identifier or version other than the release's, and a product not made of
    // the component.
    let dir = scratch("builds");
    let binary = dir.join("tierstore");
    std::fs::write(&binary, BINARY_BYTES).expect("the binary");
    let (stage, out) = (dir.join("stage"), dir.join("out"));
    let tools = FakeTools::default();
    let said = build(&tools, &request_in(&binary, &stage, &out)).expect("builds");
    assert_eq!(
        said,
        [format!(
            "tierstore-1.2.3-macos-arm64.pkg: sha256 {}",
            Sha256::of(b"product")
        )]
    );
    let dirs = [
        "",
        "Library",
        "Library/Application Support",
        "Library/Application Support/tierstore",
        "Library/Application Support/tierstore/bin",
        "Library/LaunchDaemons",
    ];
    let mut want: Vec<Seen> = dirs.map(|d| (d.to_owned(), 0o755, None)).to_vec();
    want.push((
        "Library/Application Support/tierstore/bin/tierstore".to_owned(),
        0o755,
        Some(BINARY_BYTES.to_vec()),
    ));
    want.push((
        "Library/LaunchDaemons/tierstore.plist".to_owned(),
        0o644,
        Some(PLIST.as_bytes().to_vec()),
    ));
    want.push((
        "Library/LaunchDaemons/tierstore-fs.plist".to_owned(),
        0o644,
        Some(FS_PLIST.as_bytes().to_vec()),
    ));
    want.sort();
    let scripts: Vec<Seen> = vec![
        (String::new(), 0o755, None),
        (
            "postinstall".to_owned(),
            0o755,
            Some(POSTINSTALL.as_bytes().to_vec()),
        ),
    ];
    assert_eq!(
        *tools.pkg_calls.borrow(),
        [(
            "com.komira.tierstore".to_owned(),
            "1.2.3".to_owned(),
            stage.join("tierstore.pkg"),
            want,
            scripts
        )]
    );
    assert_eq!(
        *tools.product_calls.borrow(),
        [(
            stage.join("tierstore.pkg"),
            "com.komira.tierstore".to_owned(),
            "1.2.3".to_owned(),
            out.join("tierstore-1.2.3-macos-arm64.pkg")
        )]
    );
    // The stage holds the root, the scripts and the component and nothing else.
    let names: Vec<String> = tree_of(&stage).into_iter().map(|(p, ..)| p).collect();
    assert!(
        names.iter().all(|p| p.is_empty()
            || p == "tierstore.pkg"
            || p == "root"
            || p.starts_with("root/")
            || p == "scripts"
            || p == "scripts/postinstall"),
        "{names:?}"
    );
}

#[test]
fn the_builder_refuses_a_bad_version_a_used_stage_and_a_missing_binary_before_it_builds() {
    // Catches: a version that is not X.Y.Z reaching the installer's records, a stage that holds
    // earlier files packaged with the new ones, and a missing binary reported as a pkg.
    let dir = scratch("refuses");
    let binary = dir.join("tierstore");
    std::fs::write(&binary, BINARY_BYTES).expect("the binary");
    let (stage, out) = (dir.join("stage"), dir.join("out"));
    let tools = FakeTools::default();
    let mut request = request_in(&binary, &stage, &out);
    request.version = "1.2";
    assert_eq!(
        build(&tools, &request),
        Err(Error::Usage(
            "--version \"1.2\" is not <major>.<minor>.<patch>".to_owned()
        ))
    );
    let absent = dir.join("absent");
    let request = request_in(&absent, &stage, &out);
    assert!(matches!(
        build(&tools, &request),
        Err(Error::Spawn { what, .. }) if what.starts_with("read ")
    ));
    std::fs::create_dir(&stage).expect("a used stage");
    let request = request_in(&binary, &stage, &out);
    assert_eq!(
        build(&tools, &request),
        Err(Error::Failed(format!(
            "{} exists: the pkg's tree is laid out in a new directory, so it holds nothing else",
            stage.display()
        )))
    );
    assert!(tools.pkg_calls.borrow().is_empty());
    assert!(!out.exists());
}

#[test]
fn a_failure_of_either_tool_or_a_product_that_is_not_written_ends_the_build() {
    // Catches: a pkgbuild or productbuild failure that still prints a digest, and a missing
    // product that is summed as empty.
    let binary_of = |name: &str| {
        let dir = scratch(name);
        std::fs::write(dir.join("tierstore"), BINARY_BYTES).expect("the binary");
        dir
    };
    let dir = binary_of("fails-pkg");
    let tools = FakeTools {
        fail_pkg: Some("pkgbuild failed".to_owned()),
        ..FakeTools::default()
    };
    let (bin, stage, out) = (dir.join("tierstore"), dir.join("s"), dir.join("o"));
    let request = request_in(&bin, &stage, &out);
    assert_eq!(
        build(&tools, &request),
        Err(Error::Failed("pkgbuild failed".to_owned()))
    );
    let dir = binary_of("fails-product");
    let tools = FakeTools {
        fail_product: Some("productbuild failed".to_owned()),
        ..FakeTools::default()
    };
    let (bin, stage, out) = (dir.join("tierstore"), dir.join("s"), dir.join("o"));
    let request = request_in(&bin, &stage, &out);
    assert_eq!(
        build(&tools, &request),
        Err(Error::Failed("productbuild failed".to_owned()))
    );
    // Real programs that succeed and write nothing: the product is then missing.
    let dir = binary_of("no-product");
    let tools = PkgTools {
        pkgbuild: "true".into(),
        productbuild: "true".into(),
        pkgutil: "true".into(),
    };
    let (bin, stage, out) = (dir.join("tierstore"), dir.join("s"), dir.join("o"));
    let request = request_in(&bin, &stage, &out);
    assert!(matches!(
        build(&tools, &request),
        Err(Error::Spawn { what, .. }) if what.ends_with("tierstore-1.2.3-macos-arm64.pkg")
    ));
}

#[test]
fn pkgbuild_and_productbuild_get_the_scripts_identifier_version_location_and_ownership() {
    // Catches: a pkgbuild call that leaves out the scripts (the postinstall would not be in the
    // pkg) or gives another directory, another install location or the owner of the build
    // machine's user, and a productbuild call that builds another component.
    let (root, pkg) = (Path::new("stage/root"), Path::new("stage/tierstore.pkg"));
    let scripts = Path::new("stage/scripts");
    assert_eq!(
        pkgbuild_args(root, scripts, IDENTIFIER, VERSION, pkg),
        Ok([
            "--root",
            "stage/root",
            "--scripts",
            "stage/scripts",
            "--identifier",
            "com.komira.tierstore",
            "--version",
            "1.2.3",
            "--install-location",
            "/",
            "--ownership",
            "recommended",
            "stage/tierstore.pkg"
        ]
        .map(str::to_owned)
        .to_vec())
    );
    assert_eq!(
        productbuild_args(pkg, IDENTIFIER, VERSION, Path::new("out/x.pkg")),
        Ok([
            "--package",
            "stage/tierstore.pkg",
            "--identifier",
            "com.komira.tierstore",
            "--version",
            "1.2.3",
            "out/x.pkg"
        ]
        .map(str::to_owned)
        .to_vec())
    );
}

#[test]
fn a_path_that_is_not_utf8_is_refused_by_both_tools() {
    // Catches: a lossy path handed to a tool, which would build from another directory.
    use std::os::unix::ffi::OsStrExt;
    let bad = Path::new(std::ffi::OsStr::from_bytes(b"x/\xff"));
    let tools = PkgTools::system();
    for result in [
        tools.build_pkg(bad, Path::new("s"), IDENTIFIER, VERSION, Path::new("a")),
        tools.build_pkg(Path::new("r"), bad, IDENTIFIER, VERSION, Path::new("a")),
        tools.build_product(bad, IDENTIFIER, VERSION, Path::new("a")),
    ] {
        assert!(matches!(result, Err(Error::Failed(why)) if why.ends_with("is not UTF-8")));
    }
}

#[test]
fn the_tools_run_their_programs_and_report_a_failure_or_a_program_that_is_absent() {
    // Catches: a failed program passed, and one that could not start reported as a success.
    let tools = |program: &str| PkgTools {
        pkgbuild: program.into(),
        productbuild: program.into(),
        pkgutil: program.into(),
    };
    let (r, s) = (Path::new("r"), Path::new("s"));
    assert_eq!(
        tools("true").build_pkg(r, s, IDENTIFIER, VERSION, Path::new("p")),
        Ok(())
    );
    assert_eq!(
        tools("true").build_product(Path::new("c"), IDENTIFIER, VERSION, Path::new("p")),
        Ok(())
    );
    let args = "--root r --scripts s --identifier com.komira.tierstore --version 1.2.3 \
                --install-location / --ownership recommended p";
    assert_eq!(
        tools("false").build_pkg(r, s, IDENTIFIER, VERSION, Path::new("p")),
        Err(Error::Failed(format!("false {args}: \n")))
    );
    assert!(matches!(
        tools("no-such-program").build_pkg(r, s, IDENTIFIER, VERSION, Path::new("p")),
        Err(Error::Spawn { what, .. }) if what == format!("no-such-program {args}")
    ));
}

#[test]
fn the_file_is_named_for_the_version_and_the_macos_arm64_platform() {
    // Catches: a pkg name the release workflow does not look for.
    assert_eq!(file_name("1.2.3"), "tierstore-1.2.3-macos-arm64.pkg");
}

#[test]
fn the_manifest_is_the_archive_manifests_bounds_with_the_launchd_job_and_the_installed_hook() {
    // Catches: a hook command that is not the installed binary's update-hook, a run entry that
    // is not the job's label, params that do not name launchd and both jobs' labels, and bounds or deadlines that
    // drift from the apt entry's (which a test holds to the archive manifest).
    let manifest = manifest_json("0.1.225");
    let apt = crate::release::desired_apt::entry("1.2.3", "d", "0.1.225")["manifest"].clone();
    assert_eq!(manifest["package"], "com.komira.tierstore");
    assert_eq!(
        manifest["hook"]["command"],
        serde_json::json!([
            "/Library/Application Support/tierstore/bin/tierstore",
            "update-hook"
        ])
    );
    assert_eq!(manifest["run"], serde_json::json!(["tierstore"]));
    assert_eq!(manifest["healthUrl"], "http://127.0.0.1:9300/readyz");
    assert_eq!(manifest["params"]["config_file"]["text"], CONFIG_FILE);
    assert_eq!(manifest["params"]["service_manager"]["text"], "launchd");
    assert_eq!(manifest["params"]["launchd_label"]["text"], "tierstore");
    assert_eq!(
        manifest["params"]["fs_launchd_label"]["text"],
        "tierstore-fs"
    );
    assert_eq!(manifest["params"].as_object().map(|o| o.len()), Some(4));
    assert_eq!(manifest["minHostkeeper"], "0.1.225");
    assert_eq!(manifest["hook"]["boot"], "restore");
    for key in [
        "timeoutSecs",
        "verifyDeadlineSecs",
        "promotionCeilingSecs",
        "cleanupRetries",
    ] {
        assert_eq!(manifest["hook"][key], apt["hook"][key], "{key}");
    }
    assert_eq!(manifest["installerBounds"], apt["installerBounds"]);
    assert_eq!(manifest["healthUrl"], apt["healthUrl"]);
}

/// A binary's gzip, for the `Payload` of a fixture.
pub(super) fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(bytes).expect("compress");
    gz.finish().expect("compress")
}

/// One portable (`odc`) cpio entry: a header, the name and its NUL, the data.
pub(super) fn odc_entry(name: &str, mode: u32, uid: u32, gid: u32, data: &[u8]) -> Vec<u8> {
    let header = format!(
        "070707{:06o}{:06o}{mode:06o}{uid:06o}{gid:06o}{:06o}{:06o}{:011o}{:06o}{:011o}",
        1,
        7,
        1,
        0,
        1_800_000_000_u64,
        name.len() + 1,
        data.len()
    );
    assert_eq!(header.len(), ODC_HEADER);
    [header.as_bytes(), name.as_bytes(), b"\0", data].concat()
}

/// A cpio archive of `entries`, then the trailer, then a block's padding.
pub(super) fn odc(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = entries.concat();
    bytes.extend(odc_entry(TRAILER, 0, 0, 0, b""));
    bytes.extend([0_u8; 120]);
    bytes
}

pub(super) fn dir_entry(name: &str) -> Vec<u8> {
    odc_entry(name, 0o040_755, 0, 0, b"")
}

/// The payload `pkgbuild` writes for the binary `binary`: every entry, in its order.
pub(super) fn good_entries(binary: &[u8]) -> Vec<Vec<u8>> {
    vec![
        dir_entry("."),
        dir_entry("./Library"),
        dir_entry("./Library/Application Support"),
        dir_entry("./Library/Application Support/tierstore"),
        dir_entry("./Library/Application Support/tierstore/bin"),
        odc_entry(
            "./Library/Application Support/tierstore/bin/tierstore",
            0o100_755,
            0,
            0,
            binary,
        ),
        dir_entry("./Library/LaunchDaemons"),
        odc_entry(
            "./Library/LaunchDaemons/tierstore.plist",
            0o100_644,
            0,
            0,
            PLIST.as_bytes(),
        ),
        odc_entry(
            "./Library/LaunchDaemons/tierstore-fs.plist",
            0o100_644,
            0,
            0,
            FS_PLIST.as_bytes(),
        ),
    ]
}

/// The index of the binary's, the plist's and the file service plist's entry in [`good_entries`].
pub(super) const BIN_AT: usize = 5;
pub(super) const PLIST_AT: usize = 7;
pub(super) const FS_PLIST_AT: usize = 8;

/// `PackageInfo` as `pkgbuild` writes it for the pkg of 1.2.3, with `attributes` in place of the
/// identifier, version and install location and `extra` among the root's children.
pub(super) fn package_info_with(attributes: &str, extra: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<pkg-info overwrite-permissions=\"true\" \
         relocatable=\"false\" {attributes} postinstall-action=\"none\" format-version=\"2\" \
         generator-version=\"InstallCmds-845 (24A335)\" auth=\"root\">\n    <payload \
         numberOfFiles=\"9\" installKBytes=\"1\"/>\n    <bundle-version/>\n    \
         <upgrade-bundle/>\n    <update-bundle/>\n    <atomic-update-bundle/>\n    \
         <strict-identifier/>\n    <relocate/>\n    {extra}\n</pkg-info>\n"
    )
}

pub(super) fn package_info() -> String {
    package_info_with(
        "identifier=\"com.komira.tierstore\" version=\"1.2.3\" install-location=\"/\"",
        SCRIPTS_ELEMENT,
    )
}

/// The scripts element `pkgbuild` writes in `PackageInfo` for a scripts directory that holds a
/// `postinstall`.
pub(super) const SCRIPTS_ELEMENT: &str =
    "<scripts>\n        <postinstall file=\"./postinstall\"/>\n    </scripts>";

/// The `Distribution` `productbuild --package` writes for the component pkg of 1.2.3, with
/// `refs` as the `<pkg-ref>` that refers to the component and `extra` among the root element's
/// children.
pub(super) fn distribution_with(refs: &str, extra: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<installer-gui-script \
         minSpecVersion=\"1\">\n    <pkg-ref id=\"com.komira.tierstore\"/>\n    <options \
         customize=\"never\" require-scripts=\"false\"/>\n    <choices-outline>\n        <line \
         choice=\"default\">\n            <line choice=\"com.komira.tierstore\"/>\n        \
         </line>\n    </choices-outline>\n    <choice id=\"default\"/>\n    <choice \
         id=\"com.komira.tierstore\" visible=\"false\">\n        <pkg-ref \
         id=\"com.komira.tierstore\"/>\n    </choice>\n    {refs}\n    <product \
         id=\"com.komira.tierstore\" version=\"1.2.3\"/>\n    {extra}\n\
         </installer-gui-script>\n"
    )
}

/// The reference to the component of the good `Distribution`.
pub(super) const COMPONENT_REF: &str = "<pkg-ref id=\"com.komira.tierstore\" \
    version=\"1.2.3\" onConclusion=\"none\">#tierstore.pkg</pkg-ref>";

pub(super) fn distribution_text() -> String {
    distribution_with(COMPONENT_REF, "")
}

/// A product expanded as `pkgutil --expand` leaves it (`pkg` holds the `Distribution` and the
/// component's directory, whose `Scripts` holds the postinstall), with `entries` as the
/// component's payload, and the binary it is
/// checked against, in a scratch directory of its own.
pub(super) struct Expanded {
    pub(super) dir: TestDir,
}

impl Expanded {
    pub(super) fn new(name: &str, entries: &[Vec<u8>]) -> Expanded {
        let dir = scratch(name);
        std::fs::create_dir(dir.join("pkg")).expect("the expanded product");
        std::fs::create_dir(dir.join("pkg").join(COMPONENT)).expect("the expanded component");
        std::fs::write(dir.join("tierstore"), BINARY_BYTES).expect("the binary");
        let expanded = Expanded { dir };
        std::fs::write(expanded.product().join(DISTRIBUTION), distribution_text())
            .expect("the distribution");
        expanded.write("Bom", b"BOMStore");
        expanded.write("PackageInfo", package_info().as_bytes());
        expanded.write("Payload", &gzip(&odc(entries)));
        let scripts = expanded.pkg().join("Scripts");
        std::fs::create_dir(&scripts).expect("the expanded scripts");
        let script = scripts.join("postinstall");
        std::fs::write(&script, POSTINSTALL).expect("the postinstall");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("mode");
        expanded
    }

    pub(super) fn good(name: &str) -> Expanded {
        Expanded::new(name, &good_entries(BINARY_BYTES))
    }

    /// The top directory of the product.
    pub(super) fn product(&self) -> PathBuf {
        self.dir.join("pkg")
    }

    /// The directory of the component in the product.
    pub(super) fn pkg(&self) -> PathBuf {
        self.product().join(COMPONENT)
    }

    /// The postinstall of the component.
    pub(super) fn script(&self) -> PathBuf {
        self.pkg().join("Scripts").join("postinstall")
    }

    pub(super) fn write(&self, file: &str, bytes: &[u8]) {
        std::fs::write(self.pkg().join(file), bytes).expect("write a file of the pkg");
    }

    pub(super) fn check(&self) -> Result<Vec<String>, Error> {
        check(&self.product(), VERSION, &self.dir.join("tierstore"))
    }

    pub(super) fn problems(&self) -> Vec<String> {
        match self.check() {
            Err(Error::Refused(lines)) => lines,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}

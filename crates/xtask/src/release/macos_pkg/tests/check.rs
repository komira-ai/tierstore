//! The read-back of an expanded pkg: its parts, its `PackageInfo` and its payload, checked
//! against the binary it must hold; each deviation from what the builder writes is refused.

use super::*;

const BIN: &str = "Library/Application Support/tierstore/bin/tierstore";
const PLIST_FILE: &str = "Library/LaunchDaemons/tierstore.plist";

/// What the check says of a part that is not the file or the directory it must be.
const ONLY: &str = "an expanded component of tierstore's pkg holds only the files Bom, \
                    PackageInfo and Payload and the directory Scripts";

/// A fixture whose payload is [`good_entries`] with the entry at `at` replaced by `entry`.
fn with_entry(name: &str, at: usize, entry: Vec<u8>) -> Expanded {
    let mut entries = good_entries(BINARY_BYTES);
    entries[at] = entry;
    Expanded::new(name, &entries)
}

#[test]
fn a_good_expanded_pkg_passes_the_check() {
    // Catches: a check that refuses the product productbuild makes of the component pkgbuild
    // makes of the tree the builder lays out.
    let expanded = Expanded::good("macos-pkg-check-good");
    assert_eq!(
        expanded.check(),
        Ok(vec![format!(
            "check-macos-pkg: {} is a product of the one component tierstore.pkg \
             (com.komira.tierstore 1.2.3), with the one script postinstall, which installs \
             exactly /Library/Application Support/tierstore/bin/tierstore (755) and \
             /Library/LaunchDaemons/tierstore.plist (644), owned by root",
            expanded.product().display()
        )])
    );
}

#[test]
fn a_product_without_its_distribution_or_component_or_with_another_entry_is_refused() {
    // Catches: a bare component pkg (the layer productbuild adds missing), a product without its
    // component, one with a stray file or directory beside them, or a Distribution that is a
    // directory or a component that is a link.
    let expanded = Expanded::good("macos-pkg-product-distribution-missing");
    std::fs::remove_file(expanded.product().join(DISTRIBUTION)).expect("remove");
    assert_eq!(expanded.problems(), ["missing Distribution"]);
    let expanded = Expanded::good("macos-pkg-product-component-missing");
    std::fs::remove_dir_all(expanded.pkg()).expect("remove");
    assert_eq!(expanded.problems(), ["missing tierstore.pkg"]);
    let only = "an expanded product of tierstore's pkg holds only the file Distribution and the \
                directory tierstore.pkg";
    let expanded = Expanded::good("macos-pkg-product-extra");
    std::fs::write(expanded.product().join("Resources"), b"").expect("a stray file");
    std::fs::create_dir(expanded.product().join("Scripts")).expect("a stray directory");
    assert_eq!(
        expanded.problems(),
        [format!("Resources: {only}"), format!("Scripts: {only}")]
    );
    let expanded = Expanded::good("macos-pkg-product-kinds");
    let elsewhere = Expanded::good("macos-pkg-product-kinds-source");
    std::fs::remove_file(expanded.product().join(DISTRIBUTION)).expect("remove");
    std::fs::create_dir(expanded.product().join(DISTRIBUTION)).expect("a directory");
    std::fs::remove_dir_all(expanded.pkg()).expect("remove");
    std::os::unix::fs::symlink(elsewhere.pkg(), expanded.pkg()).expect("a link");
    assert_eq!(
        expanded.problems(),
        [
            format!("Distribution: {only}"),
            format!("tierstore.pkg: {only}"),
            "missing Distribution".to_owned(),
            "missing tierstore.pkg".to_owned(),
        ]
    );
}

#[test]
fn a_component_without_its_scripts_or_with_another_file_or_without_a_part_is_refused() {
    // Catches: a pkg without the scripts directory (nothing would make the job's account and
    // directories), one whose Scripts is a file, a stray file, and a missing bill of materials,
    // payload or info.
    let expanded = Expanded::good("macos-pkg-check-no-scripts");
    std::fs::remove_dir_all(expanded.pkg().join("Scripts")).expect("remove");
    assert_eq!(expanded.problems(), ["missing Scripts"]);
    let expanded = Expanded::good("macos-pkg-check-scripts-file");
    std::fs::remove_dir_all(expanded.pkg().join("Scripts")).expect("remove");
    expanded.write("Scripts", POSTINSTALL.as_bytes());
    assert_eq!(
        expanded.problems(),
        [format!("Scripts: {ONLY}"), "missing Scripts".to_owned()]
    );
    let expanded = Expanded::good("macos-pkg-check-parts");
    expanded.write("Resources", b"x");
    std::fs::remove_file(expanded.pkg().join("Bom")).expect("remove");
    assert_eq!(
        expanded.problems(),
        [format!("Resources: {ONLY}"), "missing Bom".to_owned()]
    );
    // A payload that is a directory is not the payload, and is not read.
    let expanded = Expanded::good("macos-pkg-check-payload-dir");
    std::fs::remove_file(expanded.pkg().join("Payload")).expect("remove");
    std::fs::create_dir(expanded.pkg().join("Payload")).expect("a directory");
    std::fs::remove_file(expanded.pkg().join("PackageInfo")).expect("remove");
    assert_eq!(
        expanded.problems(),
        [
            format!("Payload: {ONLY}"),
            "missing PackageInfo".to_owned(),
            "missing Payload".to_owned(),
        ]
    );
}

#[test]
fn scripts_of_another_file_mode_kind_or_bytes_or_without_the_postinstall_are_refused() {
    // Catches: a pkg that carries another script beside the postinstall (a preinstall the check
    // never read), a postinstall the installer cannot run (not executable), one that is a link
    // (whose target the installer would run) or a directory, one whose bytes are not the
    // repository's (another account, other directories, or a job loaded behind the update
    // hook's back), and a Scripts directory that holds no postinstall.
    let expanded = Expanded::good("macos-pkg-check-scripts-extra");
    std::fs::write(expanded.pkg().join("Scripts/preinstall"), b"#!/bin/sh\n").expect("extra");
    assert_eq!(
        expanded.problems(),
        ["Scripts holds \"preinstall\": tierstore's pkg has one script, its postinstall"]
    );
    let expanded = Expanded::good("macos-pkg-check-scripts-mode");
    std::fs::set_permissions(expanded.script(), std::fs::Permissions::from_mode(0o644))
        .expect("mode");
    assert_eq!(
        expanded.problems(),
        ["Scripts: postinstall has mode 644, not 755"]
    );
    let expanded = Expanded::good("macos-pkg-check-scripts-bytes");
    let edited = POSTINSTALL.replace("name=_tierstore", "name=_other");
    std::fs::write(expanded.script(), edited).expect("other bytes");
    assert_eq!(
        expanded.problems(),
        ["Scripts: postinstall is not the postinstall release macos-pkg writes"]
    );
    let expanded = Expanded::good("macos-pkg-check-scripts-link");
    let elsewhere = Expanded::good("macos-pkg-check-scripts-link-source");
    std::fs::remove_file(expanded.script()).expect("remove");
    std::os::unix::fs::symlink(elsewhere.script(), expanded.script()).expect("a link");
    assert_eq!(
        expanded.problems(),
        ["Scripts: postinstall is of type 120000, not a regular file"]
    );
    let expanded = Expanded::good("macos-pkg-check-scripts-dir");
    std::fs::remove_file(expanded.script()).expect("remove");
    std::fs::create_dir(expanded.script()).expect("a directory");
    assert_eq!(
        expanded.problems(),
        ["Scripts: postinstall is of type 40000, not a regular file"]
    );
    let expanded = Expanded::good("macos-pkg-check-scripts-empty");
    std::fs::remove_file(expanded.script()).expect("remove");
    assert_eq!(expanded.problems(), ["Scripts holds no postinstall"]);
}

#[test]
fn an_unreadable_scripts_directory_is_an_error_naming_it() {
    // Catches: a check that passes a pkg whose scripts it could not read. A directory nobody may
    // read is an I/O error naming it (a user that reads it anyway, root, finds the pkg good).
    let expanded = Expanded::good("macos-pkg-check-scripts-unreadable");
    let scripts = expanded.pkg().join("Scripts");
    std::fs::set_permissions(&scripts, std::fs::Permissions::from_mode(0o000)).expect("mode");
    let readable = std::fs::read_dir(&scripts).is_ok();
    let result = expanded.check();
    std::fs::set_permissions(&scripts, std::fs::Permissions::from_mode(0o755)).expect("mode");
    match result {
        Ok(_) => assert!(readable),
        Err(Error::Spawn { what, .. }) => assert!(!readable && what.ends_with("Scripts")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn package_info_of_another_identifier_version_location_script_action_or_bundle_is_refused() {
    // Catches: a pkg installed under another receipt, of another release, relocatable to another
    // place, naming no postinstall (the installer would not run it), a preinstall, an action
    // after the install (a restart or logout of the Mac) or a bundle (hostkeeper's installer
    // refuses a bundle), or whose PackageInfo cannot be read.
    let problems = |text: String| {
        let expanded = Expanded::good("macos-pkg-check-info");
        expanded.write("PackageInfo", text.as_bytes());
        expanded.problems()
    };
    let attrs = |id: &str, version: &str, at: &str| {
        package_info_with(
            &format!("identifier=\"{id}\" version=\"{version}\" install-location=\"{at}\""),
            SCRIPTS_ELEMENT,
        )
    };
    assert_eq!(
        problems(attrs("com.komira.other", "1.2.4", "/usr/local")),
        [
            "PackageInfo: identifier is \"com.komira.other\", not \"com.komira.tierstore\"",
            "PackageInfo: version is \"1.2.4\", not \"1.2.3\"",
            "PackageInfo: install-location is \"/usr/local\", not \"/\"",
        ]
    );
    assert_eq!(
        problems(package_info_with("", SCRIPTS_ELEMENT)),
        [
            "PackageInfo has no identifier",
            "PackageInfo has no version",
            "PackageInfo has no install-location",
        ]
    );
    let good = "identifier=\"com.komira.tierstore\" version=\"1.2.3\" install-location=\"/\"";
    let no_postinstall = "PackageInfo names no postinstall script (<postinstall \
                          file=\"./postinstall\"/>): nothing would make the account and the \
                          directories of the job";
    // pkgbuild with no scripts directory writes an empty scripts element, or none.
    for empty in ["", "<scripts/>"] {
        assert_eq!(problems(package_info_with(good, empty)), [no_postinstall]);
    }
    assert_eq!(
        problems(package_info_with(
            good,
            "<scripts><postinstall file=\"./other\"/></scripts>"
        )),
        [no_postinstall]
    );
    let preinstall = package_info_with(
        good,
        "<scripts><preinstall file=\"./preinstall\"/><postinstall \
         file=\"./postinstall\"/></scripts><bundle path=\"./A.app\"/>",
    );
    assert_eq!(
        problems(preinstall),
        [
            "PackageInfo names a preinstall element: tierstore's pkg has one script, its \
             postinstall",
            "PackageInfo names a bundle: tierstore's pkg holds none",
        ]
    );
    let restart = package_info().replace(
        "postinstall-action=\"none\"",
        "postinstall-action=\"restart\"",
    );
    assert_eq!(
        problems(restart),
        [
            "PackageInfo: postinstall-action is \"restart\", not \"none\": the install asks for \
          no restart or logout"
        ]
    );
    let no_action = package_info().replace("postinstall-action=\"none\" ", "");
    let expanded = Expanded::good("macos-pkg-check-info-no-action");
    expanded.write("PackageInfo", no_action.as_bytes());
    assert!(expanded.check().is_ok(), "no action is no action");
    assert_eq!(
        problems("not xml".to_owned()),
        ["PackageInfo has no readable <pkg-info> element"]
    );
    assert_eq!(
        problems("<pkg-info identifier=x>".to_owned()),
        ["PackageInfo has no readable <pkg-info> element"]
    );
}

#[test]
fn a_payload_that_is_not_a_gzip_cpio_is_refused() {
    // Catches: a payload read as if it were good when it is not an archive, or a payload whose
    // gzip is intact and whose archive is cut.
    let expanded = Expanded::good("macos-pkg-check-not-gzip");
    expanded.write("Payload", b"plainly not gzip");
    assert_eq!(
        expanded.problems(),
        ["Payload is not gzip data: invalid gzip header"]
    );
    let expanded = Expanded::good("macos-pkg-check-cut");
    expanded.write("Payload", &gzip(b"070707"));
    assert_eq!(
        expanded.problems(),
        ["Payload: the archive ends at byte 6 without its trailer"]
    );
}

#[test]
fn a_payload_with_another_entry_is_refused_naming_it() {
    // Catches: a file beside the two, a directory beside the tree, a missing file, a missing
    // directory.
    let mut entries = good_entries(BINARY_BYTES);
    entries.push(odc_entry("./Library/extra", 0o100_644, 0, 0, b"x"));
    entries.push(odc_entry("stray", 0o100_644, 0, 0, b"x"));
    let expanded = Expanded::new("macos-pkg-check-extra", &entries);
    assert_eq!(
        expanded.problems(),
        [
            "/Library/extra (\"./Library/extra\") is in the payload: not a file of tierstore's pkg",
            "/stray (\"stray\") is in the payload: not a file of tierstore's pkg",
        ]
    );
    let mut entries = good_entries(BINARY_BYTES);
    entries.remove(PLIST_AT);
    entries.remove(BIN_AT);
    let expanded = Expanded::new("macos-pkg-check-missing", &entries);
    assert_eq!(
        expanded.problems(),
        [
            format!("missing /{BIN} in the payload"),
            format!("missing /{PLIST_FILE} in the payload"),
        ]
    );
}

#[test]
fn an_entry_of_another_mode_owner_group_or_kind_is_refused() {
    // Catches: a binary others can write, a setuid binary, a group-writable directory, an owner
    // that is not root, a group that is neither wheel nor admin, a file that is a directory or a
    // link. Admin (80) is allowed as a group.
    let bin = |mode: u32, uid: u32, gid: u32| {
        odc_entry(&format!("./{BIN}"), mode, uid, gid, BINARY_BYTES)
    };
    let cases = [
        (bin(0o100_777, 0, 0), format!("/{BIN}: mode 777, not 755")),
        (bin(0o104_755, 0, 0), format!("/{BIN}: mode 4755, not 755")),
        (bin(0o102_755, 0, 0), format!("/{BIN}: mode 2755, not 755")),
        (bin(0o101_755, 0, 0), format!("/{BIN}: mode 1755, not 755")),
        (
            bin(0o100_755, 501, 0),
            format!("/{BIN}: owned by uid 501 and gid 0, not root and wheel or admin"),
        ),
        (
            bin(0o100_755, 0, 20),
            format!("/{BIN}: owned by uid 0 and gid 20, not root and wheel or admin"),
        ),
        (
            bin(0o040_755, 0, 0),
            format!("/{BIN}: of type 40000, not 100000"),
        ),
        (
            bin(0o120_755, 0, 0),
            format!("/{BIN}: of type 120000, not 100000"),
        ),
    ];
    for (n, (entry, problem)) in cases.into_iter().enumerate() {
        let expanded = with_entry(&format!("macos-pkg-check-entry-{n}"), BIN_AT, entry);
        assert_eq!(expanded.problems(), [problem], "case {n}");
    }
    let admin = with_entry("macos-pkg-check-admin", BIN_AT, bin(0o100_755, 0, 80));
    assert!(admin.check().is_ok());
    let group_writable = with_entry(
        "macos-pkg-check-dir",
        2,
        odc_entry("./Library/Application Support", 0o040_775, 0, 0, b""),
    );
    assert_eq!(
        group_writable.problems(),
        ["/Library/Application Support: mode 775, not 755"]
    );
    let file_dir = with_entry(
        "macos-pkg-check-dir-as-file",
        1,
        odc_entry("./Library", 0o100_755, 0, 0, b""),
    );
    assert_eq!(file_dir.problems(), ["/Library: of type 100000, not 40000"]);
}

#[test]
fn a_binary_or_plist_of_other_bytes_is_refused() {
    // Catches: a payload that is not the binary given (the signature of the macOS build would
    // not hold) and a plist that is not the one the repository ships.
    let other = odc_entry(&format!("./{BIN}"), 0o100_755, 0, 0, b"another binary");
    let expanded = with_entry("macos-pkg-check-binary-bytes", BIN_AT, other);
    assert_eq!(
        expanded.problems(),
        [format!("/{BIN}: not the bytes release macos-pkg writes")]
    );
    let plist = |text: &str| {
        let entry = odc_entry(&format!("./{PLIST_FILE}"), 0o100_644, 0, 0, text.as_bytes());
        with_entry("macos-pkg-check-plist-bytes", PLIST_AT, entry).problems()
    };
    let bytes = format!("/{PLIST_FILE}: not the bytes release macos-pkg writes");
    let edited = PLIST.replace("<string>tierstore</string>", "<string>other</string>");
    assert_eq!(
        plist(&edited),
        [
            bytes.clone(),
            format!("/{PLIST_FILE}: the label is Some(\"other\"), not \"tierstore\"")
        ]
    );
    let moved = PLIST.replace(
        "<string>/Library/Application Support/tierstore/bin/tierstore</string>",
        "<string>/usr/local/bin/tierstore</string>",
    );
    assert_eq!(
        plist(&moved),
        [
            bytes.clone(),
            format!(
                "/{PLIST_FILE}: the job runs Some(\"/usr/local/bin/tierstore\"), which is not a \
                 file of the payload"
            )
        ]
    );
    assert_eq!(
        plist("not a plist"),
        [
            bytes.clone(),
            format!(
                "/{PLIST_FILE} does not parse as a plist: {}",
                roxmltree::Document::parse("not a plist").expect_err("not XML")
            )
        ]
    );
    assert_eq!(
        plist("<plist version=\"1.0\"><array/></plist>"),
        [
            bytes.clone(),
            format!("/{PLIST_FILE} is not a plist dictionary")
        ]
    );
    assert_eq!(
        plist(
            "<plist version=\"1.0\"><dict><key>Label</key><string>tierstore</string></dict></plist>"
        ),
        [
            bytes,
            format!("/{PLIST_FILE}: the job runs None, which is not a file of the payload")
        ]
    );
}

#[test]
fn an_unreadable_directory_binary_or_payload_is_an_error_naming_it_and_a_bad_version_is_usage() {
    // Catches: a check that passes when it could not read what it checks.
    let expanded = Expanded::good("macos-pkg-check-io");
    assert!(matches!(
        check(&expanded.dir.join("absent"), VERSION, &expanded.dir.join("tierstore")),
        Err(Error::Spawn { what, .. }) if what.starts_with("read ")
    ));
    assert!(matches!(
        check(&expanded.product(), VERSION, &expanded.dir.join("absent")),
        Err(Error::Spawn { what, .. }) if what.ends_with("absent")
    ));
    assert_eq!(
        check(&expanded.product(), "1.2", &expanded.dir.join("tierstore")),
        Err(Error::Usage(
            "--version \"1.2\" is not <major>.<minor>.<patch>".to_owned()
        ))
    );
    // A Payload that is a file nobody may read is an I/O error naming it (a user that reads
    // it anyway, root, finds the pkg good).
    let unreadable = Expanded::good("macos-pkg-check-unreadable");
    let payload = unreadable.pkg().join("Payload");
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o000)).expect("mode");
    let readable = std::fs::File::open(&payload).is_ok();
    match unreadable.check() {
        Ok(_) => assert!(readable),
        Err(Error::Spawn { what, .. }) => assert!(!readable && what.ends_with("Payload")),
        other => panic!("{other:?}"),
    }
}

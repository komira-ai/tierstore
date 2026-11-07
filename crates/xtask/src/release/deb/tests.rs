use std::io::Read;
use std::path::PathBuf;

use super::*;
use crate::testdir::TestDir;

/// A stand-in binary and notice.
const BINARY: &[u8] = b"\x7fELF stand-in for tierstore 0.4.0";
const NOTICE: &[u8] = b"THIRD-PARTY LICENSES\n";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-deb-unit"), name)
}

/// A file of `dir` holding `bytes`.
fn put(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write the file");
    path
}

/// The members of an `ar` archive: name, and content without the padding.
fn ar_members(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    assert_eq!(&bytes[..8], b"!<arch>\n");
    let mut members = Vec::new();
    let mut at = 8;
    while at < bytes.len() {
        let header = &bytes[at..at + 60];
        assert_eq!(&header[58..], b"`\n");
        let name = String::from_utf8(header[..16].to_vec())
            .expect("a name")
            .trim_end()
            .to_owned();
        let len: usize = String::from_utf8(header[48..58].to_vec())
            .expect("a length")
            .trim_end()
            .parse()
            .expect("a number");
        members.push((name, bytes[at + 60..at + 60 + len].to_vec()));
        at += 60 + len + len % 2;
    }
    members
}

/// One entry of a tar: path, type, mode, owner, group, owner name, group name, time, content.
#[derive(Debug, PartialEq, Eq)]
struct Item {
    path: String,
    directory: bool,
    mode: u32,
    uid: u64,
    gid: u64,
    user: String,
    group: String,
    mtime: u64,
    content: Vec<u8>,
}

/// The entries of the gzip-compressed tar `tgz`, and its gzip header's time.
fn items(tgz: &[u8]) -> (Vec<Item>, u32) {
    let mut decoder = flate2::read::GzDecoder::new(tgz);
    let mut tar = Vec::new();
    decoder.read_to_end(&mut tar).expect("gzip");
    let header = decoder.header().expect("a gzip header");
    assert_eq!(header.filename(), None);
    let time = header.mtime();
    let mut archive = tar::Archive::new(&tar[..]);
    let mut out = Vec::new();
    for entry in archive.entries().expect("a tar") {
        let mut entry = entry.expect("an entry");
        let h = entry.header().clone();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).expect("content");
        out.push(Item {
            path: String::from_utf8_lossy(&entry.path_bytes()).into_owned(),
            directory: h.entry_type() == tar::EntryType::Directory,
            mode: h.mode().expect("a mode"),
            uid: h.uid().expect("an owner"),
            gid: h.gid().expect("a group"),
            user: h.username().expect("utf-8").unwrap_or_default().to_owned(),
            group: h.groupname().expect("utf-8").unwrap_or_default().to_owned(),
            mtime: h.mtime().expect("a time"),
            content,
        });
    }
    (out, time)
}

/// The package of the stand-in binary for `amd64`.
fn package() -> Vec<u8> {
    build("0.4.0", "amd64", &files(BINARY, NOTICE)).expect("the package")
}

#[test]
fn a_deb_is_an_ar_archive_of_debian_binary_control_and_data_in_that_order() {
    // Catches: members in another order or under other names, which dpkg refuses, and a header
    // whose columns are off.
    let bytes = package();
    let members = ar_members(&bytes);
    let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["debian-binary", "control.tar.gz", "data.tar.gz"]);
    assert_eq!(members[0].1, b"2.0\n");
    assert_eq!(
        String::from_utf8_lossy(&bytes[8..68]),
        "debian-binary   1788220800  0     0     100644  4         `\n"
    );
}

#[test]
fn an_ar_member_of_odd_length_is_padded_with_a_newline_and_an_even_one_is_not() {
    // Catches: an odd member not padded (the next header would start a byte early), and an even
    // one padded.
    let odd = ar(&[("a", b"abc"), ("b", b"de")]).expect("ar");
    assert_eq!(odd.len(), 8 + 60 + 3 + 1 + 60 + 2);
    assert_eq!(odd[8 + 60 + 3], b'\n');
    let members = ar_members(&odd);
    assert_eq!(members[0].1, b"abc");
    assert_eq!(members[1], ("b".to_owned(), b"de".to_vec()));
}

#[test]
fn an_ar_header_holds_ten_digits_of_size_and_no_more() {
    // Catches: a member too big for its header written with a size that runs into the magic.
    assert_eq!(ar_header("x", 9_999_999_999).expect("fits").len(), 60);
    let error = ar_header("x", 10_000_000_000).expect_err("too big");
    assert_eq!(
        error,
        Error::Failed("x is 10000000000 bytes, more than an ar header holds".to_owned())
    );
}

#[test]
fn the_data_holds_each_file_at_its_path_and_mode_owned_by_root_with_directories_first() {
    // Catches: a file at another path or mode than docs/install.md places it, a directory after
    // what is in it, an owner that is not root, a time that is not the fixed one.
    let members = ar_members(&package());
    let (items, gzip_time) = items(&members[2].1);
    assert_eq!(gzip_time, MTIME);
    let listed: Vec<(&str, bool, u32)> = items
        .iter()
        .map(|i| (i.path.as_str(), i.directory, i.mode))
        .collect();
    assert_eq!(
        listed,
        [
            ("./", true, 0o755),
            ("./etc/", true, 0o755),
            ("./etc/tierstore/", true, 0o750),
            ("./etc/tierstore/tierstore.toml", false, 0o640),
            ("./usr/", true, 0o755),
            ("./usr/lib/", true, 0o755),
            ("./usr/lib/systemd/", true, 0o755),
            ("./usr/lib/systemd/system/", true, 0o755),
            ("./usr/lib/systemd/system/tierstore.service", false, 0o644),
            ("./usr/lib/sysusers.d/", true, 0o755),
            ("./usr/lib/sysusers.d/tierstore.conf", false, 0o644),
            ("./usr/libexec/", true, 0o755),
            ("./usr/libexec/tierstore/", true, 0o755),
            ("./usr/libexec/tierstore/tierstore", false, 0o755),
            ("./usr/share/", true, 0o755),
            ("./usr/share/doc/", true, 0o755),
            ("./usr/share/doc/tierstore/", true, 0o755),
            (
                "./usr/share/doc/tierstore/THIRD-PARTY-LICENSES.txt",
                false,
                0o644
            ),
            ("./usr/share/doc/tierstore/copyright", false, 0o644),
        ]
    );
    for item in &items {
        assert_eq!(
            (item.uid, item.gid, item.user.as_str(), item.group.as_str()),
            (0, 0, "root", "root"),
            "{}",
            item.path
        );
        assert_eq!(item.mtime, u64::from(MTIME), "{}", item.path);
    }
}

#[test]
fn the_files_hold_the_binary_the_packaging_files_the_licence_and_the_notice() {
    // Catches: a stale copy of a packaging file (the unit, the sysusers entry, the example
    // configuration), the binary changed on its way in, and the notice or the licence left out.
    let members = ar_members(&package());
    let (items, _) = items(&members[2].1);
    let content = |path: &str| {
        items
            .iter()
            .find(|i| i.path == path)
            .unwrap_or_else(|| panic!("{path} is not in the package"))
            .content
            .clone()
    };
    let packaging = |name: &str| {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../packaging")
                .join(name),
        )
        .expect("a packaging file")
    };
    assert_eq!(content("./usr/libexec/tierstore/tierstore"), BINARY);
    assert_eq!(
        content("./usr/lib/systemd/system/tierstore.service"),
        packaging("tierstore.service")
    );
    assert_eq!(
        content("./usr/lib/sysusers.d/tierstore.conf"),
        packaging("tierstore.conf")
    );
    assert_eq!(
        content("./etc/tierstore/tierstore.toml"),
        packaging("tierstore.toml")
    );
    assert_eq!(
        content("./usr/share/doc/tierstore/copyright"),
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../LICENSE"))
            .expect("the licence")
    );
    assert_eq!(
        content("./usr/share/doc/tierstore/THIRD-PARTY-LICENSES.txt"),
        NOTICE
    );
}

#[test]
fn the_unit_runs_the_installed_binary_as_the_account_the_scripts_create() {
    // Catches: a unit whose ExecStart is not where the package puts the binary, and scripts that
    // name an account the sysusers entry does not create.
    assert!(UNIT.contains("ExecStart=/usr/libexec/tierstore/tierstore "));
    assert!(UNIT.contains("User=tierstore\n"));
    assert!(SYSUSERS.contains("\nu tierstore "));
    assert!(EXAMPLE_CONFIG.contains("/etc/tierstore/access.key"));
    assert!(
        files(BINARY, NOTICE)
            .iter()
            .any(|(path, _, _)| format!("/{path}") == CONFFILE)
    );
    assert!(POSTINST.contains("chgrp tierstore /etc/tierstore /etc/tierstore/tierstore.toml"));
}

#[test]
fn the_control_file_names_the_package_its_version_architecture_and_size_in_kibibytes() {
    // Catches: a field out of order or missing, an Installed-Size rounded down, a description
    // line that dpkg would read as a new field.
    let files = vec![
        ("usr/a".to_owned(), 0o644, vec![0; 2049]),
        ("usr/b".to_owned(), 0o644, vec![0; 1]),
    ];
    assert_eq!(
        control("1.2.3", "arm64", &files),
        "Package: tierstore\nVersion: 1.2.3\nArchitecture: arm64\n\
         Maintainer: tierstore authors <tierstore@users.noreply.github.com>\n\
         Installed-Size: 3\nDepends: systemd\nSection: admin\nPriority: optional\n\
         Homepage: https://tierstore.dev\n\
         Description: S3 server over a hot and a cold tier of S3-compatible stores\n \
         New objects are written to the hot tier, copied to the cold tier in the background,\n \
         and freed from the hot tier when it fills.\n .\n \
         The package installs the binary, the systemd unit and an example configuration. It starts\n \
         and enables nothing: write the key files under /etc/tierstore, then start tierstore.service.\n"
    );
    assert!(control("1.2.3", "arm64", &[]).contains("Installed-Size: 0\n"));
}

#[test]
fn the_control_archive_holds_the_control_file_the_one_conffile_the_sums_and_the_scripts() {
    // Catches: a missing conffile (an upgrade would overwrite the host's edits), md5sums that do
    // not match the files, and scripts that are not executable.
    let members = ar_members(&package());
    let (items, gzip_time) = items(&members[1].1);
    assert_eq!(gzip_time, MTIME);
    let listed: Vec<(&str, u32)> = items.iter().map(|i| (i.path.as_str(), i.mode)).collect();
    assert_eq!(
        listed,
        [
            ("./", 0o755),
            ("./control", 0o644),
            ("./conffiles", 0o644),
            ("./md5sums", 0o644),
            ("./postinst", 0o755),
            ("./postrm", 0o755),
        ]
    );
    let text = |n: usize| String::from_utf8(items[n].content.clone()).expect("text");
    assert_eq!(text(1), control("0.4.0", "amd64", &files(BINARY, NOTICE)));
    assert_eq!(text(2), "/etc/tierstore/tierstore.toml\n");
    assert_eq!(text(3), md5sums(&files(BINARY, NOTICE)));
    assert_eq!(text(4), POSTINST);
    assert_eq!(text(5), POSTRM);
    assert!(items.iter().all(|i| i.user == "root" && i.group == "root"));
}

#[test]
fn md5sums_lists_each_file_in_path_order_with_the_published_test_vectors() {
    // Catches: a wrong digest, a digest of the path instead of the content, a list in the order
    // the files were given.
    let files = vec![
        ("usr/share/b".to_owned(), 0o644, Vec::new()),
        ("usr/bin/a".to_owned(), 0o755, b"abc".to_vec()),
    ];
    assert_eq!(
        md5sums(&files),
        "900150983cd24fb0d6963f7d28e17f72  usr/bin/a\n\
         d41d8cd98f00b204e9800998ecf8427e  usr/share/b\n"
    );
}

#[test]
fn the_scripts_are_valid_sh_and_start_and_enable_nothing() {
    // Catches: a syntax error that only a host's dpkg would find, and a script that starts or
    // enables the unit, which must wait for the keys under /etc/tierstore.
    let dir = scratch("scripts");
    for (name, text) in [("postinst", POSTINST), ("postrm", POSTRM)] {
        let path = put(&dir, name, text.as_bytes());
        let status = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .status()
            .expect("sh");
        assert!(status.success(), "{name} is not valid sh");
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            for word in ["start", "enable", "restart", "reload-or", "--now", "try-"] {
                assert!(!line.contains(word), "{name}: {line}");
            }
        }
    }
    let calls: Vec<&str> = POSTINST
        .lines()
        .filter(|l| l.contains("systemctl") || l.contains("systemd-sysusers"))
        .map(str::trim)
        .collect();
    assert_eq!(
        calls,
        [
            "systemd-sysusers /usr/lib/sysusers.d/tierstore.conf",
            "systemctl daemon-reload"
        ]
    );
}

#[test]
fn the_same_inputs_give_the_same_bytes_and_each_input_changes_them() {
    // Catches: a time, a path or an ordering that makes two builds of one commit differ, and an
    // input (binary, version, architecture, notice) that the package ignores.
    let one = package();
    assert_eq!(one, package());
    let other = |version: &str, architecture: &str, binary: &[u8], notice: &[u8]| {
        build(version, architecture, &files(binary, notice)).expect("the package")
    };
    assert_ne!(one, other("0.4.1", "amd64", BINARY, NOTICE));
    assert_ne!(one, other("0.4.0", "arm64", BINARY, NOTICE));
    assert_ne!(one, other("0.4.0", "amd64", b"other", NOTICE));
    assert_ne!(one, other("0.4.0", "amd64", BINARY, b"other"));
    let mut reversed = files(BINARY, NOTICE);
    reversed.reverse();
    assert_eq!(
        one,
        build("0.4.0", "amd64", &reversed).expect("the package")
    );
}

#[test]
fn a_path_too_long_for_a_tar_header_is_refused() {
    // Catches: a path that does not fit being written truncated.
    let long = vec![("a/".repeat(80), 0o644, b"x".to_vec())];
    let error = build("0.4.0", "amd64", &long).expect_err("too long");
    assert!(
        matches!(&error, Error::Failed(why) if why.ends_with("is longer than the 100 bytes of a tar entry name")),
        "{error:?}"
    );
    let exact = vec![("a/".repeat(47) + "bcde", 0o644, b"x".to_vec())];
    let (items, _) = items(&ar_members(&build("0.4.0", "amd64", &exact).expect("fits"))[2].1);
    assert_eq!(items.last().expect("a file").path.len(), 100);
}

#[test]
fn only_the_linux_platforms_have_an_architecture_and_a_file_name() {
    // Catches: the wrong Debian architecture for a platform, and a package for macOS.
    assert_eq!(architecture(Platform::LinuxX86_64), Some("amd64"));
    assert_eq!(architecture(Platform::LinuxArm64), Some("arm64"));
    assert_eq!(architecture(Platform::MacosArm64), None);
    assert_eq!(file_name("0.4.0", "amd64"), "tierstore_0.4.0_amd64.deb");
}

#[test]
fn the_verb_writes_the_package_and_its_sums_line() {
    // Catches: a package written without its line in SHA256SUMS.<platform>, a line for other
    // bytes than the file's, and a second line clobbering the first platform file's.
    let dir = scratch("verb");
    let binary = put(&dir, "tierstore", BINARY);
    let notice = put(&dir, "notice", NOTICE);
    let out = dir.join("dist");
    std::fs::create_dir_all(&out).expect("out");
    std::fs::write(
        out.join("SHA256SUMS.linux-arm64"),
        format!("{}  earlier\n", "0".repeat(64)),
    )
    .expect("earlier sums");
    let printed = deb(&binary, "0.4.0", Platform::LinuxArm64, &notice, &out).expect("deb");
    let bytes = std::fs::read(out.join("tierstore_0.4.0_arm64.deb")).expect("the package");
    assert_eq!(
        bytes,
        build("0.4.0", "arm64", &files(BINARY, NOTICE)).expect("the package")
    );
    let digest = Sha256::of(&bytes);
    assert_eq!(
        std::fs::read_to_string(out.join("SHA256SUMS.linux-arm64")).expect("sums"),
        format!(
            "{}  earlier\n{digest}  tierstore_0.4.0_arm64.deb\n",
            "0".repeat(64)
        )
    );
    assert_eq!(
        printed,
        [format!(
            "tierstore_0.4.0_arm64.deb: sha256 {digest}, {} bytes",
            bytes.len()
        )]
    );
}

#[test]
fn the_verb_creates_its_output_directory() {
    // Catches: a first build job whose dist/ does not exist yet failing.
    let dir = scratch("verb-new-out");
    let binary = put(&dir, "tierstore", BINARY);
    let notice = put(&dir, "notice", NOTICE);
    let out = dir.join("a/dist");
    deb(&binary, "0.4.0", Platform::LinuxX86_64, &notice, &out).expect("deb");
    assert!(out.join("tierstore_0.4.0_amd64.deb").is_file());
    assert!(out.join("SHA256SUMS.linux-x86_64").is_file());
}

#[test]
fn the_verb_refuses_what_it_cannot_package_and_writes_nothing() {
    // Catches: a version with a commit or a leading zero, a macOS package, an empty notice, a
    // package or a sums line there already, and a missing input, each of which would otherwise
    // give a file the release check refuses later.
    let dir = scratch("verb-refused");
    let binary = put(&dir, "tierstore", BINARY);
    let notice = put(&dir, "notice", NOTICE);
    let empty = put(&dir, "empty", b"");
    let out = dir.join("dist");
    let run = |version: &str, platform: Platform, binary: &Path, notice: &Path| {
        deb(binary, version, platform, notice, &out)
    };
    for bad in ["0.4.0+0123456789ab", "0.4", "01.2.3", "v0.4.0", ""] {
        let error = run(bad, Platform::LinuxArm64, &binary, &notice).expect_err("a bad version");
        assert_eq!(
            error,
            Error::Usage(format!("--version {bad:?} is not <major>.<minor>.<patch>"))
        );
    }
    assert_eq!(
        run("0.4.0", Platform::MacosArm64, &binary, &notice),
        Err(Error::Usage(
            "--platform macos-arm64: only the Linux platforms have a Debian package".to_owned()
        ))
    );
    assert_eq!(
        run("0.4.0", Platform::LinuxArm64, &binary, &empty),
        Err(Error::Failed("the licence notice is empty".to_owned()))
    );
    for missing in [dir.join("no-binary"), dir.join("no-notice")] {
        let (b, n) = if missing.ends_with("no-binary") {
            (&missing, &notice)
        } else {
            (&binary, &missing)
        };
        let error = run("0.4.0", Platform::LinuxArm64, b, n).expect_err("a missing input");
        assert!(matches!(error, Error::Spawn { .. }), "{error:?}");
    }
    assert!(!out.exists());
    run("0.4.0", Platform::LinuxArm64, &binary, &notice).expect("deb");
    let again = run("0.4.0", Platform::LinuxArm64, &binary, &notice).expect_err("twice");
    assert!(
        again
            .to_string()
            .contains("already lists tierstore_0.4.0_arm64.deb"),
        "{again}"
    );
    std::fs::remove_file(out.join("tierstore_0.4.0_arm64.deb")).expect("remove");
    let sums_only = run("0.4.0", Platform::LinuxArm64, &binary, &notice).expect_err("a line");
    assert!(
        sums_only.to_string().contains("already lists"),
        "{sums_only}"
    );
    std::fs::remove_file(out.join("SHA256SUMS.linux-arm64")).expect("remove");
    put(&out, "tierstore_0.4.0_arm64.deb", b"in the way");
    let exists = run("0.4.0", Platform::LinuxArm64, &binary, &notice).expect_err("a file");
    assert!(matches!(exists, Error::Spawn { .. }), "{exists:?}");
}

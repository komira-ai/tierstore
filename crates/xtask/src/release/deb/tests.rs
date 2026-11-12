use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::*;
use crate::release::script_host::{GETENT_CALL, Host, RELOAD_CALL, SYSUSERS_CALL};
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
    // Catches: a file at another path or mode than the table of the module says, anything below
    // /etc (whose files are the host's, and which a purge would delete), a directory after what is
    // in it, an owner that is not root, a time that is not the fixed one.
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
            ("./usr/", true, 0o755),
            ("./usr/lib/", true, 0o755),
            ("./usr/lib/systemd/", true, 0o755),
            ("./usr/lib/systemd/system/", true, 0o755),
            ("./usr/lib/systemd/system/tierstore@.service", false, 0o644),
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
            ("./usr/share/doc/tierstore/examples/", true, 0o755),
            (
                "./usr/share/doc/tierstore/examples/config.toml",
                false,
                0o644
            ),
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
        content("./usr/lib/systemd/system/tierstore@.service"),
        packaging("tierstore@.service")
    );
    assert_eq!(
        content("./usr/lib/sysusers.d/tierstore.conf"),
        packaging("tierstore.conf")
    );
    assert_eq!(
        content("./usr/share/doc/tierstore/examples/config.toml"),
        packaging("config.toml")
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

/// The values of the `key=` lines of a unit, its `\` continuations joined.
fn unit_values(unit: &str, key: &str) -> Vec<String> {
    let prefix = format!("{key}=");
    unit.replace("\\\n", " ")
        .lines()
        .filter_map(|line| line.strip_prefix(prefix.as_str()))
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_template_runs_the_installed_binary_on_the_hosts_file_as_the_account_the_scripts_create() {
    // Catches: a template whose ExecStart is not where the package puts the binary or reads
    // another file than the host's /etc/tierstore/config.toml (the example, say), an instance
    // that starts without the start marker the update hook writes (a fresh install would start a
    // server with no configuration), and a template that runs as an account the sysusers entry
    // does not create.
    let exec: Vec<String> = unit_values(UNIT, "ExecStart")
        .iter()
        .flat_map(|v| v.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        .collect();
    assert_eq!(
        exec,
        [
            format!("/{BINARY_PATH}"),
            "--config".to_owned(),
            CONFIG_FILE.to_owned()
        ]
    );
    assert!(
        files(BINARY, NOTICE)
            .iter()
            .any(|(path, mode, _)| path == BINARY_PATH && *mode == 0o755)
    );
    assert_eq!(
        unit_values(UNIT, "ConditionPathExists"),
        ["/var/lib/tierstore/run-%i"]
    );
    assert_eq!(unit_values(UNIT, "User"), ["tierstore"]);
    assert_eq!(unit_values(UNIT, "StateDirectory"), ["tierstore"]);
    assert_eq!(unit_values(UNIT, "WantedBy"), ["multi-user.target"]);
    assert!(SYSUSERS.contains("\nu tierstore "));
    assert!(EXAMPLE_CONFIG.contains("/etc/tierstore/access.key"));
    assert!(
        files(BINARY, NOTICE)
            .iter()
            .all(|(path, _, _)| !path.starts_with("etc/") && !path.starts_with("var/")),
        "the package installs nothing in /etc or /var"
    );
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
         Installed-Size: 3\nDepends: systemd, libc6 (>= 2.28)\nSection: admin\nPriority: optional\n\
         Homepage: https://tierstore.dev\n\
         Description: S3 server over a hot and a cold tier of S3-compatible stores\n \
         New objects are written to the hot tier, copied to the cold tier in the background,\n \
         and freed from the hot tier when it fills.\n .\n \
         The package installs the binary, the systemd unit template tierstore@.service and an example\n \
         configuration. It starts and enables nothing: write /etc/tierstore/config.toml and its keys.\n"
    );
    assert!(control("1.2.3", "arm64", &[]).contains("Installed-Size: 0\n"));
}

#[test]
fn the_control_archive_holds_the_control_file_the_sums_and_the_scripts_and_no_conffile() {
    // Catches: a conffile (the package has no file in /etc, and a purge would delete the host's
    // file of that name), md5sums that do not match the files, and scripts that are not
    // executable.
    let members = ar_members(&package());
    let (items, gzip_time) = items(&members[1].1);
    assert_eq!(gzip_time, MTIME);
    let listed: Vec<(&str, u32)> = items.iter().map(|i| (i.path.as_str(), i.mode)).collect();
    assert_eq!(
        listed,
        [
            ("./", 0o755),
            ("./control", 0o644),
            ("./md5sums", 0o644),
            ("./postinst", 0o755),
            ("./postrm", 0o755),
        ]
    );
    let text = |n: usize| String::from_utf8(items[n].content.clone()).expect("text");
    assert_eq!(text(1), control("0.4.0", "amd64", &files(BINARY, NOTICE)));
    assert_eq!(text(2), md5sums(&files(BINARY, NOTICE)));
    assert_eq!(text(3), POSTINST);
    assert_eq!(text(4), POSTRM);
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

#[test]
fn the_package_depends_on_the_glibc_the_binary_is_linked_against() {
    // Catches: a control file that lets `apt` install the package on a host whose glibc is older
    // than the binary's (it then fails to start with "GLIBC_2.xx not found"): the dependency
    // is `libc6 (>= 2.28)`, the floor `release check-glibc` holds the binary to.
    assert_eq!(glibc::FLOOR, "2.28");
    let depends = control("1.2.3", "amd64", &[])
        .lines()
        .find(|l| l.starts_with("Depends:"))
        .map(str::to_owned);
    assert_eq!(
        depends.as_deref(),
        Some("Depends: systemd, libc6 (>= 2.28)")
    );
}

#[test]
fn postinst_configure_on_a_fresh_host_makes_the_config_directory_and_starts_nothing() {
    // Catches: a postinst that does not make /etc/tierstore (root, group tierstore, 0750) on a
    // host that has none, one that writes a configuration (the example as the live file) or a
    // start marker, so that an instance would start with no configuration of the host's, and one
    // that starts, enables or restarts a unit.
    let host = Host::new("postinst-fresh", true, true);
    let (ok, calls) = host.run(POSTINST, &["configure", ""]);
    assert!(ok);
    let dir = host.root().join("etc/tierstore");
    assert_eq!(
        calls,
        [
            SYSUSERS_CALL.to_owned(),
            GETENT_CALL.to_owned(),
            format!("chgrp tierstore {}", dir.display()),
            RELOAD_CALL.to_owned(),
        ]
    );
    assert_eq!(host.config_dir_mode(), Some(0o750));
    assert!(dir.symlink_metadata().expect("made").is_dir());
    assert_eq!(std::fs::read_dir(&dir).expect("read").count(), 0);
    assert!(!host.root().join("var").exists(), "no start marker");
}

#[test]
fn postinst_leaves_a_config_directory_that_is_there_as_it_is() {
    // Catches: a postinst that changes the group or the mode of a host's /etc/tierstore, or a
    // file in it, on an install or an upgrade (the farm's files there are root's and its own),
    // and one that fails on a link there (a dangling one included).
    let host = Host::new("postinst-there", true, true);
    let dir = host.root().join("etc/tierstore");
    std::fs::create_dir_all(&dir).expect("the host's directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
    put(&dir, "config.toml", b"# the host's\n");
    for version in ["", "0.4.0"] {
        let (ok, calls) = host.run(POSTINST, &["configure", version]);
        assert!(ok);
        assert_eq!(calls, [SYSUSERS_CALL, RELOAD_CALL]);
        assert_eq!(host.config_dir_mode(), Some(0o700));
        assert_eq!(
            std::fs::read(dir.join("config.toml")).expect("kept"),
            b"# the host's\n"
        );
    }
    let host = Host::new("postinst-link", true, true);
    std::fs::create_dir_all(host.root().join("etc")).expect("etc");
    std::os::unix::fs::symlink("/nonexistent-tierstore", host.root().join("etc/tierstore"))
        .expect("a dangling link");
    let (ok, calls) = host.run(POSTINST, &["configure", ""]);
    assert!(ok);
    assert_eq!(calls, [SYSUSERS_CALL, RELOAD_CALL]);
    assert!(
        host.root()
            .join("etc/tierstore")
            .symlink_metadata()
            .expect("kept")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn postinst_makes_no_directory_without_the_group_and_no_reload_without_systemd() {
    // Catches: a postinst that fails the install when the account's group is not named
    // tierstore (a host whose account was made by hand), one that makes a directory the service
    // could not read, and one that calls systemctl in a container or chroot with no systemd.
    let host = Host::new("postinst-no-group", true, false);
    let (ok, calls) = host.run(POSTINST, &["configure", ""]);
    assert!(ok);
    assert_eq!(calls, [SYSUSERS_CALL, GETENT_CALL, RELOAD_CALL]);
    assert_eq!(host.config_dir_mode(), None);
    let host = Host::new("postinst-no-systemd", false, true);
    let (ok, calls) = host.run(POSTINST, &["configure", ""]);
    assert!(ok);
    assert_eq!(
        calls,
        [
            SYSUSERS_CALL.to_owned(),
            GETENT_CALL.to_owned(),
            format!(
                "chgrp tierstore {}",
                host.root().join("etc/tierstore").display()
            ),
        ]
    );
}

#[test]
fn postinst_does_nothing_but_on_configure() {
    // Catches: a postinst that acts on the unwinding of a failed upgrade or removal, where dpkg
    // asks it to put back what was there and nothing else.
    let host = Host::new("postinst-abort", true, true);
    for action in [
        "abort-upgrade",
        "abort-remove",
        "abort-deconfigure",
        "triggered",
    ] {
        let (ok, calls) = host.run(POSTINST, &[action, "0.4.0"]);
        assert!(ok, "{action}");
        assert!(calls.is_empty(), "{action}: {calls:?}");
    }
    assert_eq!(host.config_dir_mode(), None);
}

#[test]
fn postrm_reloads_systemd_on_remove_and_purge_and_leaves_the_host_files() {
    // Catches: a postrm that deletes /etc/tierstore or a file in it (the host's configuration
    // and keys) on a purge, one that reloads on an upgrade, and one that calls systemctl with no
    // systemd.
    let host = Host::new("postrm", true, true);
    let dir = host.root().join("etc/tierstore");
    std::fs::create_dir_all(&dir).expect("the host's directory");
    put(&dir, "config.toml", b"# the host's\n");
    for action in ["remove", "purge"] {
        let (ok, calls) = host.run(POSTRM, &[action]);
        assert!(ok, "{action}");
        assert_eq!(calls, [RELOAD_CALL], "{action}");
    }
    for action in ["upgrade", "failed-upgrade", "abort-install", "disappear"] {
        let (ok, calls) = host.run(POSTRM, &[action, "0.4.0"]);
        assert!(ok, "{action}");
        assert!(calls.is_empty(), "{action}: {calls:?}");
    }
    assert!(dir.join("config.toml").is_file());
    let host = Host::new("postrm-no-systemd", false, true);
    let (ok, calls) = host.run(POSTRM, &["purge"]);
    assert!(ok);
    assert!(calls.is_empty(), "{calls:?}");
}

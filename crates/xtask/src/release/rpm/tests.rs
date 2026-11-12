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
    TestDir::under(deps.join("xtask-rpm-unit"), name)
}

/// A file of `dir` holding `bytes`.
fn put(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write the file");
    path
}

/// The package of the stand-in binary for `x86_64`.
fn package() -> Vec<u8> {
    build("0.4.0", "x86_64", &files(BINARY, NOTICE)).expect("the package")
}

/// The package `bytes` as the `rpm` crate reads it.
fn parsed(bytes: &[u8]) -> Package {
    Package::parse(&mut &bytes[..]).expect("a package")
}

/// The content of the file `path` of the payload of `bytes`.
fn content(bytes: &[u8], path: &str) -> Vec<u8> {
    for file in parsed(bytes).files().expect("the payload") {
        let file = file.expect("a file");
        if file.metadata.path() == Path::new(path) {
            return file.content().expect("content").to_vec();
        }
    }
    panic!("{path} is not in the package");
}

#[test]
fn the_package_names_the_release_and_architecture_in_its_header() {
    // Catches: a name, version, release or architecture other than the file name says, which
    // `dnf` would take for another package; a payload that is not gzip, which an EL8 `rpm`
    // reads but a zstd or xz one it may not; a dependency other than `systemd` and the shell of
    // the scriptlets (a `config(tierstore)` would mean a %config file, and the package has none).
    let info = inspect(&package()).expect("inspect");
    assert_eq!(
        info.nevra,
        (
            "tierstore".to_owned(),
            "0.4.0".to_owned(),
            "1".to_owned(),
            "x86_64".to_owned()
        )
    );
    assert_eq!(info.compression, "gzip");
    assert_eq!(info.requires, ["/bin/sh", "glibc >= 2.28", "systemd"]);
    let other = inspect(&build("0.4.1", "aarch64", &files(BINARY, NOTICE)).expect("build"))
        .expect("inspect");
    assert_eq!(other.nevra.1, "0.4.1");
    assert_eq!(other.nevra.3, "aarch64");
}

#[test]
fn the_package_lists_the_files_of_the_deb_at_the_same_modes_owned_by_root() {
    // Catches: a file at another path or mode than the Debian package places it, an owner that
    // is not root, a directory the package does not own, anything in /etc (whose files are the
    // host's, which an erase would remove) or a %config file, and the licence not marked as
    // %license.
    let entry = |path: &str, mode: u16, directory: bool, flags: (bool, bool, bool)| Entry {
        path: path.to_owned(),
        mode,
        directory,
        owner: ("root".to_owned(), "root".to_owned()),
        config: flags.0,
        noreplace: flags.1,
        license: flags.2,
    };
    let none = (false, false, false);
    assert_eq!(
        inspect(&package()).expect("inspect").entries,
        [
            entry(
                "/usr/lib/systemd/system/tierstore@.service",
                0o644,
                false,
                none
            ),
            entry("/usr/lib/sysusers.d/tierstore.conf", 0o644, false, none),
            entry("/usr/libexec/tierstore", 0o755, true, none),
            entry("/usr/libexec/tierstore/tierstore", 0o755, false, none),
            entry("/usr/share/doc/tierstore", 0o755, true, none),
            entry(
                "/usr/share/doc/tierstore/THIRD-PARTY-LICENSES.txt",
                0o644,
                false,
                none
            ),
            entry("/usr/share/doc/tierstore/examples", 0o755, true, none),
            entry(
                "/usr/share/doc/tierstore/examples/config.toml",
                0o644,
                false,
                none
            ),
            entry("/usr/share/licenses/tierstore", 0o755, true, none),
            entry(
                "/usr/share/licenses/tierstore/LICENSE",
                0o644,
                false,
                (false, false, true)
            ),
        ]
    );
    // The same set of files as the Debian package, but for the licence's place.
    let mut deb_paths: Vec<String> = deb::files(BINARY, NOTICE)
        .into_iter()
        .map(|(path, _, _)| format!("/{path}"))
        .filter(|path| !path.ends_with("/copyright"))
        .collect();
    deb_paths.push(format!("/{LICENSE_PATH}"));
    deb_paths.sort();
    let regular: Vec<String> = inspect(&package())
        .expect("inspect")
        .entries
        .into_iter()
        .filter(|e| !e.directory)
        .map(|e| e.path)
        .collect();
    assert_eq!(regular, deb_paths);
}

#[test]
fn the_payload_holds_the_binary_the_packaging_files_the_licence_and_the_notice() {
    // Catches: a stale copy of a packaging file (the unit, the sysusers entry, the example
    // configuration), the binary changed on its way in, and the notice or the licence left out.
    let bytes = package();
    let packaging = |name: &str| {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../packaging")
                .join(name),
        )
        .expect("a packaging file")
    };
    assert_eq!(content(&bytes, "/usr/libexec/tierstore/tierstore"), BINARY);
    assert_eq!(
        content(&bytes, "/usr/lib/systemd/system/tierstore@.service"),
        packaging("tierstore@.service")
    );
    assert_eq!(
        content(&bytes, "/usr/lib/sysusers.d/tierstore.conf"),
        packaging("tierstore.conf")
    );
    assert_eq!(
        content(&bytes, "/usr/share/doc/tierstore/examples/config.toml"),
        packaging("config.toml")
    );
    assert_eq!(
        content(&bytes, "/usr/share/licenses/tierstore/LICENSE"),
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../LICENSE"))
            .expect("the licence")
    );
    assert_eq!(
        content(&bytes, "/usr/share/doc/tierstore/THIRD-PARTY-LICENSES.txt"),
        NOTICE
    );
}

#[test]
fn the_scriptlets_run_with_sh_and_start_and_enable_nothing() {
    // Catches: a scriptlet that is not valid sh (only a host's rpm would find it), one with
    // another interpreter, one that starts or enables an instance (which must wait for the
    // host's configuration and keys under /etc/tierstore), and a %postun that reloads systemd on an upgrade, not only when
    // the last version goes.
    let info = inspect(&package()).expect("inspect");
    assert_eq!(info.post, (POST.to_owned(), vec!["/bin/sh".to_owned()]));
    assert_eq!(info.postun, (POSTUN.to_owned(), vec!["/bin/sh".to_owned()]));
    let dir = scratch("scripts");
    for (name, text) in [("post", POST), ("postun", POSTUN)] {
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
    let calls: Vec<&str> = POST
        .lines()
        .filter(|l| l.contains("systemctl") || l.contains("systemd-sysusers /"))
        .map(str::trim)
        .collect();
    assert_eq!(
        calls,
        [
            "systemd-sysusers /usr/lib/sysusers.d/tierstore.conf",
            "systemctl daemon-reload"
        ]
    );
    assert!(POSTUN.contains("[ \"$1\" -eq 0 ]"));
}

#[test]
fn the_scriptlets_do_what_the_postinst_does_where_a_host_has_systemd() {
    // Catches: a %post whose account, group, mode or condition for /etc/tierstore differs from
    // the Debian postinst's (whose behaviour its own tests run), so a host would run the same
    // unit under another account, read the keys with another mode, or have a directory of its
    // own changed by one package and not the other.
    for (deb_line, rpm_line) in [
        (
            "if [ ! -e \"$DPKG_ROOT/etc/tierstore\" ] && [ ! -L \"$DPKG_ROOT/etc/tierstore\" ] \\",
            "if [ ! -e /etc/tierstore ] && [ ! -L /etc/tierstore ] && getent group tierstore >/dev/null 2>&1; then",
        ),
        (
            "mkdir -m 0750 \"$DPKG_ROOT/etc/tierstore\"",
            "mkdir -m 0750 /etc/tierstore",
        ),
        (
            "chgrp tierstore \"$DPKG_ROOT/etc/tierstore\"",
            "chgrp tierstore /etc/tierstore",
        ),
        (
            "systemd-sysusers /usr/lib/sysusers.d/tierstore.conf",
            "systemd-sysusers /usr/lib/sysusers.d/tierstore.conf",
        ),
        ("systemctl daemon-reload", "systemctl daemon-reload"),
    ] {
        assert!(
            deb::POSTINST.lines().any(|l| l.trim() == deb_line),
            "{deb_line}"
        );
        assert!(POST.lines().any(|l| l.trim() == rpm_line), "{rpm_line}");
    }
    assert!(deb::POSTINST.contains("&& getent group tierstore >/dev/null; then"));
    for script in [deb::POSTINST, POST] {
        assert!(!script.contains("chmod"), "{script}");
        assert!(!script.contains("config.toml"), "{script}");
    }
}

#[test]
fn the_same_inputs_give_the_same_bytes_and_each_input_changes_them() {
    // Catches: a time, a host name, a path or an ordering that makes two builds of one commit
    // differ, and an input (binary, version, architecture, notice) that the package ignores.
    let one = package();
    assert_eq!(one, package());
    let other = |version: &str, architecture: &str, binary: &[u8], notice: &[u8]| {
        build(version, architecture, &files(binary, notice)).expect("the package")
    };
    assert_ne!(one, other("0.4.1", "x86_64", BINARY, NOTICE));
    assert_ne!(one, other("0.4.0", "aarch64", BINARY, NOTICE));
    assert_ne!(one, other("0.4.0", "x86_64", b"other", NOTICE));
    assert_ne!(one, other("0.4.0", "x86_64", BINARY, b"other"));
    let mut reversed = files(BINARY, NOTICE);
    reversed.reverse();
    assert_eq!(
        one,
        build("0.4.0", "x86_64", &reversed).expect("the package")
    );
}

#[test]
fn the_build_time_the_file_times_and_the_host_are_the_fixed_ones() {
    // Catches: the clock or the runner's name in the package, which would make the attested
    // bytes differ from a rebuild.
    let bytes = package();
    let info = inspect(&bytes).expect("inspect");
    assert_eq!(info.build_time, u64::from(deb::MTIME));
    assert_eq!(info.build_host, BUILD_HOST);
    let package = parsed(&bytes);
    let entries = package.metadata.get_file_entries().expect("entries");
    assert!(!entries.is_empty());
    for entry in entries {
        assert_eq!(
            u64::from(u32::from(entry.modified_at())),
            u64::from(deb::MTIME),
            "{}",
            entry.path().display()
        );
    }
}

#[test]
fn the_package_is_not_signed_and_its_digests_hold() {
    // Catches: a signature in the file (the attested bytes are the unsigned ones, a later job
    // adds the signature), and a header or payload whose digest does not match.
    let package = parsed(&package());
    package.verify_digests().expect("the digests");
    assert!(package.raw_signatures().expect("signatures").is_empty());
}

#[test]
fn a_changed_byte_in_the_payload_or_a_truncated_file_is_not_read_back() {
    // Catches: a read-back that accepts a package whose payload was altered after the build.
    let bytes = package();
    let mut flipped = bytes.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    assert!(inspect(&flipped).is_err());
    assert!(inspect(&bytes[..bytes.len() / 2]).is_err());
    assert!(inspect(b"not a package").is_err());
    assert!(inspect(b"").is_err());
}

#[test]
fn a_version_the_header_cannot_carry_is_refused() {
    // Catches: a version with a control character or a dash (which splits name, version and
    // release in a file name) being written into a header anyway.
    for bad in ["0.4\n0", "0.4-1", ""] {
        let error = build(bad, "x86_64", &files(BINARY, NOTICE)).expect_err("a bad version");
        assert!(
            matches!(&error, Error::Failed(why) if why.starts_with("build the package: ")),
            "{bad:?}: {error:?}"
        );
    }
}

#[test]
fn only_the_linux_platforms_have_an_architecture_and_a_file_name() {
    assert_eq!(architecture(Platform::LinuxX86_64), Some("x86_64"));
    assert_eq!(architecture(Platform::LinuxArm64), Some("aarch64"));
    assert_eq!(architecture(Platform::MacosArm64), None);
    assert_eq!(
        file_name("0.4.0", "aarch64"),
        "tierstore-0.4.0-1.aarch64.rpm"
    );
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
    let printed = rpm(&binary, "0.4.0", Platform::LinuxArm64, &notice, &out).expect("rpm");
    let bytes = std::fs::read(out.join("tierstore-0.4.0-1.aarch64.rpm")).expect("the package");
    assert_eq!(
        bytes,
        build("0.4.0", "aarch64", &files(BINARY, NOTICE)).expect("the package")
    );
    let digest = Sha256::of(&bytes);
    assert_eq!(
        std::fs::read_to_string(out.join("SHA256SUMS.linux-arm64")).expect("sums"),
        format!(
            "{}  earlier\n{digest}  tierstore-0.4.0-1.aarch64.rpm\n",
            "0".repeat(64)
        )
    );
    assert_eq!(
        printed,
        [format!(
            "tierstore-0.4.0-1.aarch64.rpm: sha256 {digest}, {} bytes",
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
    rpm(&binary, "0.4.0", Platform::LinuxX86_64, &notice, &out).expect("rpm");
    assert!(out.join("tierstore-0.4.0-1.x86_64.rpm").is_file());
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
        rpm(binary, version, platform, notice, &out)
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
            "--platform macos-arm64: only the Linux platforms have an RPM package".to_owned()
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
        assert!(matches!(
            run("0.4.0", Platform::LinuxArm64, b, n),
            Err(Error::Spawn { .. })
        ));
    }
    assert!(!out.exists() || std::fs::read_dir(&out).expect("out").next().is_none());

    run("0.4.0", Platform::LinuxArm64, &binary, &notice).expect("the first run");
    let before = std::fs::read(out.join("SHA256SUMS.linux-arm64")).expect("sums");
    assert!(run("0.4.0", Platform::LinuxArm64, &binary, &notice).is_err());
    assert_eq!(
        std::fs::read(out.join("SHA256SUMS.linux-arm64")).expect("sums"),
        before
    );
    std::fs::remove_file(out.join("tierstore-0.4.0-1.aarch64.rpm")).expect("rm");
    assert_eq!(
        run("0.4.0", Platform::LinuxArm64, &binary, &notice),
        Err(Error::Failed(format!(
            "{} already lists tierstore-0.4.0-1.aarch64.rpm",
            out.join("SHA256SUMS.linux-arm64").display()
        )))
    );
}

#[test]
fn rpm_info_lists_the_header_and_the_files_as_rpm_does() {
    // Catches: a read-back step that prints nothing a reviewer or the build log can compare
    // with `rpm -qip` and `rpm -qlv`, and one that passes a package whose digests fail.
    let dir = scratch("info");
    let path = put(&dir, "t.rpm", &package());
    let lines = rpm_info(&path).expect("info");
    assert_eq!(
        lines[..6],
        [
            "Name: tierstore",
            "Version: 0.4.0",
            "Release: 1",
            "Architecture: x86_64",
            "Compression: gzip",
            "Requires: /bin/sh, glibc >= 2.28, systemd",
        ]
    );
    assert!(
        lines.contains(&"-0644 root:root /usr/share/doc/tierstore/examples/config.toml".to_owned())
    );
    assert!(lines.contains(&"d0755 root:root /usr/share/doc/tierstore/examples".to_owned()));
    assert!(
        lines
            .contains(&"-0644 root:root /usr/share/licenses/tierstore/LICENSE %license".to_owned())
    );
    assert!(lines.iter().all(|l| !l.contains("/etc/")), "{lines:?}");
    assert_eq!(lines.len(), 6 + 10);
    let mut bad = package();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    let bad = put(&dir, "bad.rpm", &bad);
    assert!(rpm_info(&bad).is_err());
    assert!(matches!(
        rpm_info(&dir.join("missing.rpm")),
        Err(Error::Spawn { .. })
    ));
}

#[test]
fn the_package_requires_the_glibc_the_binary_is_linked_against() {
    // Catches: a package that installs on a host whose glibc is older than the one the binary
    // needs (the service then fails to start with "GLIBC_2.xx not found" instead of `dnf`
    // refusing the install): the header carries `glibc >= 2.28` as a greater-or-equal
    // requirement of that version, not as a bare name.
    assert_eq!(glibc::FLOOR, "2.28");
    let requires = parsed(&package())
        .metadata
        .get_requires()
        .expect("requires");
    let glibc = requires.iter().find(|d| d.name == "glibc").expect("glibc");
    assert_eq!(glibc.version, "2.28");
    assert_eq!(glibc.flags, DependencyFlags::GE);
}

#[test]
fn a_requirement_is_written_as_rpm_writes_it() {
    // Catches: the read-back dropping the version of a requirement, so a package that lost
    // its `>=` would read back the same as one that has it.
    for (dependency, text) in [
        (Dependency::any("systemd"), "systemd"),
        (Dependency::greater_eq("glibc", "2.28"), "glibc >= 2.28"),
        (
            Dependency::eq("config(tierstore)", "0.4.0-1"),
            "config(tierstore) = 0.4.0-1",
        ),
        (Dependency::less("libfoo", "3"), "libfoo < 3"),
    ] {
        assert_eq!(requirement(dependency), text);
    }
}

#[test]
fn rpm_info_marks_a_configuration_file_as_rpm_does() {
    // Catches: a read-back that would not show a %config(noreplace) file, so a package that
    // gained one (a file in /etc an erase would remove) would read back like this one.
    let mut builder = PackageBuilder::new("t", "1.0.0", "Apache-2.0", "x86_64", "t");
    builder
        .using_config(BuildConfig::v4().source_date(deb::MTIME))
        .build_host(BUILD_HOST)
        .packager(deb::MAINTAINER)
        .post_install_script(Scriptlet::new(POST).prog(vec!["/bin/sh"]))
        .post_uninstall_script(Scriptlet::new(POSTUN).prog(vec!["/bin/sh"]))
        .with_file_contents(
            b"x".to_vec(),
            FileOptions::new("/etc/t/t.toml")
                .mode(FileMode::regular(0o640))
                .config()
                .noreplace(),
        )
        .expect("a file");
    let mut bytes = Vec::new();
    builder
        .build()
        .expect("a package")
        .write(&mut bytes)
        .expect("written");
    let dir = scratch("info-config");
    let lines = rpm_info(&put(&dir, "t.rpm", &bytes)).expect("info");
    assert!(
        lines.contains(&"-0640 root:root /etc/t/t.toml %config(noreplace)".to_owned()),
        "{lines:?}"
    );
}

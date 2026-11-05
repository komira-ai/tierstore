use std::path::PathBuf;

use super::*;
use crate::testdir::TestDir;

/// A version as `tierstore --version` prints it after the program's name.
const VERSION: &str = "0.4.0+0123456789ab";

/// A stand-in binary that holds [`VERSION`] among other bytes, as a build holds its constant.
fn binary() -> Vec<u8> {
    let mut bytes = b"\x7fELF stand-in: tierstore ".to_vec();
    bytes.extend_from_slice(VERSION.as_bytes());
    bytes.extend_from_slice(b" and more read-only data\0");
    bytes
}

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-hostkeeper-package-unit"), name)
}

/// `dir/tierstore` holding `bytes`.
fn put_binary(dir: &Path, bytes: &[u8]) -> PathBuf {
    let path = dir.join("tierstore");
    std::fs::write(&path, bytes).expect("write the binary");
    path
}

#[test]
fn a_release_version_is_three_numbers_a_plus_and_twelve_lowercase_hex_digits() {
    // Catches: a version hostkeeper would install that is not the text the build reports (no
    // commit, `unknown`, a short or upper-case commit, a leading zero, a prefix or a fourth
    // number), each of which the update hook's gauge comparison would never match.
    for good in [VERSION, "0.0.0+000000000000", "10.20.30+abcdefabcdef"] {
        assert!(is_release_version(good), "{good}");
    }
    for bad in [
        "0.4.0",
        "0.4.0+",
        "0.4.0+unknown",
        "0.4.0+0123456789a",
        "0.4.0+0123456789abc",
        "0.4.0+0123456789AB",
        "0.4.0+0123456789ag",
        "00.4.0+0123456789ab",
        "0.04.0+0123456789ab",
        "0.4+0123456789ab",
        "0.4.0.1+0123456789ab",
        "v0.4.0+0123456789ab",
        "0..0+0123456789ab",
        "+0123456789ab",
        "",
    ] {
        assert!(!is_release_version(bad), "{bad:?}");
    }
}

#[test]
fn a_plain_version_has_no_suffix_and_no_leading_zero() {
    // Catches: a `min_hostkeeper` the loader refuses (a build suffix, two or four numbers, a
    // leading zero), written into every package.
    for good in ["0.1.0", "1.0.0", "10.0.12"] {
        assert!(is_plain_version(good), "{good}");
    }
    for bad in [
        "0.1.0+abc",
        "0.1",
        "0.1.0.0",
        "01.0.0",
        "0.1.-1",
        "0.1.x",
        "",
        "..",
        "0.1.0-rc1",
    ] {
        assert!(!is_plain_version(bad), "{bad:?}");
    }
}

#[test]
fn every_placeholder_of_the_template_is_there_once_and_none_is_left_after_filling_it() {
    // Catches: a placeholder renamed or dropped in the template (the value would not be
    // written), or one written twice.
    for placeholder in PLACEHOLDERS {
        assert_eq!(
            MANIFEST_TEMPLATE.matches(placeholder).count(),
            1,
            "{placeholder}"
        );
    }
    for platform in Platform::ALL {
        let text = manifest(platform, VERSION, "0.1.0");
        for placeholder in PLACEHOLDERS {
            assert!(!text.contains(placeholder), "{placeholder} in {text}");
        }
    }
}

#[test]
fn the_manifest_names_the_version_the_platform_and_the_platforms_units_and_params() {
    // Catches: a value written into the wrong key, or the Linux values in the macOS manifest.
    let linux = manifest(Platform::LinuxArm64, VERSION, "0.2.0");
    for line in [
        "version        = \"0.4.0+0123456789ab\"",
        "platform       = \"linux-arm64\"",
        "min_hostkeeper = \"0.2.0\"",
        "run = [\"tierstore@a.service\", \"tierstore@b.service\"]",
        "config_file = \"/etc/tierstore/config.toml\"\nservice_manager = \"systemd\"\n",
    ] {
        assert!(linux.contains(line), "{line:?} not in {linux}");
    }
    // The template's comments name both systems; the values do not.
    for other in [
        "service_manager = \"launchd\"",
        "launchd_label = ",
        "run = [\"tierstore\"]",
    ] {
        assert!(!linux.contains(other), "{other} in {linux}");
    }
    let mac = manifest(Platform::MacosArm64, VERSION, "0.2.0");
    for line in [
        "platform       = \"macos-arm64\"",
        "run = [\"tierstore\"]",
        "config_file = \"/usr/local/tierstore/etc/tierstore.toml\"\nservice_manager = \"launchd\"\nlaunchd_label = \"tierstore\"\n",
    ] {
        assert!(mac.contains(line), "{line:?} not in {mac}");
    }
    for other in [
        "service_manager = \"systemd\"",
        "tierstore@a.service",
        "/etc/tierstore/",
    ] {
        assert!(!mac.contains(other), "{other} in {mac}");
    }
}

#[test]
fn the_entries_are_sorted_with_each_directory_before_its_files() {
    // Catches: an order that depends on how the list is built, a directory after its contents,
    // a mode other than 0755 for the binary and directories and 0644 for the rest, or the
    // other system's unit file.
    let names = |platform| -> Vec<(String, u32, bool)> {
        entries(platform, VERSION, "0.1.0", b"bytes")
            .into_iter()
            .map(|(name, mode, content)| (name, mode, content.is_some()))
            .collect()
    };
    let expected = |unit: &str| -> Vec<(String, u32, bool)> {
        vec![
            ("bin".to_owned(), 0o755, false),
            ("bin/tierstore".to_owned(), 0o755, true),
            ("manifest.toml".to_owned(), 0o644, true),
            ("units".to_owned(), 0o755, false),
            (format!("units/{unit}"), 0o644, true),
        ]
    };
    assert_eq!(names(Platform::LinuxX86_64), expected("tierstore@.service"));
    assert_eq!(names(Platform::LinuxArm64), expected("tierstore@.service"));
    assert_eq!(names(Platform::MacosArm64), expected("tierstore.plist"));
}

#[test]
fn a_name_too_long_for_ustar_is_an_error_naming_it() {
    // Catches: an entry that cannot be written being dropped or truncated instead of refused.
    let name = "x".repeat(300);
    let err = tar_to(Vec::new(), &[(name.clone(), FILE, Some(Vec::new()))]).unwrap_err();
    assert!(
        matches!(&err, Error::Spawn { what, .. } if what == &format!("the tar entry {name:?}")),
        "{err:?}"
    );
}

/// A sink that takes `room` bytes and then fails.
struct Full {
    room: usize,
}

impl Write for Full {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.room == 0 {
            return Err(std::io::Error::other("the sink is full"));
        }
        let n = buf.len().min(self.room);
        self.room -= n;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_sink_that_fails_is_an_error_naming_the_entry_or_the_end() {
    // Catches: a write error swallowed, which would leave a truncated archive.
    let entries = entries(Platform::LinuxX86_64, VERSION, "0.1.0", b"bytes");
    let err = tar_to(Full { room: 0 }, &entries).map(|_| ()).unwrap_err();
    assert!(
        matches!(&err, Error::Spawn { what, why } if what == "write the tar entry \"bin\"" && why == "the sink is full"),
        "{err:?}"
    );
    let mut all = Vec::new();
    tar_to(&mut all, &entries).expect("the archive");
    // Room for every entry and none for the two zero blocks that end the archive.
    let err = tar_to(
        Full {
            room: all.len() - 1024,
        },
        &entries,
    )
    .map(|_| ())
    .unwrap_err();
    assert!(
        matches!(&err, Error::Spawn { what, .. } if what == "finish the tar archive"),
        "{err:?}"
    );
}

#[test]
fn the_verb_writes_the_archive_and_prints_its_name_digest_and_size() {
    // Catches: a file name other than tierstore-<version>-<platform>.tar.zst, an out directory
    // that is not made, or a printed digest that is not of the bytes written.
    let dir = scratch("writes");
    let bin = put_binary(&dir, &binary());
    let out = dir.join("out").join("deeper");
    let printed =
        hostkeeper_package(&bin, VERSION, "0.1.0", Platform::MacosArm64, &out).expect("packaged");
    let name = "tierstore-0.4.0+0123456789ab-macos-arm64.tar.zst";
    let written = std::fs::read(out.join(name)).expect("the package");
    assert_eq!(
        written,
        archive(Platform::MacosArm64, VERSION, "0.1.0", &binary()).expect("archive")
    );
    assert_eq!(
        printed,
        [format!(
            "{name}: sha256 {}, {} bytes",
            hex::encode(Sha256::digest(&written)),
            written.len()
        )]
    );
    assert_eq!(
        std::fs::read_to_string(out.join("SHA256SUMS.macos-arm64")).expect("the sums file"),
        format!("{}  {name}\n", hex::encode(Sha256::digest(&written)))
    );
}

/// The text of `out/SHA256SUMS.<platform>`.
fn sums_text(out: &Path, platform: Platform) -> String {
    std::fs::read_to_string(out.join(format!("SHA256SUMS.{}", platform.name())))
        .expect("the sums file")
}

#[test]
fn a_second_platforms_package_leaves_the_first_platforms_sums_file_alone() {
    // Catches: one sums file for every platform, or a sums file of one platform rewritten while
    // another's package is added.
    let dir = scratch("two-platforms");
    let bin = put_binary(&dir, &binary());
    let out = dir.join("out");
    hostkeeper_package(&bin, VERSION, "0.1.0", Platform::LinuxX86_64, &out).expect("first");
    let first = sums_text(&out, Platform::LinuxX86_64);
    hostkeeper_package(&bin, VERSION, "0.1.0", Platform::LinuxArm64, &out).expect("second");
    assert_eq!(sums_text(&out, Platform::LinuxX86_64), first);
    let second = sums_text(&out, Platform::LinuxArm64);
    assert_eq!(second.lines().count(), 1);
    assert!(
        second.ends_with(&format!("  {}\n", file_name(VERSION, Platform::LinuxArm64))),
        "{second}"
    );
}

#[test]
fn the_package_line_is_appended_after_the_lines_package_wrote() {
    // Catches: a sums file rewritten from the package's line alone, which would drop the
    // tarball's and the SBOM's lines, or a package line written before them.
    let dir = scratch("after-package");
    let (binary_path, sbom) = (dir.join("tierstore"), dir.join("sbom.cdx.json"));
    std::fs::write(&binary_path, binary()).expect("binary");
    std::fs::write(&sbom, b"{\"bomFormat\":\"CycloneDX\"}").expect("sbom");
    let out = dir.join("out");
    let commit = "0123456789abcdef0123456789abcdef01234567";
    crate::package::package(&binary_path, &sbom, Platform::LinuxX86_64, commit, &out)
        .expect("package");
    let earlier = sums_text(&out, Platform::LinuxX86_64);
    assert_eq!(earlier.lines().count(), 2);
    hostkeeper_package(&binary_path, VERSION, "0.1.0", Platform::LinuxX86_64, &out)
        .expect("hostkeeper package");
    let after = sums_text(&out, Platform::LinuxX86_64);
    assert!(after.starts_with(&earlier), "{after}");
    let rest = &after[earlier.len()..];
    let package = std::fs::read(out.join(file_name(VERSION, Platform::LinuxX86_64))).expect("pkg");
    assert_eq!(
        rest,
        format!(
            "{}  {}\n",
            hex::encode(Sha256::digest(&package)),
            file_name(VERSION, Platform::LinuxX86_64)
        )
    );
}

#[test]
fn a_malformed_sums_file_is_refused_and_nothing_is_written() {
    // Catches: a package written beside a sums file that cannot be read back, or a damaged
    // sums file rewritten from scratch.
    let dir = scratch("bad-sums");
    let bin = put_binary(&dir, &binary());
    let out = dir.join("out");
    std::fs::create_dir_all(&out).expect("out");
    let sums_path = out.join("SHA256SUMS.linux-x86_64");
    std::fs::write(&sums_path, "not a sums line\n").expect("sums");
    let err = hostkeeper_package(&bin, VERSION, "0.1.0", Platform::LinuxX86_64, &out).unwrap_err();
    assert_eq!(
        err,
        Error::Refused(vec![
            "line 1: \"not a sums line\" is not `<sha256>  <file name>` and a newline".to_owned()
        ])
    );
    assert_eq!(
        std::fs::read_to_string(&sums_path).expect("sums"),
        "not a sums line\n"
    );
    assert!(!out.join(file_name(VERSION, Platform::LinuxX86_64)).exists());
}

#[test]
fn the_verb_refuses_a_malformed_version_or_min_hostkeeper_before_reading_anything() {
    // Catches: a manifest written with a version the build does not report, or with a floor the
    // loader refuses.
    let dir = scratch("malformed");
    let missing = dir.join("no-such-binary");
    let err =
        hostkeeper_package(&missing, "0.4.0", "0.1.0", Platform::LinuxX86_64, &dir).unwrap_err();
    assert_eq!(
        err,
        Error::Usage(
            "--version \"0.4.0\" is not <major>.<minor>.<patch>+<12 lowercase hex digits>, what `tierstore --version` prints".to_owned()
        )
    );
    let err = hostkeeper_package(&missing, VERSION, "0.1.0+abc", Platform::LinuxX86_64, &dir)
        .unwrap_err();
    assert_eq!(
        err,
        Error::Usage("--min-hostkeeper \"0.1.0+abc\" is not <major>.<minor>.<patch>".to_owned())
    );
    let err =
        hostkeeper_package(&missing, VERSION, "0.1.0", Platform::LinuxX86_64, &dir).unwrap_err();
    assert!(
        matches!(&err, Error::Spawn { what, .. } if what == &format!("read {}", missing.display())),
        "{err:?}"
    );
}

#[test]
fn the_verb_refuses_a_binary_that_does_not_hold_the_version() {
    // Catches: a package whose manifest names a version the binary does not report, so the
    // update hook's check of the build gauge fails on every host after the switch.
    let dir = scratch("other-version");
    let bin = put_binary(&dir, &binary());
    for other in ["0.4.0+0123456789ac", "0.4.1+0123456789ab"] {
        let err =
            hostkeeper_package(&bin, other, "0.1.0", Platform::LinuxX86_64, &dir).unwrap_err();
        assert_eq!(
            err,
            Error::Failed(format!(
                "{} does not hold the version {other:?}, so it does not report it",
                bin.display()
            ))
        );
    }
    let short = put_binary(&dir, b"0.4.0");
    let err =
        hostkeeper_package(&short, VERSION, "0.1.0", Platform::LinuxX86_64, &dir).unwrap_err();
    assert!(matches!(err, Error::Failed(_)), "{err:?}");
    assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 1);
    // Neither the archive nor the sums file is written into an out of its own.
    let out = dir.join("out");
    let err = hostkeeper_package(
        &bin,
        "0.4.1+0123456789ab",
        "0.1.0",
        Platform::LinuxX86_64,
        &out,
    )
    .unwrap_err();
    assert!(matches!(err, Error::Failed(_)), "{err:?}");
    assert!(!out.exists());
}

#[test]
fn the_verb_refuses_an_out_that_cannot_be_made_or_a_package_already_there() {
    // Catches: a package overwritten in place, or a failure to create the directory ignored.
    let dir = scratch("exists");
    let bin = put_binary(&dir, &binary());
    let blocker = dir.join("a-file");
    std::fs::write(&blocker, b"not a directory").expect("write");
    let err =
        hostkeeper_package(&bin, VERSION, "0.1.0", Platform::LinuxX86_64, &blocker).unwrap_err();
    assert!(
        matches!(&err, Error::Spawn { what, .. } if what == &format!("create {}", blocker.display())),
        "{err:?}"
    );
    let out = dir.join("out");
    hostkeeper_package(&bin, VERSION, "0.1.0", Platform::LinuxX86_64, &out).expect("first");
    let path = out.join(file_name(VERSION, Platform::LinuxX86_64));
    let before = std::fs::read(&path).expect("package");
    let sums_before = sums_text(&out, Platform::LinuxX86_64);
    let err = hostkeeper_package(&bin, VERSION, "0.2.0", Platform::LinuxX86_64, &out).unwrap_err();
    assert_eq!(
        err,
        Error::Failed(format!(
            "{} already lists {}",
            out.join("SHA256SUMS.linux-x86_64").display(),
            file_name(VERSION, Platform::LinuxX86_64)
        ))
    );
    assert_eq!(std::fs::read(&path).expect("package"), before);
    assert_eq!(sums_text(&out, Platform::LinuxX86_64), sums_before);
    // An archive the sums file does not list is still not overwritten.
    std::fs::remove_file(out.join("SHA256SUMS.linux-x86_64")).expect("remove the sums");
    let err = hostkeeper_package(&bin, VERSION, "0.2.0", Platform::LinuxX86_64, &out).unwrap_err();
    assert!(
        matches!(&err, Error::Spawn { what, .. } if what == &format!("write {}", path.display())),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&path).expect("package"), before);
    assert!(!out.join("SHA256SUMS.linux-x86_64").exists());
}

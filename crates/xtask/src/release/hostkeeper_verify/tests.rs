use std::ops::Deref;
use std::os::unix::fs::PermissionsExt;

use hostkeeper_config::daemon::Sha256Hex;
use hostkeeper_config::index::PackageEntry;
use hostkeeper_config::names::{ServiceName, Version};
use hostkeeper_config::platform::Platform as HostPlatform;
use sha2::{Digest, Sha256};

use super::*;
use crate::release::hostkeeper_index as hk_index;
use crate::release::hostkeeper_package::{entries, tar_to};
use crate::testdir::TestDir;

/// The commit every package of these tests is built from.
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The version of the packages of [`COMMIT`].
const VERSION: &str = "0.4.0+0123456789ab";

/// The oldest hostkeeper the packages need.
const MIN_HOSTKEEPER: &str = "0.1.0";

/// A stand-in binary that holds [`VERSION`] and the magic of a macOS code signature, so a test
/// sees the bytes go through the unpack untouched.
fn binary() -> Vec<u8> {
    let mut bytes = b"\xcf\xfa\xed\xfe stand-in tierstore\0".to_vec();
    bytes.extend_from_slice(VERSION.as_bytes());
    bytes.extend_from_slice(b"\0read-only data\0\xfa\xde\x0c\xc0 signature blob\0");
    bytes
}

/// Gives `path` and every directory below it mode 0700: hostkeeper's unpacker leaves an unpacked
/// version read-only (0555), which `remove_dir_all` cannot remove.
fn open_up(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            open_up(&entry.path());
        }
    }
}

/// An empty directory of its own for one test, opened up and removed when it drops.
struct Scratch(TestDir);

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        open_up(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let beside = std::env::current_exe().expect("test binary");
    let base = beside
        .parent()
        .expect("deps")
        .join("xtask-hostkeeper-verify-unit");
    // A run that was killed left read-only directories that the scratch cannot replace.
    open_up(&base.join(name));
    Scratch(TestDir::under(base, name))
}

/// A directory `dir` of the scratch for the packages.
fn packages_dir(scratch: &Path) -> PathBuf {
    let dir = scratch.join("dir");
    std::fs::create_dir(&dir).expect("make the packages directory");
    dir
}

fn zstd(tar: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(tar, ruzstd::encoding::CompressionLevel::Fastest)
}

/// The good package of `platform` as `version`.
fn package(platform: Platform, version: &str) -> Vec<u8> {
    hostkeeper_package_archive(platform, version)
}

fn hostkeeper_package_archive(platform: Platform, version: &str) -> Vec<u8> {
    crate::release::hostkeeper_package::archive(platform, version, MIN_HOSTKEEPER, &binary())
        .expect("the package")
}

/// Writes the package `bytes` of `platform` as `version` into `dir`.
fn put(dir: &Path, version: &str, platform: Platform, bytes: &[u8]) -> PathBuf {
    let path = dir.join(file_name(version, platform));
    std::fs::write(&path, bytes).expect("write the package");
    path
}

/// Writes the good package of each platform into `dir`.
fn put_release(dir: &Path) {
    for platform in Platform::ALL {
        put(dir, VERSION, platform, &package(platform, VERSION));
    }
}

/// The archive of `platform` with the extra entry `bin/._tierstore`.
fn apple_double_package(platform: Platform) -> Vec<u8> {
    let mut list = entries(platform, VERSION, MIN_HOSTKEEPER, &binary());
    list.push(("bin/._tierstore".to_owned(), 0o644, Some(b"x".to_vec())));
    zstd(&tar_to(Vec::new(), &list).expect("the tar"))
}

/// The pax record `<len> <key>=<value>\n`, where `<len>` counts the whole record.
fn pax_record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    while body.len() + len.to_string().len() != len {
        len += 1;
    }
    format!("{len}{body}")
}

/// A package whose first entry carries a pax extended header of `records`.
fn pax_package(platform: Platform, records: &[(&str, &str)]) -> Vec<u8> {
    let data: Vec<u8> = records
        .iter()
        .flat_map(|(key, value)| pax_record(key, value).into_bytes())
        .collect();
    let mut header = tar::Header::new_ustar();
    header
        .set_path("PaxHeaders/manifest.toml")
        .expect("the pax header's name");
    header.set_entry_type(tar::EntryType::XHeader);
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    let mut tar = header.as_bytes().to_vec();
    tar.extend_from_slice(&data);
    tar.resize(512 + data.len().div_ceil(512) * 512, 0);
    let list = entries(platform, VERSION, MIN_HOSTKEEPER, &binary());
    tar.extend_from_slice(&tar_to(Vec::new(), &list).expect("the tar"));
    zstd(&tar)
}

fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The line `verify_package` prints for the package of `platform` as `version` in `dir`, unpacked
/// into `unpacked`.
fn printed(dir: &Path, version: &str, platform: Platform, unpacked: &Path) -> String {
    let name = file_name(version, platform);
    let bytes = std::fs::read(dir.join(&name)).expect("read the package");
    format!(
        "{name}: {} bytes, sha256 {}, no extended attribute; unpacked as a host does into {}",
        bytes.len(),
        sha_hex(&bytes),
        unpacked.display()
    )
}

/// Writes `dir/index.json` listing `platforms`, each entry as `tweak` leaves it.
fn put_index(dir: &Path, platforms: &[Platform], tweak: impl Fn(&mut PackageEntry)) {
    put_index_of(dir, "tierstore", VERSION, platforms, tweak);
}

fn put_index_of(
    dir: &Path,
    service: &str,
    version: &str,
    platforms: &[Platform],
    tweak: impl Fn(&mut PackageEntry),
) {
    let mut list = Vec::new();
    for platform in platforms {
        let (sha256, bytes) =
            digest_file(&dir.join(file_name(VERSION, *platform))).expect("digest");
        let mut entry = PackageEntry {
            platform: host_platform(*platform),
            sha256,
            bytes,
        };
        tweak(&mut entry);
        list.push(entry);
    }
    let index = ReleaseIndex::new(
        ServiceName::new(service).expect("a service name"),
        Version::new(version).expect("a version"),
        list,
    )
    .expect("the index");
    std::fs::write(dir.join(INDEX), index.to_json()).expect("write the index");
}

#[test]
fn verify_package_unpacks_the_package_of_each_platform_as_a_host_does() {
    // Catches: a package the check passes that the host's unpacker would not unpack, or a
    // binary that changes on the way (the signature bytes of the macOS binary included).
    for platform in Platform::ALL {
        let s = scratch(&format!("unpack-{}", platform.name()));
        let dir = packages_dir(&s);
        put(&dir, VERSION, platform, &package(platform, VERSION));
        let unpack_into = s.join("unpack");
        let lines = verify_package(&dir, VERSION, platform, &unpack_into).expect("verified");
        let unpacked = unpack_into.join("packages/tierstore").join(VERSION);
        assert_eq!(lines, vec![printed(&dir, VERSION, platform, &unpacked)]);
        assert_eq!(
            std::fs::read(unpacked.join("bin/tierstore")).expect("the binary"),
            binary()
        );
        assert!(unpacked.join("manifest.toml").is_file());
    }
}

#[test]
fn verify_package_refuses_a_version_that_is_not_a_release_version_before_reading() {
    // Catches: a --version of a malformed text read as a file name, or the package read before
    // the version is judged.
    let s = scratch("bad-version");
    let err = verify_package(
        &s.join("absent"),
        "0.4.0",
        Platform::LinuxArm64,
        &s.join("u"),
    )
    .expect_err("refused");
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "--version \"0.4.0\" is not <major>.<minor>.<patch>+<12 lowercase hex digits>"
    );
}

#[test]
fn verify_package_reports_a_missing_package_as_a_read_error() {
    // Catches: a missing package passed as an empty or skipped one.
    let s = scratch("missing-package");
    let dir = packages_dir(&s);
    let err =
        verify_package(&dir, VERSION, Platform::MacosArm64, &s.join("u")).expect_err("refused");
    let path = dir.join(file_name(VERSION, Platform::MacosArm64));
    match err {
        Error::Spawn { what, .. } => assert_eq!(what, format!("read {}", path.display())),
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_package_refuses_a_package_over_the_byte_cap_and_passes_one_exactly_at_it() {
    // Catches: a cap that is off by one, or not applied, so a host would refuse a package the
    // release published.
    let s = scratch("byte-cap");
    let dir = packages_dir(&s);
    let bytes = package(Platform::LinuxX86_64, VERSION);
    put(&dir, VERSION, Platform::LinuxX86_64, &bytes);
    let size = bytes.len() as u64;
    let err = verify_package_within(
        &dir,
        VERSION,
        Platform::LinuxX86_64,
        &s.join("over"),
        size - 1,
    )
    .expect_err("over the cap");
    assert!(matches!(err, Error::Failed(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        format!(
            "{}: {size} bytes, over the {} a host fetches",
            file_name(VERSION, Platform::LinuxX86_64),
            size - 1
        )
    );
    verify_package_within(&dir, VERSION, Platform::LinuxX86_64, &s.join("at"), size)
        .expect("at the cap");
}

#[test]
fn verify_package_refuses_an_appledouble_entry() {
    // Catches: a package built on a Mac with a `._` file beside the binary, which carries an
    // extended attribute the unpacker would otherwise unpack as a file.
    let s = scratch("appledouble");
    let dir = packages_dir(&s);
    put(
        &dir,
        VERSION,
        Platform::MacosArm64,
        &apple_double_package(Platform::MacosArm64),
    );
    let err =
        verify_package(&dir, VERSION, Platform::MacosArm64, &s.join("u")).expect_err("refused");
    match err {
        Error::Refused(lines) => assert_eq!(
            lines,
            vec![format!(
                "{}: bin/._tierstore: an AppleDouble file, the extended attributes of another entry",
                file_name(VERSION, Platform::MacosArm64)
            )]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_package_refuses_a_pax_record_of_an_extended_attribute() {
    // Catches: a quarantine attribute written into the package as a pax record, which the
    // unpacker ignores and a host would then carry on its files.
    let s = scratch("pax-xattr");
    let dir = packages_dir(&s);
    let records = [("SCHILY.xattr.com.apple.quarantine", "0081;00000000;x;")];
    put(
        &dir,
        VERSION,
        Platform::MacosArm64,
        &pax_package(Platform::MacosArm64, &records),
    );
    let err =
        verify_package(&dir, VERSION, Platform::MacosArm64, &s.join("u")).expect_err("refused");
    match err {
        Error::Refused(lines) => assert_eq!(
            lines,
            vec![format!(
                "{}: bin: the pax record SCHILY.xattr.com.apple.quarantine, an extended attribute or ACL",
                file_name(VERSION, Platform::MacosArm64)
            )]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn attribute_problems_ignores_a_pax_record_that_is_not_an_attribute() {
    // Catches: every pax header refused, including the harmless mtime record some tar writers
    // add.
    let s = scratch("pax-mtime");
    let path = s.join("p.tar.zst");
    std::fs::write(
        &path,
        pax_package(Platform::LinuxArm64, &[("mtime", "1.5")]),
    )
    .expect("write");
    assert_eq!(
        attribute_problems(&path).expect("read"),
        Vec::<String>::new()
    );
}

#[test]
fn attribute_problems_reports_each_attribute_record_prefix() {
    // Catches: a prefix of ATTRIBUTE_RECORDS that is listed but not matched.
    let s = scratch("pax-prefixes");
    let path = s.join("p.tar.zst");
    for prefix in ATTRIBUTE_RECORDS {
        let key = format!("{prefix}name");
        std::fs::write(
            &path,
            pax_package(Platform::LinuxArm64, &[(key.as_str(), "v")]),
        )
        .expect("write");
        assert_eq!(
            attribute_problems(&path).expect("read"),
            vec![format!(
                "bin: the pax record {key}, an extended attribute or ACL"
            )],
            "{prefix}"
        );
    }
}

#[test]
fn attribute_problems_reports_a_file_that_is_not_a_zstd_frame() {
    // Catches: a plain file read as an empty tar and passed.
    let s = scratch("not-zstd");
    let path = s.join("p.tar.zst");
    std::fs::write(&path, b"plain text, not a frame").expect("write");
    let problems = attribute_problems(&path).expect("read");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("not a zstd frame: "),
        "{problems:?}"
    );
}

#[test]
fn attribute_problems_reports_a_zstd_frame_that_is_not_a_tar() {
    // Catches: a frame of arbitrary bytes read as an empty tar and passed.
    let s = scratch("not-tar");
    let path = s.join("p.tar.zst");
    std::fs::write(&path, zstd(&[b'x'; 2048])).expect("write");
    let problems = attribute_problems(&path).expect("read");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("not a tar: "), "{problems:?}");
}

#[test]
fn attribute_problems_reports_a_missing_file_as_a_read_error() {
    // Catches: a file that cannot be opened reported as a package without attributes.
    let s = scratch("no-file");
    let err = attribute_problems(&s.join("absent")).expect_err("missing");
    assert!(matches!(err, Error::Spawn { .. }), "{err:?}");
}

#[test]
fn verify_package_reports_a_package_hostkeepers_unpacker_refuses_with_its_reason() {
    // Catches: a package of another version than its name says, passed because only its
    // attributes were checked, which a host refuses with manifest_mismatch.
    let s = scratch("unpack-refused");
    let dir = packages_dir(&s);
    let named = "0.4.1+0123456789ab";
    let path = put(
        &dir,
        named,
        Platform::LinuxX86_64,
        &package(Platform::LinuxX86_64, VERSION),
    );
    let err =
        verify_package(&dir, named, Platform::LinuxX86_64, &s.join("u")).expect_err("refused");
    match err {
        Error::Failed(text) => {
            let start = format!("{}: hostkeeper does not unpack it: ", path.display());
            assert!(text.starts_with(&start), "{text}");
            assert!(text.ends_with("(manifest_mismatch)"), "{text}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_package_reports_a_scratch_that_is_a_file_as_a_create_error() {
    // Catches: a scratch directory that cannot be made reported as an unpack of the package.
    let s = scratch("scratch-file");
    let dir = packages_dir(&s);
    put(
        &dir,
        VERSION,
        Platform::LinuxArm64,
        &package(Platform::LinuxArm64, VERSION),
    );
    let file = s.join("a-file");
    std::fs::write(&file, b"x").expect("write");
    let err = verify_package(&dir, VERSION, Platform::LinuxArm64, &file).expect_err("refused");
    match err {
        Error::Spawn { what, .. } => {
            assert_eq!(what, format!("create {}", file.join("packages").display()));
        }
        other => panic!("{other:?}"),
    }
}

/// A directory of the three packages and the index `release hostkeeper-index` writes for them.
fn released(s: &Path) -> PathBuf {
    let dir = packages_dir(s);
    put_release(&dir);
    hk_index::hostkeeper_index(&dir, COMMIT, "push", None).expect("the index");
    dir
}

#[test]
fn verify_index_passes_an_index_of_three_platforms_and_prints_each_package() {
    // Catches: an index the check passes without verifying each package, or one line short.
    let s = scratch("index-good");
    let dir = released(&s);
    let lines = verify_index(&dir, COMMIT, &s.join("u")).expect("verified");
    let mut expected = vec![format!(
        "index.json: tierstore {VERSION}, a package for each of 3 platforms, each the size and SHA-256 of its file"
    )];
    for platform in Platform::ALL {
        let unpacked = s
            .join("u")
            .join(platform.name())
            .join("packages/tierstore")
            .join(VERSION);
        expected.push(printed(&dir, VERSION, platform, &unpacked));
    }
    assert_eq!(lines, expected);
}

#[test]
fn verify_index_refuses_an_index_that_lacks_a_platform() {
    // Catches: an index of two platforms passed, which leaves every Mac on its old version
    // (no_package_for_platform).
    let s = scratch("index-two");
    let dir = packages_dir(&s);
    let two = [Platform::LinuxArm64, Platform::LinuxX86_64];
    for platform in two {
        put(&dir, VERSION, platform, &package(platform, VERSION));
    }
    put_index(&dir, &two, |_| {});
    let err = verify_index(&dir, COMMIT, &s.join("u")).expect_err("refused");
    let mac = HostPlatform::MacosArm64;
    match err {
        Error::Refused(lines) => assert_eq!(
            lines,
            vec![format!(
                "index.json: the release index has no package for {mac} (no_package_for_platform): a host of macos-arm64 would be refused"
            )]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_index_refuses_an_index_that_is_not_canonical() {
    // Catches: an index that parses but differs from the bytes the writer would give, which two
    // runners would then hash differently.
    let s = scratch("index-pretty");
    let dir = released(&s);
    let text = std::fs::read_to_string(dir.join(INDEX)).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    std::fs::write(
        dir.join(INDEX),
        serde_json::to_string_pretty(&value).expect("pretty"),
    )
    .expect("write");
    let err = verify_index(&dir, COMMIT, &s.join("u")).expect_err("refused");
    match err {
        Error::Refused(lines) => assert_eq!(
            lines,
            vec![
                "index.json is not the canonical JSON of its index (keys sorted, no white space)"
                    .to_owned()
            ]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_index_refuses_an_index_that_lists_a_package_the_directory_lacks() {
    // Catches: an index that lists a file the release does not publish, which a host would
    // fail to fetch.
    let s = scratch("index-extra");
    let dir = released(&s);
    let gone = dir.join(file_name(VERSION, Platform::MacosArm64));
    std::fs::remove_file(&gone).expect("remove");
    let err = verify_index(&dir, COMMIT, &s.join("u")).expect_err("refused");
    match err {
        Error::Refused(lines) => assert_eq!(
            lines,
            vec![format!(
                "index.json lists {}, which is not in {}",
                file_name(VERSION, Platform::MacosArm64),
                dir.display()
            )]
        ),
        other => panic!("{other:?}"),
    }
}

/// The refusal for the digest of `platform`'s package, as `index.json` lists `sha` and `bytes`.
fn mismatch_line(dir: &Path, platform: Platform, sha: &str, bytes: u64) -> String {
    let (real, real_bytes) = digest_file(&dir.join(file_name(VERSION, platform))).expect("digest");
    format!(
        "{}: sha256 {} and {real_bytes} bytes, not the sha256 {sha} and {bytes} bytes index.json lists (package_digest_mismatch)",
        file_name(VERSION, platform),
        real.as_str()
    )
}

#[test]
fn verify_index_refuses_an_entry_whose_sha256_is_not_its_files() {
    // Catches: an index whose digest is not the file's, passed because the size matched.
    let s = scratch("index-sha");
    let dir = packages_dir(&s);
    put_release(&dir);
    let fake = "ab".repeat(32);
    put_index(&dir, &Platform::ALL, |entry| {
        if entry.platform == HostPlatform::LinuxX86_64 {
            entry.sha256 = Sha256Hex::parse(&fake).expect("hex");
        }
    });
    let bytes = digest_file(&dir.join(file_name(VERSION, Platform::LinuxX86_64)))
        .expect("digest")
        .1;
    let err = verify_index(&dir, COMMIT, &s.join("u")).expect_err("refused");
    match err {
        Error::Refused(lines) => assert_eq!(
            lines,
            vec![mismatch_line(&dir, Platform::LinuxX86_64, &fake, bytes)]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_index_refuses_an_entry_whose_size_is_not_its_files_though_the_sha256_is() {
    // Catches: an index whose size is not the file's, passed because the digest matched.
    let s = scratch("index-bytes");
    let dir = packages_dir(&s);
    put_release(&dir);
    put_index(&dir, &Platform::ALL, |entry| {
        if entry.platform == HostPlatform::LinuxArm64 {
            entry.bytes += 1;
        }
    });
    let (real, bytes) =
        digest_file(&dir.join(file_name(VERSION, Platform::LinuxArm64))).expect("digest");
    let err = verify_index(&dir, COMMIT, &s.join("u")).expect_err("refused");
    match err {
        Error::Refused(lines) => assert_eq!(
            lines,
            vec![mismatch_line(
                &dir,
                Platform::LinuxArm64,
                real.as_str(),
                bytes + 1
            )]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_index_reports_a_missing_index_as_a_read_error() {
    // Catches: a directory of packages without an index passed.
    let s = scratch("index-missing");
    let dir = packages_dir(&s);
    put_release(&dir);
    let err = verify_index(&dir, COMMIT, &s.join("u")).expect_err("refused");
    match err {
        Error::Spawn { what, .. } => {
            assert_eq!(what, format!("read {}", dir.join(INDEX).display()));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_index_refuses_an_index_of_another_service_or_version() {
    // Catches: an index of another service or version passed, which a host would refuse as
    // index_invalid.
    for (service, version) in [("other", VERSION), ("tierstore", "0.4.1+0123456789ab")] {
        let s = scratch(&format!("index-other-{service}-{}", &version[..5]));
        let dir = packages_dir(&s);
        put_release(&dir);
        put_index_of(&dir, service, version, &Platform::ALL, |_| {});
        let err = verify_index(&dir, COMMIT, &s.join("u")).expect_err("refused");
        match err {
            Error::Failed(text) => assert!(text.ends_with("(index_invalid)"), "{text}"),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn verify_index_refuses_a_commit_that_is_not_a_full_sha() {
    // Catches: a short commit taken as the version's build part.
    let s = scratch("index-commit");
    let err = verify_index(&s.join("absent"), "0123456789ab", &s.join("u")).expect_err("refused");
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "--commit \"0123456789ab\" is not a full commit SHA (40 lowercase hex digits)"
    );
}

#[test]
fn verify_index_propagates_the_refusal_of_a_package_that_carries_an_attribute() {
    // Catches: an index whose digests all match passed though one package fails the check of
    // the package (the index is rebuilt after the archive is swapped).
    let s = scratch("index-appledouble");
    let dir = packages_dir(&s);
    put_release(&dir);
    put(
        &dir,
        VERSION,
        Platform::LinuxX86_64,
        &apple_double_package(Platform::LinuxX86_64),
    );
    hk_index::hostkeeper_index(&dir, COMMIT, "push", None).expect("the index");
    let err = verify_index(&dir, COMMIT, &s.join("u")).expect_err("refused");
    match err {
        Error::Refused(lines) => assert_eq!(
            lines,
            vec![format!(
                "{}: bin/._tierstore: an AppleDouble file, the extended attributes of another entry",
                file_name(VERSION, Platform::LinuxX86_64)
            )]
        ),
        other => panic!("{other:?}"),
    }
}

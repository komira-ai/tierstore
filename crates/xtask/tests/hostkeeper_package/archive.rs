//! The archive of every platform: one zstd frame, the tar entries and their headers, and the same
//! bytes on every run.

use std::time::{Duration, UNIX_EPOCH};

use xtask::package::Platform;
use xtask::release::hostkeeper_package::{file_name, hostkeeper_package};

use crate::support::{
    FrameHeader, LINUX, MIN_HOSTKEEPER, Scratch, VERSION, binary, decode, file, frame_header,
    listing, package,
};

#[test]
fn the_archive_is_one_zstd_frame_with_a_checksum_no_dictionary_and_a_window_of_at_most_8_mib() {
    // Catches: a second frame or trailing bytes (hostkeeper refuses the package), a window above
    // hostkeeper's bound, a dictionary, or a frame with no content checksum.
    for platform in Platform::ALL {
        let archive = package(platform);
        let header = frame_header(&archive);
        assert!(
            header.window <= hostkeeper_unpack::MAX_WINDOW_BYTES,
            "{platform:?}: window {}",
            header.window
        );
        assert_eq!(
            header,
            FrameHeader {
                single_segment: false,
                reserved: false,
                checksum: true,
                dictionary: false,
                window: header.window,
            },
            "{platform:?}"
        );
        let (tar, after) = decode(&archive);
        assert_eq!(after, 0, "{platform:?}: bytes after the one frame");
        assert_eq!(tar.len() % 512, 0, "{platform:?}: whole tar blocks");
        assert!(
            tar.ends_with(&[0; 1024]),
            "{platform:?}: the end-of-archive marker"
        );
    }
}

#[test]
fn the_archive_holds_only_manifest_bin_and_units_each_directory_first_with_fixed_headers() {
    // Catches: another top-level name, an entry that is neither a file nor a directory, a
    // directory after its contents, an unsorted order, a mode other than 0755 (binary and
    // directories) or 0644 (other files), a setuid bit, an owner, owner name or time taken from
    // the build, or a header that is not ustar.
    for platform in Platform::ALL {
        let unit = match platform {
            Platform::MacosArm64 => "units/tierstore.plist",
            Platform::LinuxX86_64 | Platform::LinuxArm64 => "units/tierstore@.service",
        };
        let listed = listing(&package(platform));
        let shape: Vec<(&str, tar::EntryType, u32)> = listed
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.mode))
            .collect();
        assert_eq!(
            shape,
            [
                ("bin", tar::EntryType::Directory, 0o755),
                ("bin/tierstore", tar::EntryType::Regular, 0o755),
                ("manifest.toml", tar::EntryType::Regular, 0o644),
                ("units", tar::EntryType::Directory, 0o755),
                (unit, tar::EntryType::Regular, 0o644),
            ],
            "{platform:?}"
        );
        let names: Vec<&str> = listed.iter().map(|entry| entry.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "{platform:?}: sorted by name");
        for entry in &listed {
            let top = entry.name.split('/').next().unwrap_or_default();
            assert!(
                ["manifest.toml", "bin", "units"].contains(&top),
                "{}",
                entry.name
            );
            assert!(entry.ustar, "{}: a ustar header", entry.name);
            assert_eq!(
                entry.mode & 0o7000,
                0,
                "{}: no setuid, setgid or sticky bit",
                entry.name
            );
            assert_eq!(
                (entry.uid, entry.gid, entry.mtime),
                (0, 0, 0),
                "{}",
                entry.name
            );
            assert_eq!(
                entry.owner_names,
                (Vec::new(), Vec::new()),
                "{}",
                entry.name
            );
            if entry.kind == tar::EntryType::Directory {
                assert!(entry.content.is_empty(), "{}", entry.name);
            }
        }
    }
}

#[test]
fn the_binary_in_every_package_is_the_input_byte_for_byte() {
    // Catches: a binary changed on the way in (re-signed, stripped, truncated): the signature the
    // macOS linker wrote would no longer match it.
    for platform in Platform::ALL {
        assert_eq!(
            file(&package(platform), "bin/tierstore"),
            binary(),
            "{platform:?}"
        );
    }
}

#[test]
fn two_runs_of_the_verb_give_the_same_bytes_whatever_the_input_files_name_mode_and_time() {
    // Catches: anything of the build machine or the input file (its name, mode, owner or time,
    // the time of the run) reaching the package, so two builds of one commit would differ.
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("hostkeeper-package-twice");
    let inputs = [
        ("tierstore", 0o755, 0),
        ("another-name", 0o600, 1_800_000_000),
    ];
    for platform in Platform::ALL {
        let mut written = Vec::new();
        for (run, (name, mode, mtime)) in inputs.into_iter().enumerate() {
            let dir = scratch.path().join(format!("{}-{run}", platform.name()));
            std::fs::create_dir_all(&dir).expect("dir");
            let bin = dir.join(name);
            std::fs::write(&bin, binary()).expect("binary");
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode)).expect("mode");
            let handle = std::fs::File::options()
                .write(true)
                .open(&bin)
                .expect("open");
            handle
                .set_modified(UNIX_EPOCH + Duration::from_secs(mtime))
                .expect("time");
            let out = dir.join("out");
            hostkeeper_package(&bin, VERSION, MIN_HOSTKEEPER, platform, &out).expect("packaged");
            written.push(std::fs::read(out.join(file_name(VERSION, platform))).expect("package"));
        }
        assert_eq!(written[0], written[1], "{platform:?}");
        assert_eq!(written[0], package(platform), "{platform:?}");
    }
    // The two Linux packages differ: each manifest names its own platform.
    let [x86, arm] = LINUX.map(package);
    assert_ne!(x86, arm);
}

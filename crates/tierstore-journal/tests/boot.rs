//! The boot id (section 8): parsing the kernel's text, reading it from a file, and the rule that
//! only two known and equal ids are the same boot.
//!
//! Catches: a boot id parsed from text that is not a UUID (wrong groups, a non-hex digit, no
//! dashes); a trailing newline not accepted; a missing file reported as an id; an unknown boot
//! id on either side treated as the same boot.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use tierstore_journal::{BootId, BootIdError, same_boot};
use tierstore_testkit::dir::TestDir;

const TEXT: &str = "3f2a1b0c-9d8e-4f7a-b6c5-d4e3f2a1b0c9";

#[test]
fn the_kernel_s_text_parses_and_prints_back() {
    let id = BootId::parse(&format!("{TEXT}\n")).unwrap();
    assert_eq!(
        id.0,
        [
            0x3f, 0x2a, 0x1b, 0x0c, 0x9d, 0x8e, 0x4f, 0x7a, 0xb6, 0xc5, 0xd4, 0xe3, 0xf2, 0xa1,
            0xb0, 0xc9
        ]
    );
    assert_eq!(id.to_string(), TEXT);
    assert_eq!(BootId::parse(TEXT).unwrap(), id, "no newline");
    assert_eq!(
        BootId::parse(&TEXT.to_uppercase()).unwrap(),
        id,
        "uppercase hex"
    );
}

#[test]
fn text_that_is_not_a_uuid_is_malformed() {
    for bad in [
        "",
        "\n",
        "3f2a1b0c9d8e4f7ab6c5d4e3f2a1b0c9",
        "3f2a1b0c-9d8e-4f7a-b6c5-d4e3f2a1b0c",
        "3f2a1b0c-9d8e-4f7a-b6c5-d4e3f2a1b0c9-",
        "3f2a1b0c-9d8e-4f7a-b6c5d-4e3f2a1b0c9",
        "3f2a1bg0-9d8e-4f7a-b6c5-d4e3f2a1b0c9",
        "3f2a1b0c-9d8e-4f7a-b6c5-d4e3f2a1b0cz",
        "3f2a1b0c-9d8e-4f7a-b6c5-d4e3f2a1b0c9\n\n",
    ] {
        let err = BootId::parse(bad).unwrap_err();
        let shown = bad.strip_suffix('\n').unwrap_or(bad);
        assert!(
            matches!(&err, BootIdError::Malformed { text } if text == shown),
            "{bad:?}: {err}"
        );
        assert_eq!(err.to_string(), format!("boot id {shown:?} is not a UUID"));
    }
}

#[test]
fn the_id_is_read_from_a_file_and_a_missing_file_is_an_error() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "boot-id");
    let path = dir.join("boot_id");
    std::fs::write(&path, format!("{TEXT}\n")).unwrap();
    assert_eq!(BootId::read(&path).unwrap().to_string(), TEXT);
    let missing = dir.join("missing");
    let err = BootId::read(&missing).unwrap_err();
    assert!(
        matches!(&err, BootIdError::Read { path, .. } if *path == missing),
        "{err}"
    );
    assert!(
        err.to_string()
            .starts_with(&format!("read {}: ", missing.display())),
        "{err}"
    );
    assert!(std::error::Error::source(&err).is_some());
    std::fs::write(&path, "not an id").unwrap();
    assert!(matches!(
        BootId::read(&path),
        Err(BootIdError::Malformed { .. })
    ));
    assert_eq!(BootId::LINUX_PATH, "/proc/sys/kernel/random/boot_id");
}

#[test]
fn only_two_known_equal_ids_are_the_same_boot() {
    let a = Some(BootId([1; 16]));
    let b = Some(BootId([2; 16]));
    assert!(same_boot(a, a));
    assert!(!same_boot(a, b));
    assert!(!same_boot(a, None));
    assert!(!same_boot(None, a));
    assert!(!same_boot(None, None));
}

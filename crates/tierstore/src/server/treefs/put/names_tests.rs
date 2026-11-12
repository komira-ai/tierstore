//! The names of the put directory: components, directories under the prefixes, keys, and the
//! expected digest of a `.part` name.

use super::error::PutError;
use super::names::{
    MAX_DEPTH, MAX_KEY_BYTES, MAX_NAME_BYTES, PutDir, Sha256Digest, check_component,
    check_prefixes, expected_digest, is_part,
};

fn prefixes() -> Vec<String> {
    vec!["cas/b/".to_owned(), "out/".to_owned()]
}

/// Asserts that a component of 1 to 255 bytes that is not `.` or `..` and has no `/` or NUL is a
/// name, and that each other kind is refused with the error of its kind.
#[test]
fn a_component_is_one_name_of_1_to_255_bytes() {
    check_component("pack.part").unwrap();
    check_component("..x").unwrap();
    check_component(&"a".repeat(MAX_NAME_BYTES)).unwrap();
    check_component("é").unwrap();
    for bad in ["", ".", "..", "a/b", "a\0b"] {
        assert!(
            matches!(check_component(bad), Err(PutError::InvalidName(_))),
            "{bad:?}"
        );
    }
    assert!(matches!(
        check_component(&"a".repeat(MAX_NAME_BYTES + 1)),
        Err(PutError::NameTooLong)
    ));
}

/// Asserts that a directory at or under an allowed prefix is taken, and one beside it, above it, or
/// sharing only the prefix's text without its `/`, is refused as under no prefix.
#[test]
fn a_directory_is_taken_only_at_or_under_an_allowed_prefix() {
    assert_eq!(
        PutDir::new("cas/b/1/n", &prefixes()).unwrap().as_str(),
        "cas/b/1/n"
    );
    assert_eq!(PutDir::new("cas/b", &prefixes()).unwrap().as_str(), "cas/b");
    assert_eq!(PutDir::new("out", &prefixes()).unwrap().as_str(), "out");
    for outside in ["cas", "cas/bx", "cas/c/1", "other/1", "b/cas/b"] {
        assert!(
            matches!(
                PutDir::new(outside, &prefixes()),
                Err(PutError::NotUnderPrefix)
            ),
            "{outside}"
        );
    }
    assert!(matches!(
        PutDir::new("cas/b/1", &[]),
        Err(PutError::NotUnderPrefix)
    ));
}

/// Asserts that a directory with an empty, `.`, `..` or over-long component, a leading or trailing
/// `/`, or more than `MAX_DEPTH` components, is refused before the prefixes are looked at.
#[test]
fn a_directory_of_a_bad_component_or_too_deep_is_refused() {
    for bad in [
        "",
        "cas/b/",
        "/cas/b/1",
        "cas/b//1",
        "cas/b/./1",
        "cas/b/../c",
    ] {
        assert!(
            matches!(PutDir::new(bad, &prefixes()), Err(PutError::InvalidName(_))),
            "{bad:?}"
        );
    }
    let long = format!("cas/b/{}", "a".repeat(MAX_NAME_BYTES + 1));
    assert!(matches!(
        PutDir::new(&long, &prefixes()),
        Err(PutError::NameTooLong)
    ));
    let deepest = format!("cas/b{}", "/d".repeat(MAX_DEPTH - 2));
    let _ = PutDir::new(&deepest, &prefixes()).unwrap();
    let deeper = format!("{deepest}/d");
    assert!(matches!(
        PutDir::new(&deeper, &prefixes()),
        Err(PutError::InvalidName("a directory deeper than the limit"))
    ));
}

/// Asserts that a key is `<dir>/<name>` and that one of more than `MAX_KEY_BYTES` is refused.
#[test]
fn a_key_is_the_directory_and_the_name_up_to_1024_bytes() {
    let dir = PutDir::new("cas/b/1/n", &prefixes()).unwrap();
    assert_eq!(dir.key("pack").unwrap(), "cas/b/1/n/pack");
    let fits = "a".repeat(MAX_KEY_BYTES - "cas/b/1/n/".len());
    assert_eq!(dir.key(&fits).unwrap().len(), MAX_KEY_BYTES);
    assert!(matches!(
        dir.key(&format!("{fits}a")),
        Err(PutError::NameTooLong)
    ));
}

/// Asserts that a list of prefixes is good when each is a directory and `/`, and that the first one
/// that is not is named.
#[test]
fn each_prefix_is_a_directory_and_a_slash() {
    check_prefixes(&prefixes()).unwrap();
    check_prefixes(&[]).unwrap();
    for bad in ["cas/b", "/", "", "cas//", "../x/"] {
        let list = vec!["ok/".to_owned(), bad.to_owned()];
        assert_eq!(check_prefixes(&list), Err(bad.to_owned()), "{bad:?}");
    }
}

/// Asserts that only a name ending `.sha256-<64 lower-case hex>.part` carries an expected digest:
/// upper-case or short hex, a missing `.part`, or no mark, carry none.
#[test]
fn a_part_name_carries_the_digest_of_its_sha256_suffix() {
    let hex = "ab".repeat(32);
    let digest = Sha256Digest::from_hex(&hex).unwrap();
    assert_eq!(digest.to_string(), hex);
    assert_eq!(format!("{digest:?}"), hex);
    assert_eq!(
        expected_digest(&format!("L0.sha256-{hex}.part")),
        Some(digest)
    );
    assert_eq!(
        expected_digest(&format!(".sha256-{hex}.part")),
        Some(digest)
    );
    assert_eq!(
        expected_digest(&format!("a.sha256-x.sha256-{hex}.part")),
        Some(digest)
    );
    for none in [
        format!("L0.sha256-{}.part", hex.to_uppercase()),
        format!("L0.sha256-{}.part", &hex[1..]),
        format!("L0.sha256-{hex}"),
        format!("L0.{hex}.part"),
        "L0.part".to_owned(),
    ] {
        assert_eq!(expected_digest(&none), None, "{none}");
    }
    assert!(is_part("L0.part"));
    assert!(is_part(".part"));
    assert!(!is_part("L0"));
    assert!(!is_part("L0.part.x"));
}

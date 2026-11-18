//! The `NODE` and `DRIVE` identity files: the exact text written and every way a decode refuses a
//! file (the wrong-mount guard reads these).
//!
//! Catches:
//! - a file whose text differs from `key = value` lines in the documented order, or that does not
//!   round trip its ids;
//! - a format version other than 1, a missing field, a wrong key, a wrong order, a wrong separator or
//!   a value that does not parse, that is read as valid;
//! - a line after the last field (including an empty one) that is ignored;
//! - the other kind of file (a `DRIVE` text read as a `NODE`, and the reverse) that is accepted.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use tierstore_format::FormatError;
use tierstore_format::identity::{DriveIdentity, IDENTITY_VERSION, NodeIdentity};
use tierstore_format::ids::{ClusterId, DriveId, NodeId};

const CLUSTER: &str = "00112233445566778899aabbccddeeff";
const DRIVE: &str = "0f0e0d0c0b0a09080706050403020100";
const FS: &str = "ffffffffffffffffffffffffffffffff";

fn node() -> NodeIdentity {
    NodeIdentity {
        cluster: CLUSTER.parse::<ClusterId>().unwrap(),
        node: NodeId(7),
    }
}

fn drive() -> DriveIdentity {
    DriveIdentity {
        cluster: CLUSTER.parse::<ClusterId>().unwrap(),
        node: NodeId(255),
        drive: DRIVE.parse::<DriveId>().unwrap(),
        filesystem: FS.parse::<DriveId>().unwrap(),
    }
}

fn node_text() -> String {
    format!("format = 1\ncluster = {CLUSTER}\nnode = 7\n")
}

fn drive_text() -> String {
    format!("format = 1\ncluster = {CLUSTER}\nnode = 255\ndrive = {DRIVE}\nfilesystem = {FS}\n")
}

fn node_err(text: &str) -> FormatError {
    NodeIdentity::decode(text).unwrap_err()
}

fn drive_err(text: &str) -> FormatError {
    DriveIdentity::decode(text).unwrap_err()
}

const NOT_KEY_VALUE: &str = "a line is not the expected `key = value`";

#[test]
fn a_node_file_is_the_documented_text_and_round_trips() {
    assert_eq!(IDENTITY_VERSION, 1);
    assert_eq!(node().encode(), node_text());
    assert_eq!(NodeIdentity::decode(&node_text()).unwrap(), node());
    let zero = NodeIdentity {
        cluster: ClusterId([0; 16]),
        node: NodeId(0),
    };
    assert_eq!(NodeIdentity::decode(&zero.encode()).unwrap(), zero);
}

#[test]
fn a_drive_file_is_the_documented_text_and_round_trips() {
    assert_eq!(drive().encode(), drive_text());
    assert_eq!(DriveIdentity::decode(&drive_text()).unwrap(), drive());
}

#[test]
fn another_format_version_is_unsupported_for_both_files() {
    for version in [0u32, 2, 4_000_000_000] {
        let text = node_text().replacen("format = 1", &format!("format = {version}"), 1);
        assert_eq!(
            node_err(&text),
            FormatError::UnsupportedVersion {
                what: "NODE file",
                version
            }
        );
        let text = drive_text().replacen("format = 1", &format!("format = {version}"), 1);
        assert_eq!(
            drive_err(&text),
            FormatError::UnsupportedVersion {
                what: "DRIVE file",
                version
            }
        );
    }
}

#[test]
fn an_empty_file_or_a_missing_field_is_refused() {
    assert_eq!(node_err("").to_string(), "NODE file: a field is missing");
    assert_eq!(
        node_err("format = 1\n").to_string(),
        "NODE file: a field is missing"
    );
    assert_eq!(
        node_err(&format!("format = 1\ncluster = {CLUSTER}\n")).to_string(),
        "NODE file: a field is missing"
    );
    assert_eq!(drive_err("").to_string(), "DRIVE file: a field is missing");
    for keep in 1..5 {
        let text: String = drive_text()
            .lines()
            .take(keep)
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(
            drive_err(&text).to_string(),
            "DRIVE file: a field is missing",
            "{keep} lines"
        );
    }
}

#[test]
fn a_wrong_key_a_wrong_order_or_a_wrong_separator_is_refused() {
    let wrong_first = format!("cluster = {CLUSTER}\nformat = 1\nnode = 7\n");
    assert_eq!(
        node_err(&wrong_first).to_string(),
        format!("NODE file: {NOT_KEY_VALUE}")
    );
    let swapped = format!("format = 1\nnode = 7\ncluster = {CLUSTER}\n");
    assert_eq!(
        node_err(&swapped).to_string(),
        format!("NODE file: {NOT_KEY_VALUE}")
    );
    for text in [
        format!("format = 1\ncluster = {CLUSTER}\nnodes = 7\n"),
        format!("format = 1\ncluster = {CLUSTER}\nnode=7\n"),
        format!("format = 1\ncluster = {CLUSTER}\nnode  = 7\n"),
        format!("format = 1\ncluster = {CLUSTER}\n node = 7\n"),
        format!("format = 1\ncluster = {CLUSTER}\nno = 7\n"),
        format!("format = 1\ncluster = {CLUSTER}\nnode =7\n"),
        format!("format = 1\ncluster = {CLUSTER}\n\n"),
    ] {
        assert_eq!(
            node_err(&text).to_string(),
            format!("NODE file: {NOT_KEY_VALUE}"),
            "{text:?}"
        );
    }
    let drive_swapped =
        format!("format = 1\ncluster = {CLUSTER}\nnode = 1\nfilesystem = {FS}\ndrive = {DRIVE}\n");
    assert_eq!(
        drive_err(&drive_swapped).to_string(),
        format!("DRIVE file: {NOT_KEY_VALUE}")
    );
}

#[test]
fn a_value_that_does_not_parse_is_refused() {
    for text in [
        format!("format = x\ncluster = {CLUSTER}\nnode = 7\n"),
        format!("format = \ncluster = {CLUSTER}\nnode = 7\n"),
        format!("format = 1\ncluster = {}\nnode = 7\n", &CLUSTER[1..]),
        format!(
            "format = 1\ncluster = {}\nnode = 7\n",
            CLUSTER.to_uppercase()
        ),
        format!("format = 1\ncluster = {CLUSTER}\nnode = 256\n"),
        format!("format = 1\ncluster = {CLUSTER}\nnode = -1\n"),
        format!("format = 1\ncluster = {CLUSTER}\nnode = seven\n"),
        format!("format = 1\ncluster = {CLUSTER}\nnode = \n"),
    ] {
        assert_eq!(
            node_err(&text).to_string(),
            format!("NODE file: {NOT_KEY_VALUE}"),
            "{text:?}"
        );
    }
    let bad_drive = drive_text().replace(DRIVE, &DRIVE[2..]);
    assert_eq!(
        drive_err(&bad_drive).to_string(),
        format!("DRIVE file: {NOT_KEY_VALUE}")
    );
    let bad_fs = drive_text().replace(FS, &FS.to_uppercase());
    assert_eq!(
        drive_err(&bad_fs).to_string(),
        format!("DRIVE file: {NOT_KEY_VALUE}")
    );
}

#[test]
fn a_line_after_the_last_field_is_refused() {
    for tail in ["extra = 1\n", "\n", "node = 8\n", "x"] {
        let text = format!("{}{tail}", node_text());
        assert_eq!(
            node_err(&text).to_string(),
            "NODE file: a line after the last field",
            "{tail:?}"
        );
        let text = format!("{}{tail}", drive_text());
        assert_eq!(
            drive_err(&text).to_string(),
            "DRIVE file: a line after the last field",
            "{tail:?}"
        );
    }
}

#[test]
fn a_drive_file_is_not_a_node_file_and_the_reverse() {
    assert_eq!(
        node_err(&drive_text()).to_string(),
        "NODE file: a line after the last field"
    );
    assert_eq!(
        drive_err(&node_text()).to_string(),
        "DRIVE file: a field is missing"
    );
}

#[test]
fn a_value_in_a_non_canonical_form_is_refused() {
    let canonical_reason = FormatError::Invalid {
        what: "identity file",
        reason: "not in the canonical form this release writes",
    };
    let node = node_text();
    for text in [
        node.replace("node = 7", "node = 07"),
        node.replace("node = 7", "node = +7"),
        node.replace('\n', "\r\n"),
        node.trim_end_matches('\n').to_owned(),
    ] {
        assert_eq!(node_err(&text), canonical_reason, "{text:?}");
    }
    let drive = drive_text();
    assert_eq!(drive_err(&drive.replace('\n', "\r\n")), canonical_reason);
}

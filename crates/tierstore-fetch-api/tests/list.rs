//! The list reader's types (`list`).
//!
//! Proves the refusal labels (the `reason` label of `list_refused_total`) and the exact text of
//! `ListVersionUnsupported` for every refusal, and the constants of the L2 header and pack flags.
//!
//! Catches:
//! - a refusal whose metric label changes, or collides with another's;
//! - a refusal message that loses its reason;
//! - a changed digest-function value or pack flag bit.

use tierstore_fetch_api::ids::PackKey;
use tierstore_fetch_api::list::{
    CELL_DIGEST_SHA256, ListVersionUnsupported, NodeIndex, PACK_FULLY_COVERED, PACK_SHARED,
    PackRecord, Refusal,
};

#[test]
fn the_l2_constants_are_the_documented_values() {
    assert_eq!(CELL_DIGEST_SHA256, 1);
    assert_eq!(PACK_FULLY_COVERED, 1);
    assert_eq!(PACK_SHARED, 2);
}

#[test]
fn each_refusal_has_its_own_metric_label() {
    let all = [
        (Refusal::V1List, "v1_list"),
        (Refusal::MissingSidecar, "missing_sidecar"),
        (Refusal::UnknownCellDigest(9), "unknown_cell_digest"),
        (Refusal::UnknownVersion(3), "unknown_version"),
    ];
    for (r, label) in all {
        assert_eq!(r.label(), label);
    }
}

#[test]
fn the_label_of_a_refusal_does_not_depend_on_the_value_it_carries() {
    assert_eq!(
        Refusal::UnknownCellDigest(0).label(),
        Refusal::UnknownCellDigest(255).label()
    );
    assert_eq!(
        Refusal::UnknownVersion(0).label(),
        Refusal::UnknownVersion(u32::MAX).label()
    );
    assert_ne!(Refusal::UnknownCellDigest(1), Refusal::UnknownCellDigest(2));
}

#[test]
fn the_unsupported_error_names_its_reason_exactly() {
    for (reason, text) in [
        (Refusal::V1List, "tree list not supported: v1_list"),
        (
            Refusal::MissingSidecar,
            "tree list not supported: missing_sidecar",
        ),
        (
            Refusal::UnknownCellDigest(2),
            "tree list not supported: unknown_cell_digest",
        ),
        (
            Refusal::UnknownVersion(7),
            "tree list not supported: unknown_version",
        ),
    ] {
        let e = ListVersionUnsupported { reason };
        assert_eq!(e.to_string(), text);
        assert_eq!(e, ListVersionUnsupported { reason });
        assert_eq!(e.reason, reason);
    }
}

#[test]
fn a_pack_record_and_a_node_index_compare_by_value() {
    let rec = PackRecord {
        key: PackKey("p".into()),
        pack_size: 5,
        cell_first: 0,
        cell_count: 1,
        cell_shift: 20,
        pack_flags: PACK_FULLY_COVERED,
    };
    let mut other = rec.clone();
    assert_eq!(rec, other);
    other.pack_size = 6;
    assert_ne!(rec, other);
    assert!(NodeIndex(1) < NodeIndex(2));
}

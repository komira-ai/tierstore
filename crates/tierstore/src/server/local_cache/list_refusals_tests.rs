//! The provider's refused tree lists and missing sidecars on `/metricz`: every reason shown at zero
//! before any refusal, each refusal counted under its own reason whatever its payload, and a
//! missing sidecar and a tree placed from its list each counted in its own family, apart from the
//! refusals.

use tierstore_fetch_api::list::Refusal;

use super::super::super::metrics::bump;
use super::{ListRefusals, REFUSED, SIDECAR_FALLBACK, SIDECAR_MISSING};

/// The text `render` gives for the counts of `v1_list`, `missing_sidecar`,
/// `unknown_cell_digest` and `unknown_version`, in that order, then of the preparations that found
/// no sidecar and of the trees placed from their list.
fn expected([v1, missing, digest, version]: [u64; 4], absent: u64, fallback: u64) -> String {
    format!(
        "# TYPE {REFUSED} counter\n\
         {REFUSED}{{reason=\"v1_list\"}} {v1}\n\
         {REFUSED}{{reason=\"missing_sidecar\"}} {missing}\n\
         {REFUSED}{{reason=\"unknown_cell_digest\"}} {digest}\n\
         {REFUSED}{{reason=\"unknown_version\"}} {version}\n\
         # TYPE {SIDECAR_MISSING} counter\n\
         {SIDECAR_MISSING} {absent}\n\
         # TYPE {SIDECAR_FALLBACK} counter\n\
         {SIDECAR_FALLBACK} {fallback}\n"
    )
}

/// Asserts the families before any refusal: all three typed, every reason and count at zero.
#[test]
fn every_reason_is_shown_at_zero_before_a_refusal() {
    assert_eq!(REFUSED, "tierstore_list_refused_total");
    assert_eq!(SIDECAR_MISSING, "tierstore_l2_sidecar_missing_total");
    assert_eq!(SIDECAR_FALLBACK, "tierstore_l2_sidecar_fallback_total");
    assert_eq!(ListRefusals::default().render(), expected([0; 4], 0, 0));
}

/// Asserts that each refusal counts once under its own reason, and that the digest function or
/// the version a refusal names does not make a label of its own; and that no refusal counts as a
/// missing sidecar or a tree placed from its list.
#[test]
fn each_refusal_counts_under_its_reason_alone() {
    let refusals = ListRefusals::default();
    for refusal in [
        Refusal::V1List,
        Refusal::MissingSidecar,
        Refusal::MissingSidecar,
        Refusal::UnknownCellDigest(2),
        Refusal::UnknownCellDigest(9),
        Refusal::UnknownCellDigest(255),
        Refusal::UnknownVersion(7),
    ] {
        refusals.refused(refusal);
    }
    assert_eq!(refusals.render(), expected([1, 2, 3, 1], 0, 0));
}

/// Asserts that a missing sidecar and a tree placed from its list are counted each in its own
/// family, and not as a refusal under any reason (`missing_sidecar` included).
#[test]
fn a_missing_sidecar_and_a_fallback_count_apart_from_the_refusals() {
    let refusals = ListRefusals::default();
    for _ in 0..3 {
        bump(&refusals.missing);
    }
    bump(&refusals.fallback);
    bump(&refusals.fallback);
    assert_eq!(refusals.render(), expected([0; 4], 3, 2));
}

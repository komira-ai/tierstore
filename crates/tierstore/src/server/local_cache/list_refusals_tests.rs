//! The provider's refused tree lists on `/metricz`: every reason shown at zero before any refusal,
//! each refusal counted under its own reason whatever its payload, and a missing sidecar counted
//! in its family too.

use tierstore_fetch_api::list::Refusal;

use super::{ListRefusals, REFUSED, SIDECAR_MISSING};

/// The text `render` gives for the counts of `v1_list`, `missing_sidecar`,
/// `unknown_cell_digest` and `unknown_version`, in that order.
fn expected([v1, missing, digest, version]: [u64; 4]) -> String {
    format!(
        "# TYPE {REFUSED} counter\n\
         {REFUSED}{{reason=\"v1_list\"}} {v1}\n\
         {REFUSED}{{reason=\"missing_sidecar\"}} {missing}\n\
         {REFUSED}{{reason=\"unknown_cell_digest\"}} {digest}\n\
         {REFUSED}{{reason=\"unknown_version\"}} {version}\n\
         # TYPE {SIDECAR_MISSING} counter\n\
         {SIDECAR_MISSING} {missing}\n"
    )
}

/// Asserts the families before any refusal: both typed, every reason at zero.
#[test]
fn every_reason_is_shown_at_zero_before_a_refusal() {
    assert_eq!(REFUSED, "tierstore_list_refused_total");
    assert_eq!(SIDECAR_MISSING, "tierstore_l2_sidecar_missing_total");
    assert_eq!(ListRefusals::default().render(), expected([0; 4]));
}

/// Asserts that each refusal counts once under its own reason, that the digest function or the
/// version a refusal names does not make a label of its own, and that only a missing sidecar
/// counts in `tierstore_l2_sidecar_missing_total`.
#[test]
fn each_refusal_counts_under_its_reason_and_a_missing_sidecar_in_its_family_too() {
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
    assert_eq!(refusals.render(), expected([1, 2, 3, 1]));
}

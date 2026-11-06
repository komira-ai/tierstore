//! Each rule judged on its own over the passing scenarios: which rules the document's protocol
//! as written breaks, and that the corrected one breaks none.

use tierstore_model::scenarios::matrix;

fn broken(literal: bool) -> Vec<&'static str> {
    matrix(literal)
        .iter()
        .filter(|r| r.broken.is_some())
        .map(|r| r.rule)
        .collect()
}

#[test]
#[ignore = "re-runs 27 scenarios once per rule (about 25 s with the literal run); run with --ignored"]
fn the_corrected_protocol_breaks_no_rule() {
    assert_eq!(broken(false), Vec::<&str>::new());
}

#[test]
#[ignore = "re-runs 31 scenarios once per rule (about 12 s); run with --ignored"]
fn the_protocol_as_written_breaks_six_rules() {
    assert_eq!(broken(true), ["A1", "A2", "A4", "A5", "A7", "A12"]);
}

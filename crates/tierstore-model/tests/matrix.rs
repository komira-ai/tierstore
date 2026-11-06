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
#[ignore = "re-runs 35 scenarios once per rule (about 12 s); run with --ignored"]
fn the_protocol_as_written_breaks_six_rules() {
    assert_eq!(broken(true), ["A1", "A2", "A5", "A7", "A8", "A12"]);
}

use tierstore_model::scenarios::matrix_of;

#[test]
fn the_protocol_as_written_breaks_the_rule_its_counterexample_names() {
    let rows = matrix_of(true, Some(&["doc/recreate-read", "fixed/recreate-read"]));
    let broken: Vec<_> = rows
        .iter()
        .filter(|r| r.broken.is_some())
        .map(|r| r.rule)
        .collect();
    assert_eq!(broken, ["A2"]);
    let a4 = rows.iter().find(|r| r.rule == "A4");
    assert_eq!(a4.map(|r| r.judged), Some(false));
    assert!(rows.iter().all(|r| r.scenarios == 2), "{rows:?}");
}

#[test]
fn the_corrected_protocol_on_a_small_set_breaks_no_rule() {
    let rows = matrix_of(false, Some(&["fixed/recreate-read"]));
    assert!(
        rows.iter().all(|r| r.broken.is_none() && r.judged),
        "{rows:?}"
    );
}

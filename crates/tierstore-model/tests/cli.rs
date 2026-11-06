//! The command line: verdict lines, traces, the list and the ablation.

use tierstore_model::cli::{run_cli, write_ablation, write_row};
use tierstore_model::scenarios::Row;

fn run(args: &[&str]) -> (bool, String) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let mut out = Vec::new();
    let ok = match run_cli(&args, &mut out) {
        Ok(ok) => ok,
        Err(e) => panic!("writing to a vector failed: {e}"),
    };
    (ok, String::from_utf8_lossy(&out).into_owned())
}

#[test]
fn a_scenario_that_meets_its_verdict_prints_one_line() {
    let (ok, text) = run(&["if-match-race"]);
    assert!(ok);
    assert_eq!(text.lines().count(), 1);
    assert!(
        text.contains("if-match-race") && text.contains("PASS"),
        "{text}"
    );
}

#[test]
fn a_rejected_variant_prints_its_trace_only_when_asked() {
    let (ok, quiet) = run(&["negative/no-marker-on-put"]);
    assert!(ok);
    assert_eq!(quiet.lines().count(), 1);
    assert!(quiet.contains("FAIL as required (A7)"), "{quiet}");
    let (ok, loud) = run(&["negative/no-marker-on-put", "-v"]);
    assert!(ok);
    assert!(loud.lines().count() > 3);
    assert!(loud.contains("the request arrives"), "{loud}");
}

#[test]
fn the_list_has_a_line_for_every_scenario_with_its_verdict() {
    let (ok, text) = run(&["list"]);
    assert!(ok);
    assert!(text.lines().count() > 80);
    assert!(text.contains("doc/recreate-read\tfalse\tfail:A2"), "{text}");
    assert!(text.contains("overwrite-race\tfalse\tpass"), "{text}");
}

#[test]
fn every_correction_is_needed_by_some_scenario() {
    let (ok, text) = run(&["ablation"]);
    assert!(ok, "{text}");
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("needed: without it"))
            .count(),
        11,
        "{text}"
    );
}

#[test]
fn an_unknown_name_runs_nothing() {
    let (ok, text) = run(&["no-such-scenario"]);
    assert!(ok);
    assert!(text.is_empty());
}

#[test]
fn the_trace_command_prints_the_shortest_counterexample_of_one_rule() {
    let (ok, text) = run(&["trace", "doc/recreate-read", "literal", "A2"]);
    assert!(ok);
    assert!(text.starts_with("A2 a Get returned version 1"), "{text}");
    assert!(text.contains("hot missed: read cold"), "{text}");
    let (ok, text) = run(&["trace", "fixed/recreate-read", "corrected", "A2"]);
    assert!(ok);
    assert_eq!(text.trim(), "A2 holds in fixed/recreate-read");
    let (ok, text) = run(&["trace", "no-such", "literal", "A2"]);
    assert!(!ok);
    assert!(text.contains("no scenario called"), "{text}");
}

#[test]
fn the_corrected_table_passes_every_rule() {
    let (ok, text) = run(&["corrected"]);
    assert!(ok);
    assert_eq!(
        text.lines().filter(|l| l.contains("PASS")).count(),
        12,
        "{text}"
    );
}

#[test]
fn with_no_name_the_pr_set_runs_and_the_heavy_runs_do_not() {
    let (ok, text) = run(&[]);
    assert!(ok, "{text}");
    assert!(text.contains("if-match-race"), "{text}");
    assert!(!text.contains("heavy/"), "{text}");
}

#[test]
fn the_name_heavy_runs_only_the_heavy_scenarios() {
    let (ok, text) = run(&["heavy"]);
    assert!(ok, "{text}");
    assert!(text.lines().all(|l| l.starts_with("heavy/")), "{text}");
}

#[test]
fn a_matrix_row_says_pass_fail_or_not_judged() {
    let row = |judged, broken| Row {
        rule: "A4",
        what: "converges",
        scenarios: 3,
        states: 10,
        broken,
        judged,
    };
    let text = |r: Row| {
        let mut out = Vec::new();
        write_row(&mut out, &r).unwrap_or_else(|e| panic!("{e}"));
        String::from_utf8_lossy(&out).into_owned()
    };
    assert_eq!(
        text(row(true, None)),
        "A4        scenarios=3   states=10        PASS (converges)\n"
    );
    assert!(text(row(false, None)).contains("NOT JUDGED (every run"));
    assert!(
        text(row(true, Some(("x/y", "A4 broke".into()))))
            .contains("FAIL in x/y: A4 broke (converges)")
    );
}

#[test]
fn a_correction_no_scenario_needs_fails_the_ablation_table() {
    let mut out = Vec::new();
    let rows = [
        ("fix_a", Some(("s/1", "A1 broke".to_string()))),
        ("fix_b", None),
    ];
    let ok = write_ablation(&mut out, &rows).unwrap_or_else(|e| panic!("{e}"));
    let text = String::from_utf8_lossy(&out);
    assert!(!ok);
    assert!(
        text.contains("needed: without it s/1 fails: A1 broke"),
        "{text}"
    );
    assert!(
        text.contains("fix_b") && text.contains("NOT needed by any scenario"),
        "{text}"
    );
}

#[test]
fn the_trace_command_refuses_a_baseline_scenario_and_an_unknown_rule() {
    let (ok, text) = run(&["trace", "baseline/crash", "literal", "A2"]);
    assert!(!ok);
    assert!(
        text.contains("baseline/crash is not a marker scenario"),
        "{text}"
    );
    let (ok, text) = run(&["trace", "if-match-race", "literal", "A99"]);
    assert!(!ok);
    assert!(text.contains("no rule called A99"), "{text}");
}

#[test]
fn the_binary_forwards_to_the_library_and_exits_with_its_verdict() {
    let exe = env!("CARGO_BIN_EXE_tierstore-model");
    let good = std::process::Command::new(exe)
        .arg("if-match-race")
        .output()
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(good.status.success());
    assert!(String::from_utf8_lossy(&good.stdout).contains("PASS"));
    let bad = std::process::Command::new(exe)
        .args(["trace", "no-such", "literal", "A2"])
        .output()
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(!bad.status.success());
}

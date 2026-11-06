//! The command line: verdict lines, traces, the list and the ablation.

use tierstore_model::cli::run_cli;

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

//! The conditions of `macos.yml`'s macOS job and of its one check, `macos-checks`, read from the
//! file and evaluated ([`super::expr`]) against every outcome of the jobs they name: the macOS job
//! runs unless the pull request is known not to need it, and the check passes only when the macOS
//! checks passed or were not needed.

use std::collections::BTreeMap;

use super::expr::{Context, holds};
use super::lint::entry;
use super::macos_pr::{GATE_JOB, MAC_JOB};
use super::platforms::{at, steps};
use super::text_of;
use super::yaml;

/// `(the macOS job's if, the gate job's if, the gate step's if)` of `text`.
fn conditions(text: &str) -> (String, String, String) {
    let doc = yaml::load(text).expect("load");
    let job = |name: &str| {
        entry(&doc, "jobs")
            .and_then(|j| entry(j, name))
            .unwrap_or_else(|| panic!("no job {name}"))
    };
    let gate = job(GATE_JOB);
    (
        at(job(MAC_JOB), &["if"]).expect("macos if").to_owned(),
        at(gate, &["if"]).expect("gate if").to_owned(),
        at(&steps(Some(gate))[0], &["if"])
            .expect("gate step if")
            .to_owned(),
    )
}

/// One outcome of the jobs before the gate.
#[derive(Debug, Clone, Copy)]
struct Outcome {
    cancelled: bool,
    changes: &'static str,
    /// The answer `changes` gave, `""` for none.
    answer: &'static str,
    version: &'static str,
    /// What the macOS job ends as, if it runs.
    macos: &'static str,
}

const RESULTS: [&str; 3] = ["success", "failure", "cancelled"];
const ANSWERS: [&str; 4] = ["true", "false", "", "FALSE"];

fn outcomes() -> Vec<Outcome> {
    let mut all = Vec::new();
    for cancelled in [false, true] {
        for changes in RESULTS {
            for answer in ANSWERS {
                for version in RESULTS {
                    for macos in RESULTS {
                        all.push(Outcome {
                            cancelled,
                            changes,
                            answer,
                            version,
                            macos,
                        });
                    }
                }
            }
        }
    }
    all
}

/// `(whether the macOS job runs, its result, whether the gate passes)` for `o` under the
/// conditions of `text`.
fn run(text: &str, o: Outcome) -> (bool, &'static str, bool) {
    let (mac_if, gate_if, step_if) = conditions(text);
    let mut names = BTreeMap::from([
        ("needs.changes.result".to_owned(), o.changes.to_owned()),
        (
            "needs.changes.outputs.macos".to_owned(),
            o.answer.to_owned(),
        ),
        ("needs.version.result".to_owned(), o.version.to_owned()),
    ]);
    let mut context = Context {
        names: names.clone(),
        cancelled: o.cancelled,
    };
    let runs = holds(&mac_if, &context).expect("macos if");
    let macos = if runs { o.macos } else { "skipped" };
    names.insert("needs.macos.result".to_owned(), macos.to_owned());
    context.names = names;
    assert!(
        holds(&gate_if, &context).expect("gate if"),
        "{o:?}: the gate is skipped"
    );
    let fails = holds(&step_if, &context).expect("gate step if");
    (runs, macos, !fails)
}

#[test]
fn the_macos_job_is_skipped_only_on_a_passing_changes_job_that_answered_false() {
    // Catches: a macOS job skipped when `changes` failed, was cancelled or gave no answer (the
    // diff could not be judged), or after an answer of `true`; and one that runs after a failed
    // `version` or in a cancelled run.
    let text = text_of("macos.yml");
    for o in outcomes() {
        let not_needed = o.changes == "success" && o.answer.eq_ignore_ascii_case("false");
        let want = !o.cancelled && o.version == "success" && !not_needed;
        assert_eq!(run(&text, o).0, want, "{o:?}");
    }
}

#[test]
fn the_gate_passes_only_when_the_macos_checks_passed_or_were_not_needed() {
    // Catches: a gate that passes on a failed or cancelled macOS job, on a failed `version`, on
    // a skipped macOS job that `changes` did not clear (it failed, or answered `true` or nothing),
    // and one that fails a pull request whose macOS job passed or was rightly not needed.
    let text = text_of("macos.yml");
    let mut passed = 0;
    for o in outcomes() {
        let (_, macos, passes) = run(&text, o);
        let not_needed = o.changes == "success" && o.answer.eq_ignore_ascii_case("false");
        let want =
            o.version == "success" && (macos == "success" || (macos == "skipped" && not_needed));
        assert_eq!(passes, want, "{o:?}");
        passed += usize::from(passes);
    }
    // Both arms are reached: a pass on a macOS job that passed, and one on a job not needed.
    assert!(passed > 0);
    let skipped = Outcome {
        cancelled: false,
        changes: "success",
        answer: "false",
        version: "success",
        macos: "failure",
    };
    assert_eq!(run(&text, skipped), (false, "skipped", true));
    let ran = Outcome {
        answer: "true",
        macos: "success",
        ..skipped
    };
    assert_eq!(run(&text, ran), (true, "success", true));
    let failed = Outcome {
        macos: "failure",
        ..ran
    };
    assert_eq!(run(&text, failed), (true, "failure", false));
    let undecided = Outcome {
        changes: "failure",
        answer: "",
        macos: "success",
        ..skipped
    };
    assert_eq!(run(&text, undecided), (true, "success", true));
}

#[test]
fn a_gate_or_macos_condition_that_lets_a_defect_through_is_caught_by_the_tests_above() {
    // Catches: the two tests above passing whatever the conditions say (they would then hold
    // nothing): each spoiled condition makes one of their expectations false.
    let text = text_of("macos.yml");
    for (replace, with) in [
        (" && needs.changes.outputs.macos == 'false'))) }}", "))) }}"),
        (
            "!(needs.changes.result == 'success' && needs.changes.outputs.macos == 'false') }}",
            "needs.changes.outputs.macos == 'true' }}",
        ),
        ("${{ always() && !(", "${{ !cancelled() && !("),
        (
            "needs.macos.result == 'success' ||",
            "needs.macos.result != 'failure' ||",
        ),
    ] {
        assert!(text.contains(replace), "{replace:?}");
        let spoiled = text.replacen(replace, with, 1);
        let wrong = outcomes().into_iter().any(|o| {
            let (runs, macos, passes) = run(&spoiled, o);
            let not_needed = o.changes == "success" && o.answer.eq_ignore_ascii_case("false");
            let should_run = !o.cancelled && o.version == "success" && !not_needed;
            let should_pass = o.version == "success"
                && (macos == "success" || (macos == "skipped" && not_needed));
            runs != should_run || passes != should_pass
        });
        assert!(wrong, "{with:?}");
    }
}

#[test]
fn the_expressions_are_read_with_githubs_precedence_and_refused_when_not_understood() {
    // Catches: an evaluator that reads `!a == b` as `!(a == b)`, `a || b && c` as `(a || b) && c`,
    // compares with regard to case, or reads what it does not understand as false.
    let context = Context {
        names: BTreeMap::from([("a.b".to_owned(), "X".to_owned())]),
        cancelled: true,
    };
    for (text, want) in [
        ("${{ a.b == 'x' }}", true),
        ("${{ a.b != 'x' }}", false),
        ("${{ unset == '' }}", true),
        ("${{ always() || always() && !always() }}", true),
        ("${{ (always() || always()) && !always() }}", false),
        ("${{ cancelled() }}", true),
        ("${{ !cancelled() }}", false),
        ("${{ a.b }}", true),
        ("${{ unset }}", false),
    ] {
        assert_eq!(holds(text, &context), Ok(want), "{text}");
    }
    for (text, why) in [
        ("a.b", "\"a.b\" is not `${{ … }}`"),
        ("${{ 'x }}", "an unclosed string in \" 'x \""),
        (
            "${{ a.b > 'x' }}",
            "'>' is not understood in \" a.b > 'x' \"",
        ),
        ("${{ (always() }}", "a `(` that is not closed"),
        ("${{ success() }}", "the function success is not understood"),
        ("${{ always('x') }}", "always( takes no arguments here"),
        (
            "${{ always() == 'x' }}",
            "Bool(true) compared with Text(\"x\"): only strings are compared here",
        ),
        // `!` binds tighter than `==`: `(!a.b) == 'x'`, a comparison of a boolean.
        (
            "${{ !a.b == 'x' }}",
            "Bool(false) compared with Text(\"x\"): only strings are compared here",
        ),
        ("${{ && }}", "Some(And) where a value was expected"),
        (
            "${{ always() always() }}",
            "[Name(\"always\"), Open, Close] left over in \"${{ always() always() }}\"",
        ),
    ] {
        assert_eq!(holds(text, &context), Err(why.to_owned()), "{text}");
    }
}

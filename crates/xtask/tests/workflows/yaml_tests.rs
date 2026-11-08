//! Tests of the YAML reader ([`super::yaml`]): a plain scalar holding `: ` is refused with
//! the line GitHub refuses it on, and ` #` is read as the start of a comment, as GitHub
//! reads it.

use super::lint::lint;
use super::text_of;
use super::yaml::{self, Value};

/// The `run:` of komira-ai/tierstore#46, written as a plain scalar: `): ` ends the
/// scalar, so GitHub refuses the file.
const ISSUE_RUN: &str = r#"run: echo "::notice::off (the variable is unset): no farm was told""#;

/// The command of [`ISSUE_RUN`], as the step would run it.
const ISSUE_COMMAND: &str = r#"echo "::notice::off (the variable is unset): no farm was told""#;

/// `ci.yml` with `step` (one line, or several joined by `\n`, at the step's indentation)
/// inserted as the first step of its first job, and the 1-based line the step starts on.
fn ci_with_step(step: &str) -> (String, usize) {
    let text = text_of("ci.yml");
    let at = text
        .find("\n    steps:\n")
        .expect("ci.yml has a job with steps")
        + "\n    steps:\n".len();
    let line = text[..at].lines().count() + 1;
    let step = step.replace('\n', "\n        ");
    let spoiled = format!("{}      - {step}\n{}", &text[..at], &text[at..]);
    (spoiled, line)
}

/// The text of the first scalar in `node` whose text starts with `prefix`, and whether it
/// was plain.
fn scalar_starting(node: &yaml::Node, prefix: &str) -> Option<(String, bool)> {
    match &node.value {
        Value::Scalar { text, plain } => text.starts_with(prefix).then(|| (text.clone(), *plain)),
        Value::Seq(items) => items.iter().find_map(|n| scalar_starting(n, prefix)),
        Value::Map(entries) => entries
            .iter()
            .find_map(|(k, v)| scalar_starting(k, prefix).or_else(|| scalar_starting(v, prefix))),
    }
}

#[test]
fn the_issue_run_as_a_plain_scalar_is_refused_on_its_own_line() {
    // Catches: a reader that accepts the run of komira-ai/tierstore#46, which GitHub refuses
    // ("workflow file issue", no jobs), and one that refuses it without saying where.
    let (spoiled, line) = ci_with_step(ISSUE_RUN);
    let column = "      - ".len() + ISSUE_RUN.find("): ").expect("the issue's `): `") + 2;
    assert_eq!(
        lint("ci.yml", &spoiled),
        vec![format!(
            "line {line}: not valid YAML: mapping values are not allowed in this context \
             (column {column})"
        )]
    );
}

#[test]
fn a_colon_space_on_a_later_line_of_a_plain_scalar_names_that_line() {
    // Catches: an error that names the line the scalar starts on, or the first line of the
    // file, when the `: ` is on a continuation line.
    let (spoiled, line) = ci_with_step("run: echo one\n  two: three");
    assert_eq!(
        lint("ci.yml", &spoiled),
        vec![format!(
            "line {}: not valid YAML: mapping values are not allowed in this context \
             (column 14)",
            line + 1
        )]
    );
}

#[test]
fn a_colon_tab_in_a_plain_scalar_is_refused_on_its_line() {
    // Catches: a reader that accepts `:` followed by a tab, which ends a plain scalar as a
    // space does.
    let (spoiled, line) = ci_with_step("run: echo a:\tb");
    let problems = lint("ci.yml", &spoiled);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with(&format!("line {line}: not valid YAML: ")),
        "{problems:?}"
    );
}

#[test]
fn an_error_on_the_first_line_of_the_file_names_line_one() {
    // Catches: a 0-based line, which would name line 0 for the first line of the file.
    assert_eq!(
        yaml::load("on: a: b\n").map(|_| ()),
        Err(
            "line 1: not valid YAML: mapping values are not allowed in this context (column 6)"
                .to_owned()
        )
    );
}

#[test]
fn an_unclosed_quote_names_the_line_it_opens_on() {
    // Catches: an error found only at the end of the text that names line 0, or a line
    // other than the one the quote opens on.
    assert_eq!(
        lint("t.yml", "name: t\non: 'push\n"),
        vec![
            "line 2: not valid YAML: while scanning a quoted scalar, found unexpected end of \
             stream (column 5)"
                .to_owned()
        ]
    );
}

#[test]
fn the_issue_run_as_a_block_or_a_quoted_scalar_is_read_whole() {
    // Catches: a reader that refuses the forms GitHub accepts for the same command, or that
    // reads them as anything but the command.
    let quoted = format!("run: '{}'", ISSUE_COMMAND.replace('\'', "''"));
    let double = format!("run: \"{}\"", ISSUE_COMMAND.replace('"', "\\\""));
    for step in [format!("run: |\n  {ISSUE_COMMAND}"), quoted, double] {
        let (spoiled, _) = ci_with_step(&step);
        assert_eq!(lint("ci.yml", &spoiled), Vec::<String>::new(), "{step}");
        let doc = yaml::load(&spoiled).expect("valid YAML");
        let (text, plain) = scalar_starting(&doc, "echo \"::notice::").expect(&step);
        assert_eq!(text.trim_end(), ISSUE_COMMAND, "{step}");
        assert!(!plain, "{step}");
    }
}

#[test]
fn a_space_hash_in_a_plain_scalar_starts_a_comment_and_a_hash_alone_does_not() {
    // Catches: a reader that refuses ` #` (GitHub reads it as a comment and runs the rest),
    // keeps the comment in the command, or drops a `#` that no space comes before. The
    // issue's `::notice::` (colons with no space after) is kept.
    for (step, command) in [
        ("run: echo a #b", "echo a"),
        ("run: echo a\t#b", "echo a"),
        ("run: echo a#b", "echo a#b"),
        ("run: echo \"::notice::off\"", "echo \"::notice::off\""),
        ("run: echo a\n  #b", "echo a"),
    ] {
        let (spoiled, _) = ci_with_step(step);
        assert_eq!(lint("ci.yml", &spoiled), Vec::<String>::new(), "{step:?}");
        let doc = yaml::load(&spoiled).expect("valid YAML");
        assert_eq!(
            scalar_starting(&doc, "echo a").or_else(|| scalar_starting(&doc, "echo \"::")),
            Some((command.to_owned(), true)),
            "{step:?}"
        );
    }
}

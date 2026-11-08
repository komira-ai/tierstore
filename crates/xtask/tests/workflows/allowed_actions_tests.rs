//! The Actions policy lint ([`super::allowed_actions`]): the workflows of the repository pass,
//! and an action the policy does not admit is named.

use super::allowed_actions::{PATTERNS_ALLOWED, action_problems, admitted};
use super::lint::lint;
use super::text_of;

#[test]
fn every_action_of_every_workflow_is_admitted_by_the_policy() {
    // Catches: a workflow that names an action the policy refuses, as release.yml did with
    // aws-actions/configure-aws-credentials, which made GitHub fail every run of it at start.
    let mut problems = Vec::new();
    for (file, text) in super::workflows() {
        for p in lint(&file, &text) {
            if p.contains("Actions policy") {
                problems.push(format!("{file}: {p}"));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn release_yml_names_no_action_outside_the_policy() {
    // Catches: the credentials step of channel-upload going back to an action of a third party.
    let text = text_of("release.yml");
    assert!(
        !text.contains("aws-actions/"),
        "release.yml names aws-actions/"
    );
}

#[test]
fn an_action_the_policy_does_not_admit_is_named_with_its_line() {
    // Catches: a lint that lets an action of a third party through.
    let problems = action_problems(&[
        (
            350,
            "aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd",
        ),
        (
            7,
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
        ),
    ]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with(
            "line 350: `uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd` is not admitted by the Actions policy"
        ),
        "{problems:?}"
    );
}

#[test]
fn a_workflow_text_that_names_a_refused_action_fails_the_lint() {
    // Catches: the rule being a function nothing calls.
    let text = "permissions: {}\non: push\njobs:\n  a:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n";
    let problems = lint("ci.yml", text);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("not admitted by the Actions policy")),
        "{problems:?}"
    );
}

#[test]
fn github_own_and_the_owners_actions_are_admitted_and_lookalikes_are_not() {
    // Catches: a prefix test that admits `actionsx/...` or an owner that only ends alike.
    for ok in [
        "actions/checkout@v7",
        "actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
        "github/codeql-action/init@abc",
        "komira-ai/hostkeeper/.github/workflows/set-desired.yml@1adad60c2a7c0136b9f46cb79a987883f2185445",
    ] {
        assert!(admitted(ok), "{ok}");
    }
    for refused in [
        "actionsx/checkout@v7",
        "evil-actions/checkout@v7",
        "komira-ai-evil/x@v1",
        "aws-actions/configure-aws-credentials@v6",
    ] {
        assert!(!admitted(refused), "{refused}");
    }
}

#[test]
fn each_pattern_of_the_setting_admits_what_it_says_and_nothing_near_it() {
    // Catches: a pattern mirrored wrong: a wildcard that admits another repository, a tag
    // pattern that admits another tag or a commit SHA.
    assert_eq!(PATTERNS_ALLOWED.len(), 5);
    let cases = [
        ("tailscale/github-action@v3", true),
        (
            "tailscale/github-action@6986d2c82a91fbac2949fe01f5bab95cf21b5102",
            true,
        ),
        ("tailscale/github-actions@v3", false),
        ("sigstore/cosign-installer@v3.9.0", true),
        ("sigstore/cosign-installer@", true),
        ("sigstore/cosign@v3", false),
        ("sebrollen/toml-action@v1.2.0", true),
        ("sebrollen/toml-action@v1.2.1", false),
        ("sebrollen/toml-action@v1.2.0x", false),
        ("dtolnay/rust-toolchain@v1", true),
        ("dtolnay/rust-toolchain@v1.2", false),
        (
            "dtolnay/rust-toolchain@1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d",
            false,
        ),
        ("swatinem/rust-cache@v2", true),
        ("swatinem/rust-cache@v3", false),
        ("Swatinem/rust-cache@v2", false),
    ];
    for (value, expected) in cases {
        assert_eq!(admitted(value), expected, "{value}");
    }
}

#[test]
fn a_local_path_is_left_to_the_pin_rule() {
    // Catches: one problem reported twice for the same line.
    assert!(action_problems(&[(3, "./.github/actions/x")]).is_empty());
}

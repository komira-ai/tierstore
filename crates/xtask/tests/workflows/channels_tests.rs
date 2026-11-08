//! The channel jobs of `release.yml` ([`super::channels`]): the file passes, and each way of
//! leaving a channel job without its gate, giving it more than it needs, handing the bot's token
//! or the role to a step that should not have it, or naming its secrets in another job is named.

use super::channels::{CHANNELS, channel_problems};
use super::lint::{lint, release_problems};
use super::platforms_tests::{each_named, in_job};
use super::text_of;
use super::yaml;

#[test]
fn the_channel_jobs_of_release_yml_pass_and_are_the_two_the_design_names() {
    // Catches: a channel job lost or renamed, which the defects below would not notice.
    let doc = yaml::load(&text_of("release.yml")).expect("release.yml parses");
    assert_eq!(channel_problems(&doc), Vec::<String>::new());
    let jobs: Vec<&str> = CHANNELS.iter().map(|c| c.job).collect();
    assert_eq!(jobs, ["channel-upload", "tap"]);
}

/// `(job, text to replace once in it, its replacement, what the problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "channel-upload",
        "needs: [plan, publish]",
        "needs: [plan]",
        "channel-upload: needs are not plan and publish",
    ),
    (
        "channel-upload",
        "    if: ${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' }}\n",
        "",
        "channel-upload: `if` is not",
    ),
    (
        "tap",
        "    if: ${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' }}\n",
        "    if: ${{ always() }}\n",
        "tap: `if` is not",
    ),
    (
        "channel-upload",
        "environment: prod-releases-tierstore",
        "environment: release",
        "channel-upload: environment is not prod-releases-tierstore",
    ),
    (
        "tap",
        "environment: release",
        "environment: prod-releases-tierstore",
        "tap: environment is not release",
    ),
    (
        "channel-upload",
        "      id-token: write\n",
        "",
        "channel-upload: permissions are [\"contents: read\"]",
    ),
    (
        "channel-upload",
        "      contents: read\n      id-token: write\n",
        "      contents: write\n      id-token: write\n",
        "channel-upload: permissions grant `contents: write`; only publish writes",
    ),
    (
        "tap",
        "      contents: read\n    env:",
        "      contents: read\n      id-token: write\n    env:",
        "tap: permissions grant `id-token: write`; only publish writes",
    ),
    (
        "tap",
        "      contents: read\n    env:",
        "      contents: write\n    env:",
        "tap: permissions are [\"contents: write\"], not [\"contents: read\"]",
    ),
    (
        "channel-upload",
        "id: gate\n",
        "id: check\n",
        "channel-upload: no step has the id `gate`",
    ),
    (
        "channel-upload",
        "run: cargo xtask channels gate --label channel-upload ",
        "run: cargo xtask channels gate --label tap ",
        "channel-upload: the gate does not run `cargo xtask channels gate --label channel-upload ...`",
    ),
    (
        "channel-upload",
        " --setting \"TIERSTORE_RELEASES_ROLE_ARN=${ROLE_ARN:+set}\"",
        "",
        "channel-upload: the gate is not given the setting TIERSTORE_RELEASES_ROLE_ARN",
    ),
    (
        "tap",
        " --setting \"RELEASE_BOT_PRIVATE_KEY=${PRIVATE_KEY:+set}\"",
        "",
        "tap: the gate is not given the setting RELEASE_BOT_PRIVATE_KEY",
    ),
    (
        "tap",
        "\"RELEASE_BOT_PRIVATE_KEY=${PRIVATE_KEY:+set}\"",
        "\"RELEASE_BOT_PRIVATE_KEY=${PRIVATE_KEY}\"",
        "tap: the gate must be given each setting as NAME=${VAR:+set}",
    ),
    (
        "tap",
        "\"RELEASE_BOT_PRIVATE_KEY=${PRIVATE_KEY:+set}\"",
        "\"RELEASE_BOT_PRIVATE_KEY=${{ secrets.RELEASE_BOT_PRIVATE_KEY }}\"",
        "tap: the gate's command line holds a value",
    ),
    (
        "channel-upload",
        "      - name: The files are exactly the assets of this commit\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n",
        "      - name: The files are exactly the assets of this commit\n",
        "channel-upload: the step `The files are exactly the assets of this commit` after the gate does not have `if: ${{ steps.gate.outputs.enabled == 'true' }}`",
    ),
    (
        "tap",
        "        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        env:\n          GH_TOKEN",
        "        if: ${{ always() }}\n        env:\n          GH_TOKEN",
        "tap: the step `The pull request that puts the formula of this release in the tap, with auto-merge when the tap allows it` after the gate does not have",
    ),
    (
        "channel-upload",
        "role-to-assume: ${{ vars.TIERSTORE_RELEASES_ROLE_ARN }}",
        "role-to-assume: ${{ vars.APT_PUBLISH_ROLE_ARN }}",
        "channel-upload: the role assumed is not ${{ vars.TIERSTORE_RELEASES_ROLE_ARN }}, the environment's",
    ),
    (
        "channel-upload",
        "      - name: Put the files of the version once each, then replace latest.json\n",
        "      - name: Put the files of the version once each, then replace latest.json\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        run: cargo xtask channels upload dist\n      - name: Again\n",
        "channel-upload: it needs one step that assumes the role and one that uploads",
    ),
    (
        "channel-upload",
        " --bucket \"$BUCKET\"",
        "",
        "channel-upload: the upload is not given --bucket \"$BUCKET\"",
    ),
    (
        "channel-upload",
        " --tag \"$TAG\"",
        "",
        "channel-upload: the upload is not given --tag \"$TAG\"",
    ),
    (
        "channel-upload",
        "run: cargo xtask release check dist --commit \"$COMMIT\"",
        "run: echo",
        "channel-upload: no step checks the downloaded files",
    ),
    (
        "channel-upload",
        "      - name: Gate on the settings of the release files",
        "      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n        with:\n          role-to-assume: ${{ vars.TIERSTORE_RELEASES_ROLE_ARN }}\n          aws-region: us-east-1\n      - name: Gate on the settings of the release files",
        "channel-upload: a step before the gate names aws-actions/",
    ),
    (
        "tap",
        "        run: cargo xtask --help\n      - name: Gate",
        "        run: cargo xtask --help\n        env:\n          K: ${{ secrets.RELEASE_BOT_PRIVATE_KEY }}\n      - name: Gate",
        "tap: a step before the gate names secrets.",
    ),
    (
        "tap",
        "          repositories: homebrew-tap\n",
        "          repositories: homebrew-tap,tierstore\n",
        "tap: the token is not for `repositories: homebrew-tap` alone",
    ),
    (
        "tap",
        "          repositories: homebrew-tap\n",
        "",
        "tap: the token is not for `repositories: homebrew-tap` alone",
    ),
    (
        "tap",
        "          permission-pull-requests: write\n",
        "          permission-pull-requests: write\n          permission-actions: write\n",
        "tap: the token's permissions are",
    ),
    (
        "tap",
        "          permission-contents: write\n",
        "",
        "tap: the token's permissions are",
    ),
    (
        "tap",
        "id: bot",
        "id: robot",
        "tap: the second to last step is not the token of the release bot (id `bot`)",
    ),
    (
        "tap",
        "      - name: Render Formula/tierstore.rb from the verified SHA256SUMS\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n",
        "      - name: Render Formula/tierstore.rb from the verified SHA256SUMS\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        env:\n          GH_TOKEN: ${{ steps.bot.outputs.token }}\n",
        "tap: the step `Render Formula/tierstore.rb from the verified SHA256SUMS` reads the bot's token; only the last step may",
    ),
    (
        "tap",
        "        env:\n          GH_TOKEN: ${{ steps.bot.outputs.token }}\n        run: cargo xtask channels tap",
        "        run: cargo xtask channels tap",
        "tap: the last step is not given the bot's token",
    ),
    (
        "tap",
        "run: cargo xtask channels tap ",
        "run: cargo xtask channels formula ",
        "tap: the last step does not run `cargo xtask channels tap`",
    ),
    (
        "verify",
        "      TAG: ${{ needs.plan.outputs.tag }}\n    outputs:",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      KEY: ${{ secrets.RELEASE_BOT_PRIVATE_KEY }}\n    outputs:",
        "verify: names RELEASE_BOT_PRIVATE_KEY, which only tap may",
    ),
    (
        "publish",
        "      TAG: ${{ needs.plan.outputs.tag }}\n    steps:",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      BUCKET: ${{ vars.TIERSTORE_RELEASES_BUCKET }}\n    steps:",
        "publish: names TIERSTORE_RELEASES_, which only channel-upload may",
    ),
    (
        "tap",
        "      - name: The token of the release bot, for the tap repository alone\n",
        "      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        with:\n          role-to-assume: x\n      - name: The token of the release bot, for the tap repository alone\n",
        "tap: names configure-aws-credentials, which only channel-upload may",
    ),
];

#[test]
fn each_defect_in_the_channel_jobs_is_named_by_the_lint() {
    // Catches: a pin of the channel jobs that holds nothing, because the file passes whether or
    // not the thing it pins is there.
    each_named("release.yml", release_problems, DEFECTS);
}

#[test]
fn a_job_that_is_missing_or_too_short_is_named() {
    // Catches: a channel job deleted, and a tap that has no mint step or no last step, which the
    // rules about the steps would otherwise skip.
    let doc = yaml::load("jobs: {}\n").expect("parses");
    assert_eq!(
        channel_problems(&doc),
        [
            "channel-upload: there is no such job",
            "tap: there is no such job"
        ]
    );
    let short = "jobs:
  channel-upload:
    steps:
      - id: gate
        run: echo
  tap:
    steps:
      - id: gate
        run: echo
      - run: echo
";
    let problems = channel_problems(&yaml::load(short).expect("parses"));
    assert!(
        problems.contains(
            &"channel-upload: it needs one step that assumes the role and one that uploads"
                .to_owned()
        ),
        "{problems:?}"
    );
    assert!(
        problems.contains(&"tap: it needs a mint step and a last step after the gate".to_owned()),
        "{problems:?}"
    );
}

#[test]
fn the_upload_assumes_the_role_before_it_uploads_and_pins_the_action() {
    // Catches: an upload that runs without credentials, and an action pinned by tag.
    let text = text_of("release.yml");
    let swapped = {
        let upload =
            "      - name: Put the files of the version once each, then replace latest.json\n";
        let role = "      - name: Assume the release-files role through OIDC\n";
        let tap = "\n  # The Homebrew formula of the release";
        let (before, rest) = text.split_once(role).expect("role step");
        let (role_step, rest) = rest.split_once(upload).expect("upload step");
        let (upload_step, after) = rest.split_once(tap).expect("the tap job");
        format!("{before}{upload}{upload_step}{role}{role_step}{tap}{after}")
    };
    let problems = release_problems(&swapped);
    assert!(
        problems
            .iter()
            .any(|p| p == "channel-upload: the role is assumed after the upload"),
        "{problems:?}"
    );
    let unpinned = in_job(
        &text,
        "channel-upload",
        "aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0",
        "aws-actions/configure-aws-credentials@v6",
    );
    assert!(
        lint("release.yml", &unpinned)
            .iter()
            .any(|p| p.contains("40-digit")),
        "{unpinned}"
    );
}

#[test]
fn only_channel_upload_holds_the_cloud_token_and_only_under_the_release_condition() {
    // Catches: `id-token: write` moving to another job of release.yml (the tap, which holds the
    // bot's key), and the upload holding it under a condition that lets a run without a plan
    // through.
    let text = text_of("release.yml");
    assert!(
        lint("release.yml", &text).is_empty(),
        "{:?}",
        lint("release.yml", &text)
    );
    for job in ["tap", "verify", "publish", "plan"] {
        let spoiled = in_job(
            &text,
            job,
            "    permissions:\n",
            "    permissions:\n      id-token: write\n",
        );
        assert!(
            lint("release.yml", &spoiled).iter().any(
                |p| p == &format!("job {job}: `id-token: write` is not allowed in release.yml")
            ),
            "{job}"
        );
    }
    let spoiled = in_job(
        &text,
        "channel-upload",
        "needs.plan.outputs.release == 'true' && ",
        "",
    );
    assert!(
        lint("release.yml", &spoiled)
            .iter()
            .any(|p| p.starts_with("job channel-upload: `id-token: write` needs the exact `if:")),
        "{:?}",
        lint("release.yml", &spoiled)
    );
    // Not in another file either.
    assert!(
        lint("ci.yml", &text)
            .iter()
            .any(|p| p == "job channel-upload: `id-token: write` is not allowed in ci.yml")
    );
}

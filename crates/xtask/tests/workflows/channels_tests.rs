//! The channel jobs of `release.yml` ([`super::channels`]): the file passes, and each way of
//! leaving a channel job without its gate, giving it more than it needs, handing the bot's token
//! or the role to a step that should not have it, or naming its secrets in another job is named.

use super::channels::{CHANNELS, channel_problems};
use super::lint::{lint, release_problems};
use super::platforms_tests::{each_named, in_job};
use super::text_of;
use super::yaml;

#[test]
fn the_channel_jobs_of_release_yml_pass_and_are_the_three_the_design_names() {
    // Catches: a channel job lost or renamed, which the defects below would not notice.
    let doc = yaml::load(&text_of("release.yml")).expect("release.yml parses");
    assert_eq!(channel_problems(&doc), Vec::<String>::new());
    let jobs: Vec<&str> = CHANNELS.iter().map(|c| c.job).collect();
    assert_eq!(jobs, ["testing-upload", "promote-latest", "tap"]);
}

/// `(job, text to replace once in it, its replacement, what the problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "testing-upload",
        "needs: [plan, publish]",
        "needs: [plan]",
        "testing-upload: needs are not",
    ),
    (
        "testing-upload",
        "    if: ${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' }}\n",
        "",
        "testing-upload: `if` is not",
    ),
    (
        "tap",
        "    if: ${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.promote-check.outputs.promote == 'true' }}\n",
        "    if: ${{ always() }}\n",
        "tap: `if` is not",
    ),
    (
        "testing-upload",
        "environment: prod-releases-tierstore",
        "environment: release",
        "testing-upload: environment is not prod-releases-tierstore",
    ),
    (
        "tap",
        "environment: release",
        "environment: prod-releases-tierstore",
        "tap: environment is not release",
    ),
    (
        "testing-upload",
        "      id-token: write\n",
        "",
        "testing-upload: permissions are [\"contents: read\"]",
    ),
    (
        "testing-upload",
        "      contents: read\n      id-token: write\n",
        "      contents: write\n      id-token: write\n",
        "testing-upload: permissions grant `contents: write`; only publish writes",
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
        "testing-upload",
        "id: gate\n",
        "id: check\n",
        "testing-upload: no step has the id `gate`",
    ),
    (
        "testing-upload",
        "run: cargo xtask channels gate --label testing-upload ",
        "run: cargo xtask channels gate --label tap ",
        "testing-upload: the gate does not run `cargo xtask channels gate --label testing-upload ...`",
    ),
    (
        "testing-upload",
        " --setting \"TIERSTORE_RELEASES_ROLE_ARN=${ROLE_ARN:+set}\"",
        "",
        "testing-upload: the gate is not given the setting TIERSTORE_RELEASES_ROLE_ARN",
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
        "testing-upload",
        "      - name: The files are exactly the assets of this commit\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n",
        "      - name: The files are exactly the assets of this commit\n",
        "testing-upload: the step `The files are exactly the assets of this commit` after the gate does not have `if: ${{ steps.gate.outputs.enabled == 'true' }}`",
    ),
    (
        "tap",
        "        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        env:\n          GH_TOKEN",
        "        if: ${{ always() }}\n        env:\n          GH_TOKEN",
        "tap: the step `The pull request that puts the formula of this release in the tap, with auto-merge when the tap allows it` after the gate does not have",
    ),
    (
        "testing-upload",
        "'true' }}\n        env:\n          ROLE_ARN: ${{ vars.TIERSTORE_RELEASES_ROLE_ARN }}",
        "'true' }}\n        env:\n          ROLE_ARN: ${{ vars.APT_PUBLISH_ROLE_ARN }}",
        "testing-upload: the role assumed is not ${{ vars.TIERSTORE_RELEASES_ROLE_ARN }}, the environment's",
    ),
    (
        "testing-upload",
        "      - name: Put the files of the version once each (latest.json is not touched)\n",
        "      - name: Put the files of the version once each (latest.json is not touched)\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        run: cargo xtask channels upload dist\n      - name: Again\n",
        "testing-upload: it needs one step that assumes the role and one that runs `cargo xtask channels upload`",
    ),
    (
        "testing-upload",
        " --bucket \"$BUCKET\"",
        "",
        "testing-upload: `cargo xtask channels upload` is not given --bucket \"$BUCKET\"",
    ),
    (
        "testing-upload",
        " --tag \"$TAG\"",
        "",
        "testing-upload: `cargo xtask channels upload` is not given --tag \"$TAG\"",
    ),
    (
        "testing-upload",
        "run: cargo xtask release check dist --commit \"$COMMIT\"",
        "run: echo",
        "testing-upload: no step checks the downloaded files",
    ),
    (
        "testing-upload",
        "      - name: Gate on the settings of the release files",
        "      - run: cargo xtask channels assume --role-arn x\n      - name: Gate on the settings of the release files",
        "testing-upload: a step before the gate names channels assume",
    ),
    (
        "testing-upload",
        " --role-arn \"$ROLE_ARN\"",
        "",
        "testing-upload: the assume step is not given --role-arn \"$ROLE_ARN\"",
    ),
    (
        "testing-upload",
        " --region us-east-1 --env-file",
        " --env-file",
        "testing-upload: the assume step is not given --region ",
    ),
    (
        "testing-upload",
        " --env-file \"$GITHUB_ENV\"",
        "",
        "testing-upload: the assume step is not given --env-file \"$GITHUB_ENV\"",
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
        "      - name: The formula that check-brew installed\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n",
        "      - name: The formula that check-brew installed\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        env:\n          GH_TOKEN: ${{ steps.bot.outputs.token }}\n",
        "tap: the step `The formula that check-brew installed` reads the bot's token; only the last step may",
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
        "publish: names TIERSTORE_RELEASES_, which only testing-upload and promote-latest may",
    ),
    (
        "testing-upload",
        " --no-latest",
        "",
        "testing-upload: the upload is not given --no-latest, so it would name an unchecked release the newest",
    ),
    (
        "promote-latest",
        "needs: [plan, promote-check, testing-upload]",
        "needs: [plan, testing-upload]",
        "promote-latest: needs are not",
    ),
    (
        "promote-latest",
        "&& needs.promote-check.outputs.promote == 'true' }}",
        "}}",
        "promote-latest: `if` is not",
    ),
    (
        "promote-latest",
        " --require \"UPLOADED=$UPLOADED\"",
        "",
        "promote-latest: the gate is not given --require \"UPLOADED=$UPLOADED\"",
    ),
    (
        "promote-latest",
        "cargo xtask channels latest dist",
        "cargo xtask channels upload dist",
        "promote-latest: it needs one step that assumes the role and one that runs `cargo xtask channels latest`",
    ),
    (
        "promote-latest",
        "cargo xtask channels latest dist",
        "cargo xtask channels latest dist --x; cargo xtask channels upload dist",
        "promote-latest: names channels upload, which only testing-upload may",
    ),
    (
        "testing-upload",
        "channels upload dist --no-latest",
        "channels latest dist; cargo xtask channels upload dist --no-latest",
        "testing-upload: names channels latest, which only promote-latest may",
    ),
    (
        "tap",
        "needs: [plan, promote-check, formula]",
        "needs: [plan, formula]",
        "tap: needs are not",
    ),
    (
        "tap",
        "&& needs.promote-check.outputs.promote == 'true' }}",
        "}}",
        "tap: `if` is not",
    ),
    (
        "tap",
        "      - name: The token of the release bot, for the tap repository alone\n",
        "      - run: cargo xtask channels assume --role-arn x\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n      - name: The token of the release bot, for the tap repository alone\n",
        "tap: names channels assume, which only testing-upload and promote-latest may",
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
            "testing-upload: there is no such job",
            "promote-latest: there is no such job",
            "tap: there is no such job"
        ]
    );
    let short = "jobs:
  testing-upload:
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
            &"testing-upload: it needs one step that assumes the role and one that runs `cargo xtask channels upload`"
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
fn the_upload_assumes_the_role_before_it_uploads() {
    // Catches: an upload that runs without credentials.
    let text = text_of("release.yml");
    let swapped = {
        let upload =
            "      - name: Put the files of the version once each (latest.json is not touched)\n";
        let role = "      - name: Assume the release-files role through OIDC\n";
        let tap = "\n  # TESTING, the apt repository";
        let (before, rest) = text.split_once(role).expect("role step");
        let (role_step, rest) = rest.split_once(upload).expect("upload step");
        let (upload_step, after) = rest.split_once(tap).expect("the tap job");
        format!("{before}{upload}{upload_step}{role}{role_step}{tap}{after}")
    };
    let problems = release_problems(&swapped);
    assert!(
        problems
            .iter()
            .any(|p| p
                == "testing-upload: the role is assumed after `cargo xtask channels upload` runs"),
        "{problems:?}"
    );
}

#[test]
fn the_upload_job_cannot_go_back_to_an_action_for_its_credentials() {
    // Catches: aws-actions/configure-aws-credentials returning to testing-upload, which the
    // Actions policy of the repository refuses (GitHub then fails every run of the workflow).
    let text = text_of("release.yml");
    let back = in_job(
        &text,
        "testing-upload",
        "        run: cargo xtask channels assume --role-arn \"$ROLE_ARN\" --session-name \"tierstore-release-$GITHUB_RUN_ID\" --region us-east-1 --env-file \"$GITHUB_ENV\"\n",
        "        run: echo\n      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n",
    );
    assert!(
        lint("release.yml", &back)
            .iter()
            .any(|p| p.contains("not admitted by the Actions policy")),
        "{:?}",
        lint("release.yml", &back)
    );
    assert!(
        release_problems(&back)
            .iter()
            .any(|p| p
                == "testing-upload: it needs one step that assumes the role and one that runs `cargo xtask channels upload`"),
        "{:?}",
        release_problems(&back)
    );
}

#[test]
fn only_the_upload_jobs_hold_the_cloud_token_and_only_under_the_release_condition() {
    // Catches: `id-token: write` moving to another job of release.yml (the tap, which holds the
    // bot's key), and the upload holding it under a condition that lets a run without a plan
    // through.
    let text = text_of("release.yml");
    assert!(
        lint("release.yml", &text).is_empty(),
        "{:?}",
        lint("release.yml", &text)
    );
    for job in [
        "tap",
        "verify",
        "publish",
        "plan",
        "promote-check",
        "formula",
    ] {
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
        "testing-upload",
        "needs.plan.outputs.release == 'true' && ",
        "",
    );
    assert!(
        lint("release.yml", &spoiled)
            .iter()
            .any(|p| p.starts_with("job testing-upload: `id-token: write` needs the exact `if:")),
        "{:?}",
        lint("release.yml", &spoiled)
    );
    // Not in another file either.
    assert!(
        lint("ci.yml", &text)
            .iter()
            .any(|p| p == "job testing-upload: `id-token: write` is not allowed in ci.yml")
    );
}

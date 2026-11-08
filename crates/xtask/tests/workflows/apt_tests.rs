//! The apt jobs of `release.yml` ([`super::apt`]): the file passes, and each way of giving a job
//! both the key and the role, leaving a job without its gate, handing the role to the wrong job,
//! or running a verb in a job of another kind is named.

use super::apt::{JOBS, apt_problems};
use super::lint::{lint, release_problems};
use super::platforms_tests::{each_named, in_job};
use super::text_of;
use super::yaml;

const ROLE_STEP: &str = "        uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n        with:\n          role-to-assume: ${{ vars.APT_PUBLISH_ROLE_ARN }}\n          aws-region: us-east-1\n          mask-aws-account-id: true\n";

#[test]
fn the_apt_jobs_of_release_yml_pass_and_are_the_eighteen_the_design_names() {
    // Catches: an apt job lost or renamed, which the defects below would not notice.
    let doc = yaml::load(&text_of("release.yml")).expect("release.yml parses");
    assert_eq!(apt_problems(&doc), Vec::<String>::new());
    let jobs: Vec<&str> = JOBS.iter().map(|j| j.name).collect();
    assert_eq!(
        jobs,
        [
            "testing-apt-read",
            "testing-apt-index",
            "testing-apt-write",
            "testing-apt-read-2",
            "testing-apt-index-2",
            "testing-apt-write-2",
            "testing-apt-read-3",
            "testing-apt-index-3",
            "testing-apt-write-3",
            "stable-apt-read",
            "stable-apt-index",
            "stable-apt-write",
            "stable-apt-read-2",
            "stable-apt-index-2",
            "stable-apt-write-2",
            "stable-apt-read-3",
            "stable-apt-index-3",
            "stable-apt-write-3"
        ]
    );
    assert_eq!(
        release_problems(&text_of("release.yml")),
        Vec::<String>::new()
    );
}

/// `(job, text to replace once in it, its replacement, what the problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "testing-apt-read",
        "needs: [plan, publish]",
        "needs: [plan]",
        "testing-apt-read: needs are not",
    ),
    (
        "testing-apt-read-2",
        "needs.testing-apt-write.outputs.conflict == 'true' ",
        "",
        "testing-apt-read-2: `if` is not",
    ),
    (
        "testing-apt-index",
        "    if: ${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' }}\n",
        "",
        "testing-apt-index: `if` is not",
    ),
    (
        "testing-apt-read",
        "environment: apt-publish",
        "environment: apt-signing",
        "testing-apt-read: environment is not apt-publish",
    ),
    (
        "testing-apt-index",
        "environment: apt-signing",
        "environment: apt-publish",
        "testing-apt-index: environment is not apt-signing",
    ),
    (
        "testing-apt-read",
        "      id-token: write\n",
        "",
        "testing-apt-read: permissions are [\"contents: read\"]",
    ),
    (
        "testing-apt-index",
        "    permissions:\n      contents: read\n",
        "    permissions:\n      contents: read\n      id-token: write\n",
        "testing-apt-index: permissions are [\"contents: read\", \"id-token: write\"], not [\"contents: read\"]",
    ),
    (
        "testing-apt-write",
        "      contents: read\n      id-token: write\n",
        "      contents: write\n      id-token: write\n",
        "testing-apt-write: permissions grant `contents: write`; only publish writes",
    ),
    (
        "testing-apt-read",
        "id: gate\n",
        "id: check\n",
        "testing-apt-read: no step has the id `gate`",
    ),
    (
        "testing-apt-read",
        "cargo xtask channels gate --label testing-apt-read ",
        "cargo xtask channels gate --label apt ",
        "the gate does not run `cargo xtask channels gate --label testing-apt-read ...`",
    ),
    (
        "testing-apt-read",
        " --setting \"APT_HOSTNAME=${APT_HOSTNAME:+set}\"",
        "",
        "testing-apt-read: the gate is not given the setting APT_HOSTNAME",
    ),
    (
        "testing-apt-read",
        "--setting \"APT_BUCKET=${APT_BUCKET:+set}\"",
        "--setting \"APT_BUCKET=${{ vars.APT_BUCKET }}\"",
        "testing-apt-read: the gate's command line holds a value",
    ),
    (
        "testing-apt-index",
        " --setting \"APT_GPG_PASSPHRASE=${APT_GPG_PASSPHRASE:+set}\"",
        " --setting \"APT_GPG_PASSPHRASE=${APT_GPG_PASSPHRASE:+set}\" --setting \"APT_BUCKET=x\"",
        "testing-apt-index: the gate is given a setting it should not be",
    ),
    (
        "testing-apt-index",
        " --require \"READ_ENABLED=$READ_ENABLED\"",
        "",
        "testing-apt-index: the gate is not given --require \"READ_ENABLED=$READ_ENABLED\"",
    ),
    (
        "testing-apt-write",
        " --require \"INDEX_SIGNED=$INDEX_SIGNED\"",
        "",
        "testing-apt-write: the gate is not given --require \"INDEX_SIGNED=$INDEX_SIGNED\"",
    ),
    (
        "testing-apt-read",
        "--setting \"APT_HOSTNAME",
        "--require \"X=$X\" --setting \"APT_HOSTNAME",
        "testing-apt-read: the gate is given a requirement it should not be",
    ),
    (
        "testing-apt-read",
        "      - name: Assume the apt publish role through OIDC\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n",
        "      - name: Assume the apt publish role through OIDC\n",
        "testing-apt-read: the step `Assume the apt publish role through OIDC",
    ),
    (
        "testing-apt-read",
        "      - name: Gate on the settings",
        "      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n      - name: Gate on the settings",
        "testing-apt-read: a step before the gate names aws-actions/",
    ),
    (
        "testing-apt-write",
        "role-to-assume: ${{ vars.APT_PUBLISH_ROLE_ARN }}",
        "role-to-assume: ${{ vars.TIERSTORE_RELEASES_ROLE_ARN }}",
        "testing-apt-write: the role assumed is not ${{ vars.APT_PUBLISH_ROLE_ARN }}",
    ),
    (
        "testing-apt-write",
        ROLE_STEP,
        "        run: echo\n",
        "testing-apt-write: it needs one step that assumes the role and one that runs `cargo xtask channels apt write...`",
    ),
    (
        "testing-apt-write-3",
        " --last",
        "",
        "testing-apt-write-3: `--last` must be passed by the last attempt's write and by no other",
    ),
    (
        "testing-apt-write",
        "\"${TAG#v}\" --output",
        "\"${TAG#v}\" --last --output",
        "testing-apt-write: `--last` must be passed by the last attempt's write and by no other",
    ),
    (
        "testing-apt-write",
        "cargo xtask release check dist",
        "echo check dist",
        "testing-apt-write: no step checks the downloaded files",
    ),
    (
        "testing-apt-index",
        "cargo xtask release check dist",
        "echo check dist",
        "testing-apt-index: no step checks the downloaded files",
    ),
    (
        "testing-apt-read",
        "      TAG: ${{ needs.plan.outputs.tag }}\n    steps:",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n    steps:",
        "testing-apt-read: names APT_GPG_, the signing key, which only the apt and rpm index jobs may",
    ),
    (
        "tap",
        "      TAG: ${{ needs.plan.outputs.tag }}\n    steps:",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      ROLE: ${{ vars.APT_PUBLISH_ROLE_ARN }}\n    steps:",
        "tap: names APT_PUBLISH_ROLE_ARN, which only the apt and rpm jobs that read and write may",
    ),
    (
        "testing-apt-index",
        "      - name: Merge the .debs into the index",
        "      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n      - name: Merge the .debs into the index",
        "testing-apt-index: names aws-actions/; this job holds the key, not the role",
    ),
    (
        "testing-apt-index",
        "      COMMIT: ${{ needs.plan.outputs.commit }}\n",
        "      COMMIT: ${{ needs.plan.outputs.commit }}\n      AWS_REGION: us-east-1\n",
        "testing-apt-index: names AWS_; this job holds the key, not the role",
    ),
    (
        "testing-apt-index",
        "cargo xtask channels apt sign ",
        "echo ",
        "testing-apt-index: it needs a step that merges (`cargo xtask channels apt index...`) and then one that signs",
    ),
    (
        "testing-apt-index",
        "        id: sign\n",
        "",
        "testing-apt-index: the signing step does not have the id `sign`",
    ),
    (
        "testing-apt-index",
        "          APT_GPG_PASSPHRASE: ${{ secrets.APT_GPG_PASSPHRASE }}\n        run: |",
        "        run: |",
        "testing-apt-index: the signing step does not read secrets.APT_GPG_PASSPHRASE",
    ),
    (
        "testing-apt-index",
        "        env:\n          OWNER: ${{ github.repository_owner }}\n",
        "        env:\n          OWNER: ${{ github.repository_owner }}\n          KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n",
        "testing-apt-index: the step `Merge the .debs into the index that was read (the entries of every other product are kept)` names a secret; only the gate and the signing step may",
    ),
    (
        "testing-apt-write",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          APT_DISTRIBUTION_ID: ${{ vars.APT_DISTRIBUTION_ID }}\n          APT_HOSTNAME",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          X: ${{ secrets.ANYTHING }}\n          APT_DISTRIBUTION_ID: ${{ vars.APT_DISTRIBUTION_ID }}\n          APT_HOSTNAME",
        "this job holds the role, not a key",
    ),
    (
        "testing-apt-read",
        "cargo xtask channels apt read --suite testing --bucket",
        "cargo xtask channels apt sign --bucket",
        "testing-apt-read: runs `cargo xtask channels apt sign...`, which only the apt index jobs may",
    ),
    (
        "testing-apt-index",
        "cargo xtask channels apt index --suite testing --current",
        "cargo xtask channels apt write --suite testing --current",
        "testing-apt-index: runs `cargo xtask channels apt write...`, which only the Write jobs of the apt repository may",
    ),
    (
        "testing-apt-index",
        "cargo xtask channels apt index --suite testing --current",
        "cargo xtask channels apt read --current",
        "testing-apt-index: runs `cargo xtask channels apt read...`, which only the Read jobs of the apt repository may",
    ),
    (
        "testing-apt-read",
        "channels apt read --suite testing --bucket",
        "channels apt read --suite stable --bucket",
        "testing-apt-read: `cargo xtask channels apt read...` is not given `--suite testing`",
    ),
    (
        "testing-apt-index",
        "channels apt index --suite testing --current",
        "channels apt index --current",
        "testing-apt-index: `cargo xtask channels apt index...` is not given `--suite testing`",
    ),
    (
        "testing-apt-index",
        "--dir \"$RUNNER_TEMP/apt-next/dists/testing\" ",
        "--dir \"$RUNNER_TEMP/apt-next/dists/stable\" ",
        "testing-apt-index: the index is not signed in dists/testing, the suite the job writes",
    ),
    (
        "testing-apt-write",
        "channels apt write --suite testing --bucket",
        "channels apt write --suite stable --bucket",
        "testing-apt-write: `cargo xtask channels apt write...` is not given `--suite testing`",
    ),
    (
        "testing-apt-write",
        "channels apt write --suite testing --bucket",
        "channels apt write --suite testing --no-pool --bucket",
        "testing-apt-write: passes --no-pool; the release's write puts the packages in the pool",
    ),
    // the promotion into stable
    (
        "stable-apt-index",
        "needs: [plan, promote-check, stable-apt-read, testing-apt-index]",
        "needs: [plan, stable-apt-read, testing-apt-index]",
        "stable-apt-index: needs are not",
    ),
    (
        "stable-apt-write",
        "&& needs.promote-check.outputs.promote == 'true' }}",
        "}}",
        "stable-apt-write: `if` is not",
    ),
    (
        "stable-apt-read",
        "needs: [plan, promote-check, testing-apt-read]",
        "needs: [plan, testing-apt-read]",
        "stable-apt-read: needs are not",
    ),
    (
        "stable-apt-read",
        " --require \"TESTING_ENABLED=$TESTING_ENABLED\"",
        "",
        "stable-apt-read: the gate is not given --require \"TESTING_ENABLED=$TESTING_ENABLED\"",
    ),
    (
        "stable-apt-read",
        "channels apt read --suite testing --bucket",
        "channels apt read --bucket",
        "stable-apt-read: `cargo xtask channels apt read...` is not given `--suite testing`: the promotion reads both",
    ),
    (
        "stable-apt-read",
        "channels apt read --suite stable --bucket",
        "channels apt read --suite testing --bucket",
        "stable-apt-read: `cargo xtask channels apt read...` is not given `--suite stable`: the promotion reads both",
    ),
    (
        "stable-apt-index",
        "--sums dist/SHA256SUMS ",
        "",
        "stable-apt-index: `cargo xtask channels apt promote...` is not given --sums dist/SHA256SUMS",
    ),
    (
        "stable-apt-index",
        "--testing \"$RUNNER_TEMP/apt-read/testing\" ",
        "--testing \"$RUNNER_TEMP/apt-read/stable\" ",
        "stable-apt-index: `cargo xtask channels apt promote...` is not given --testing",
    ),
    (
        "stable-apt-index",
        "channels apt promote ",
        "channels apt index --suite stable ",
        "stable-apt-index: runs `cargo xtask channels apt index...`, which only the Index jobs of the suite testing of the apt repository may",
    ),
    (
        "testing-apt-index",
        "channels apt index --suite testing ",
        "channels apt promote ",
        "testing-apt-index: runs `cargo xtask channels apt promote...`, which only the Index jobs of the suite stable of the apt repository may",
    ),
    (
        "stable-apt-index",
        "--dir \"$RUNNER_TEMP/apt-next/dists/stable\" ",
        "--dir \"$RUNNER_TEMP/apt-next/dists/testing\" ",
        "stable-apt-index: the index is not signed in dists/stable, the suite the job writes",
    ),
    (
        "stable-apt-write",
        "--suite stable --bucket",
        "--suite testing --bucket",
        "stable-apt-write: `cargo xtask channels apt write...` is not given `--suite stable`",
    ),
    (
        "stable-apt-write",
        " --no-pool ",
        " ",
        "stable-apt-write: does not pass --no-pool; the promotion writes the index only",
    ),
    (
        "stable-apt-write",
        " --no-pool ",
        " --no-pool --debs dist ",
        "stable-apt-write: passes --debs; the promotion uploads no package",
    ),
    (
        "testing-apt-read",
        "channels apt read --suite testing --bucket",
        "channels apt read --suite testing --suite stable --bucket",
        "testing-apt-read: names `--suite stable`; this job is for the suite testing",
    ),
    (
        "stable-apt-write",
        "role-to-assume: ${{ vars.APT_PUBLISH_ROLE_ARN }}",
        "role-to-assume: ${{ vars.TIERSTORE_RELEASES_ROLE_ARN }}",
        "stable-apt-write: the role assumed is not ${{ vars.APT_PUBLISH_ROLE_ARN }}",
    ),
    (
        "stable-apt-write-3",
        " --last",
        "",
        "stable-apt-write-3: `--last` must be passed by the last attempt's write and by no other",
    ),
    (
        "stable-apt-index",
        "cargo xtask release check dist",
        "echo check dist",
        "stable-apt-index: no step checks the downloaded files",
    ),
    (
        "tap",
        "cargo xtask channels tap ",
        "cargo xtask channels apt index ",
        "tap: runs `cargo xtask channels apt index...`, which only the Index jobs of the suite testing of the apt repository may",
    ),
];

#[test]
fn each_defect_in_the_apt_jobs_is_named_by_the_lint() {
    // Catches: a pin of the apt jobs that holds nothing, because the file passes whether or not
    // the thing it pins is there.
    each_named("release.yml", release_problems, DEFECTS);
}

#[test]
fn a_job_that_is_missing_or_too_short_is_named() {
    // Catches: an apt job deleted, and one with a gate but no merge, no signature or no role,
    // which the rules about the steps would otherwise skip.
    let doc = yaml::load("jobs: {}\n").expect("parses");
    let problems = apt_problems(&doc);
    assert_eq!(problems.len(), 18);
    assert!(
        problems
            .iter()
            .all(|p| p.ends_with(": there is no such job"))
    );
    let short = "jobs:
  testing-apt-read:
    steps:
      - id: gate
        run: echo
  testing-apt-index:
    steps:
      - id: gate
        run: echo
";
    let problems = apt_problems(&yaml::load(short).expect("parses"));
    assert!(
        problems.contains(
            &"testing-apt-read: it needs one step that assumes the role and one that runs `cargo xtask channels apt read...`"
                .to_owned()
        ),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.starts_with(
            "testing-apt-index: it needs a step that merges (`cargo xtask channels apt index...`) and then one that signs"
        )),
        "{problems:?}"
    );
}

#[test]
fn a_job_renamed_or_added_is_named_by_the_list_of_jobs() {
    // Catches: a fourth attempt, or an apt job the lint does not hold.
    let text = text_of("release.yml");
    let renamed = text.replace("\n  testing-apt-index-3:\n", "\n  testing-apt-index-4:\n");
    let problems = release_problems(&renamed);
    assert!(
        problems.iter().any(|p| p.starts_with("jobs are")),
        "{problems:?}"
    );
    assert!(
        problems.contains(&"testing-apt-index-3: there is no such job".to_owned()),
        "{problems:?}"
    );
}

#[test]
fn the_role_is_assumed_before_the_index_is_read_and_before_it_is_written() {
    // Catches: a read or write that runs without credentials, and an action pinned by tag.
    let text = text_of("release.yml");
    for (job, verb) in [
        ("testing-apt-read", "      - name: Read the current index"),
        (
            "testing-apt-write",
            "      - name: Upload the .debs once each",
        ),
    ] {
        let role = "      - name: Assume the apt publish role through OIDC\n";
        let body_start = text.find(&format!("\n  {job}:\n")).expect("job");
        let (head, rest) = text.split_at(body_start);
        let (role_at, verb_at) = (
            rest.find(role).expect("role"),
            rest.find(verb).expect("verb"),
        );
        assert!(role_at < verb_at);
        let after_verb = rest[verb_at..]
            .find("\n  testing-apt-")
            .or_else(|| rest[verb_at..].find("\n\n  # "))
            .map_or(rest.len(), |n| verb_at + n);
        let swapped = format!(
            "{head}{}{}{}{}",
            &rest[..role_at],
            &rest[verb_at..after_verb],
            &rest[role_at..verb_at],
            &rest[after_verb..]
        );
        let problems = release_problems(&swapped);
        assert!(
            problems
                .iter()
                .any(|p| p == &format!("{job}: the role is assumed after the verb runs")),
            "{job}: {problems:?}"
        );
    }
    let unpinned = in_job(
        &text,
        "testing-apt-write-2",
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
fn only_the_apt_jobs_that_read_and_write_hold_the_cloud_token_and_the_index_job_never_does() {
    // Catches: `id-token: write` moving to the job that holds the signing key, and a job of the
    // apt repository holding it under a condition that lets a run without a plan through.
    let text = text_of("release.yml");
    for job in [
        "testing-apt-index",
        "testing-apt-index-2",
        "testing-apt-index-3",
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
    for job in [
        "testing-apt-read",
        "testing-apt-write",
        "testing-apt-read-2",
        "testing-apt-write-2",
        "testing-apt-read-3",
        "testing-apt-write-3",
    ] {
        let spoiled = in_job(&text, job, "needs.plan.outputs.release == 'true' && ", "");
        assert!(
            lint("release.yml", &spoiled)
                .iter()
                .any(|p| p.starts_with(&format!(
                    "job {job}: `id-token: write` needs the exact `if:"
                ))),
            "{job}"
        );
    }
}

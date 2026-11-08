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
fn the_apt_jobs_of_release_yml_pass_and_are_the_nine_the_design_names() {
    // Catches: an apt job lost or renamed, which the defects below would not notice.
    let doc = yaml::load(&text_of("release.yml")).expect("release.yml parses");
    assert_eq!(apt_problems(&doc), Vec::<String>::new());
    let jobs: Vec<&str> = JOBS.iter().map(|j| j.name).collect();
    assert_eq!(
        jobs,
        [
            "apt-deploy-read",
            "apt-index",
            "apt-deploy-write",
            "apt-deploy-read-2",
            "apt-index-2",
            "apt-deploy-write-2",
            "apt-deploy-read-3",
            "apt-index-3",
            "apt-deploy-write-3"
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
        "apt-deploy-read",
        "needs: [plan, soak]",
        "needs: [plan]",
        "apt-deploy-read: needs are not",
    ),
    (
        "apt-deploy-read-2",
        "needs.apt-deploy-write.outputs.conflict == 'true' ",
        "",
        "apt-deploy-read-2: `if` is not",
    ),
    (
        "apt-index",
        "    if: ${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' }}\n",
        "",
        "apt-index: `if` is not",
    ),
    (
        "apt-deploy-read",
        "environment: apt-publish",
        "environment: apt-signing",
        "apt-deploy-read: environment is not apt-publish",
    ),
    (
        "apt-index",
        "environment: apt-signing",
        "environment: apt-publish",
        "apt-index: environment is not apt-signing",
    ),
    (
        "apt-deploy-read",
        "      id-token: write\n",
        "",
        "apt-deploy-read: permissions are [\"contents: read\"]",
    ),
    (
        "apt-index",
        "    permissions:\n      contents: read\n",
        "    permissions:\n      contents: read\n      id-token: write\n",
        "apt-index: permissions are [\"contents: read\", \"id-token: write\"], not [\"contents: read\"]",
    ),
    (
        "apt-deploy-write",
        "      contents: read\n      id-token: write\n",
        "      contents: write\n      id-token: write\n",
        "apt-deploy-write: permissions grant `contents: write`; only publish writes",
    ),
    (
        "apt-deploy-read",
        "id: gate\n",
        "id: check\n",
        "apt-deploy-read: no step has the id `gate`",
    ),
    (
        "apt-deploy-read",
        "cargo xtask channels gate --label apt-deploy-read ",
        "cargo xtask channels gate --label apt ",
        "the gate does not run `cargo xtask channels gate --label apt-deploy-read ...`",
    ),
    (
        "apt-deploy-read",
        " --setting \"APT_HOSTNAME=${APT_HOSTNAME:+set}\"",
        "",
        "apt-deploy-read: the gate is not given the setting APT_HOSTNAME",
    ),
    (
        "apt-deploy-read",
        "--setting \"APT_BUCKET=${APT_BUCKET:+set}\"",
        "--setting \"APT_BUCKET=${{ vars.APT_BUCKET }}\"",
        "apt-deploy-read: the gate's command line holds a value",
    ),
    (
        "apt-index",
        " --setting \"APT_GPG_PASSPHRASE=${APT_GPG_PASSPHRASE:+set}\"",
        " --setting \"APT_GPG_PASSPHRASE=${APT_GPG_PASSPHRASE:+set}\" --setting \"APT_BUCKET=x\"",
        "apt-index: the gate is given a setting it should not be",
    ),
    (
        "apt-index",
        " --require \"READ_ENABLED=$READ_ENABLED\"",
        "",
        "apt-index: the gate is not given --require \"READ_ENABLED=$READ_ENABLED\"",
    ),
    (
        "apt-deploy-write",
        " --require \"INDEX_SIGNED=$INDEX_SIGNED\"",
        "",
        "apt-deploy-write: the gate is not given --require \"INDEX_SIGNED=$INDEX_SIGNED\"",
    ),
    (
        "apt-deploy-read",
        "--setting \"APT_HOSTNAME",
        "--require \"X=$X\" --setting \"APT_HOSTNAME",
        "apt-deploy-read: the gate is given a requirement it should not be",
    ),
    (
        "apt-deploy-read",
        "      - name: Assume the apt publish role through OIDC\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n",
        "      - name: Assume the apt publish role through OIDC\n",
        "apt-deploy-read: the step `Assume the apt publish role through OIDC",
    ),
    (
        "apt-deploy-read",
        "      - name: Gate on the settings",
        "      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n      - name: Gate on the settings",
        "apt-deploy-read: a step before the gate names aws-actions/",
    ),
    (
        "apt-deploy-write",
        "role-to-assume: ${{ vars.APT_PUBLISH_ROLE_ARN }}",
        "role-to-assume: ${{ vars.TIERSTORE_RELEASES_ROLE_ARN }}",
        "apt-deploy-write: the role assumed is not ${{ vars.APT_PUBLISH_ROLE_ARN }}",
    ),
    (
        "apt-deploy-write",
        ROLE_STEP,
        "        run: echo\n",
        "apt-deploy-write: it needs one step that assumes the role and one that runs `cargo xtask channels apt write...`",
    ),
    (
        "apt-deploy-write-3",
        " --last",
        "",
        "apt-deploy-write-3: `--last` must be passed by the last attempt's write and by no other",
    ),
    (
        "apt-deploy-write",
        "\"${TAG#v}\" --output",
        "\"${TAG#v}\" --last --output",
        "apt-deploy-write: `--last` must be passed by the last attempt's write and by no other",
    ),
    (
        "apt-deploy-write",
        "cargo xtask release check dist",
        "echo check dist",
        "apt-deploy-write: no step checks the downloaded files",
    ),
    (
        "apt-index",
        "cargo xtask release check dist",
        "echo check dist",
        "apt-index: no step checks the downloaded files",
    ),
    (
        "apt-deploy-read",
        "      TAG: ${{ needs.plan.outputs.tag }}\n    steps:",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n    steps:",
        "apt-deploy-read: names APT_GPG_, the signing key, which only the apt index jobs may",
    ),
    (
        "tap",
        "      TAG: ${{ needs.plan.outputs.tag }}\n    steps:",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      ROLE: ${{ vars.APT_PUBLISH_ROLE_ARN }}\n    steps:",
        "tap: names APT_PUBLISH_ROLE_ARN, which only the apt jobs that read and write may",
    ),
    (
        "apt-index",
        "      - name: Merge the .debs into the index",
        "      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n      - name: Merge the .debs into the index",
        "apt-index: names aws-actions/; this job holds the key, not the role",
    ),
    (
        "apt-index",
        "      COMMIT: ${{ needs.plan.outputs.commit }}\n",
        "      COMMIT: ${{ needs.plan.outputs.commit }}\n      AWS_REGION: us-east-1\n",
        "apt-index: names AWS_; this job holds the key, not the role",
    ),
    (
        "apt-index",
        "cargo xtask channels apt sign ",
        "echo ",
        "apt-index: it needs a step that merges (`cargo xtask channels apt index...`) and then one that signs",
    ),
    (
        "apt-index",
        "        id: sign\n",
        "",
        "apt-index: the signing step does not have the id `sign`",
    ),
    (
        "apt-index",
        "          APT_GPG_PASSPHRASE: ${{ secrets.APT_GPG_PASSPHRASE }}\n        run: |",
        "        run: |",
        "apt-index: the signing step does not read secrets.APT_GPG_PASSPHRASE",
    ),
    (
        "apt-index",
        "        env:\n          OWNER: ${{ github.repository_owner }}\n",
        "        env:\n          OWNER: ${{ github.repository_owner }}\n          KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n",
        "apt-index: the step `Merge the .debs into the index that was read (the entries of every other product are kept)` names a secret; only the gate and the signing step may",
    ),
    (
        "apt-deploy-write",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          APT_DISTRIBUTION_ID: ${{ vars.APT_DISTRIBUTION_ID }}\n          APT_HOSTNAME",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          X: ${{ secrets.ANYTHING }}\n          APT_DISTRIBUTION_ID: ${{ vars.APT_DISTRIBUTION_ID }}\n          APT_HOSTNAME",
        "this job holds the role, not a key",
    ),
    (
        "apt-deploy-read",
        "cargo xtask channels apt read --bucket",
        "cargo xtask channels apt sign --bucket",
        "apt-deploy-read: runs `cargo xtask channels apt sign...`, which only the apt index jobs may",
    ),
    (
        "apt-index",
        "cargo xtask channels apt index --current",
        "cargo xtask channels apt write --current",
        "apt-index: runs `cargo xtask channels apt write...`, which only the Write jobs of the apt repository may",
    ),
    (
        "apt-index",
        "cargo xtask channels apt index --current",
        "cargo xtask channels apt read --current",
        "apt-index: runs `cargo xtask channels apt read...`, which only the Read jobs of the apt repository may",
    ),
    (
        "tap",
        "cargo xtask channels tap ",
        "cargo xtask channels apt index ",
        "tap: runs `cargo xtask channels apt index...`, which only the Index jobs of the apt repository may",
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
    assert_eq!(problems.len(), 9);
    assert!(
        problems
            .iter()
            .all(|p| p.ends_with(": there is no such job"))
    );
    let short = "jobs:
  apt-deploy-read:
    steps:
      - id: gate
        run: echo
  apt-index:
    steps:
      - id: gate
        run: echo
";
    let problems = apt_problems(&yaml::load(short).expect("parses"));
    assert!(
        problems.contains(
            &"apt-deploy-read: it needs one step that assumes the role and one that runs `cargo xtask channels apt read...`"
                .to_owned()
        ),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.starts_with(
            "apt-index: it needs a step that merges (`cargo xtask channels apt index...`) and then one that signs"
        )),
        "{problems:?}"
    );
}

#[test]
fn a_job_renamed_or_added_is_named_by_the_list_of_jobs() {
    // Catches: a fourth attempt, or an apt job the lint does not hold.
    let text = text_of("release.yml");
    let renamed = text.replace("\n  apt-index-3:\n", "\n  apt-index-4:\n");
    let problems = release_problems(&renamed);
    assert!(
        problems.iter().any(|p| p.starts_with("jobs are")),
        "{problems:?}"
    );
    assert!(
        problems.contains(&"apt-index-3: there is no such job".to_owned()),
        "{problems:?}"
    );
}

#[test]
fn the_role_is_assumed_before_the_index_is_read_and_before_it_is_written() {
    // Catches: a read or write that runs without credentials, and an action pinned by tag.
    let text = text_of("release.yml");
    for (job, verb) in [
        ("apt-deploy-read", "      - name: Read the current index"),
        (
            "apt-deploy-write",
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
            .find("\n  apt-")
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
        "apt-deploy-write-2",
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
    for job in ["apt-index", "apt-index-2", "apt-index-3"] {
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
        "apt-deploy-read",
        "apt-deploy-write",
        "apt-deploy-read-2",
        "apt-deploy-write-2",
        "apt-deploy-read-3",
        "apt-deploy-write-3",
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

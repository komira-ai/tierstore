//! The rpm jobs of `release.yml` ([`super::rpm`]): the file passes, and each way of giving a job
//! both the key and the role, leaving a job without its gate, signing the packages after they are
//! merged, uploading a package in the promotion, handing the role to the wrong job, or running a
//! verb in a job of another kind is named.

use super::lint::{lint, release_problems};
use super::platforms_tests::{each_named, in_job};
use super::rpm::{JOBS, rpm_problems};
use super::text_of;
use super::yaml;

#[test]
fn the_rpm_jobs_of_release_yml_pass_and_are_the_eighteen_the_design_names() {
    // Catches: an rpm job lost or renamed, which the defects below would not notice.
    let doc = yaml::load(&text_of("release.yml")).expect("release.yml parses");
    assert_eq!(rpm_problems(&doc), Vec::<String>::new());
    let jobs: Vec<&str> = JOBS.iter().map(|j| j.name).collect();
    let mut want = Vec::new();
    for suite in ["testing", "stable"] {
        for attempt in ["", "-2", "-3"] {
            for kind in ["read", "index", "write"] {
                want.push(format!("{suite}-rpm-{kind}{attempt}"));
            }
        }
    }
    assert_eq!(jobs, want);
    assert_eq!(
        release_problems(&text_of("release.yml")),
        Vec::<String>::new()
    );
}

/// `(job, text to replace once in it, its replacement, what the problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "testing-rpm-read",
        "needs: [plan, publish]",
        "needs: [plan]",
        "testing-rpm-read: needs are not",
    ),
    (
        "testing-rpm-read-2",
        "needs.testing-rpm-write.outputs.conflict == 'true' ",
        "",
        "testing-rpm-read-2: `if` is not",
    ),
    (
        "testing-rpm-read",
        "environment: apt-publish",
        "environment: apt-signing",
        "testing-rpm-read: environment is not apt-publish",
    ),
    (
        "testing-rpm-index",
        "environment: apt-signing",
        "environment: apt-publish",
        "testing-rpm-index: environment is not apt-signing",
    ),
    (
        "testing-rpm-index",
        "    permissions:\n      contents: read\n",
        "    permissions:\n      contents: read\n      id-token: write\n",
        "testing-rpm-index: permissions are [\"contents: read\", \"id-token: write\"], not [\"contents: read\"]",
    ),
    (
        "testing-rpm-write",
        "      contents: read\n      id-token: write\n",
        "      contents: write\n      id-token: write\n",
        "testing-rpm-write: permissions grant `contents: write`; only publish writes",
    ),
    (
        "testing-rpm-read",
        "id: gate\n",
        "id: check\n",
        "testing-rpm-read: no step has the id `gate`",
    ),
    (
        "testing-rpm-read",
        " --require \"RPM_REPO_READY=$RPM_REPO_READY\"",
        "",
        "testing-rpm-read: the gate is not given --require \"RPM_REPO_READY=$RPM_REPO_READY\"",
    ),
    (
        "testing-rpm-read",
        " --setting \"APT_HOSTNAME=${APT_HOSTNAME:+set}\"",
        "",
        "testing-rpm-read: the gate is not given the setting APT_HOSTNAME",
    ),
    (
        "stable-rpm-read",
        " --require \"TESTING_ENABLED=$TESTING_ENABLED\"",
        "",
        "stable-rpm-read: the gate is not given --require \"TESTING_ENABLED=$TESTING_ENABLED\"",
    ),
    (
        "testing-rpm-write",
        " --require \"INDEX_SIGNED=$INDEX_SIGNED\"",
        "",
        "testing-rpm-write: the gate is not given --require \"INDEX_SIGNED=$INDEX_SIGNED\"",
    ),
    (
        "testing-rpm-index",
        "      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        with:\n          name: rpm-current\n",
        "      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        with:\n          name: rpm-current\n",
        "testing-rpm-index: the step `actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c` after the gate does not have",
    ),
    (
        "testing-rpm-read",
        "      TAG: ${{ needs.plan.outputs.tag }}\n    steps:",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n    steps:",
        "testing-rpm-read: names APT_GPG_, the signing key, which only the apt and rpm index jobs may",
    ),
    (
        "testing-rpm-read",
        "        run: cargo xtask channels rpm repo read ",
        "          OTHER: ${{ secrets.OTHER }}\n        run: cargo xtask channels rpm repo read ",
        "names a secret; this job holds the role, not a key",
    ),
    (
        "testing-rpm-index",
        "      TAG: ${{ needs.plan.outputs.tag }}\n    steps:",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      ROLE: ${{ vars.APT_PUBLISH_ROLE_ARN }}\n    steps:",
        "testing-rpm-index: names APT_PUBLISH_; this job holds the key, not the role",
    ),
    (
        "tap",
        "      TAG: ${{ needs.plan.outputs.tag }}\n    steps:",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      ROLE: ${{ vars.APT_PUBLISH_ROLE_ARN }}\n    steps:",
        "tap: names APT_PUBLISH_ROLE_ARN, which only the apt and rpm jobs that read and write may",
    ),
    (
        "testing-rpm-write",
        " ROLE_ARN: ${{ vars.APT_PUBLISH_ROLE_ARN }}",
        " ROLE_ARN: arn:aws:iam::1:role/other",
        "testing-rpm-write: the role assumed is not ${{ vars.APT_PUBLISH_ROLE_ARN }}, the environment's",
    ),
    (
        "testing-rpm-write",
        "--version \"${TAG#v}\" --output",
        "--version \"${TAG#v}\" --last --output",
        "testing-rpm-write: `--last` must be passed by the last attempt's write and by no other",
    ),
    (
        "testing-rpm-write-3",
        " --last",
        "",
        "testing-rpm-write-3: `--last` must be passed by the last attempt's write and by no other",
    ),
    (
        "testing-rpm-write",
        " --signed-sums \"$RUNNER_TEMP/rpm-signed/SHA256SUMS.signed\"\n",
        "\n",
        "testing-rpm-write: `cargo xtask channels rpm check-signed...` is not given --signed-sums \"$RUNNER_TEMP/rpm-signed/SHA256SUMS.signed\"",
    ),
    (
        "testing-rpm-write",
        "cargo xtask channels rpm check-signed ",
        "echo checked ",
        "testing-rpm-write: a step must run `cargo xtask channels rpm check-signed...` before the role is assumed",
    ),
    (
        "testing-rpm-write",
        "cargo xtask release check dist",
        "echo check dist",
        "testing-rpm-write: no step checks the downloaded files",
    ),
    (
        "testing-rpm-index",
        "cargo xtask release check dist",
        "echo check dist",
        "testing-rpm-index: no step checks the downloaded files",
    ),
    (
        "testing-rpm-read",
        "channels rpm repo read --suite testing ",
        "channels rpm repo read --suite stable ",
        "testing-rpm-read: names `--suite stable`; this job is for the suite testing",
    ),
    (
        "testing-rpm-write",
        "channels rpm repo write --suite testing ",
        "channels rpm repo write ",
        "testing-rpm-write: `cargo xtask channels rpm repo write...` is not given `--suite testing`",
    ),
    (
        "testing-rpm-write",
        "--version \"${TAG#v}\" --output",
        "--version \"${TAG#v}\" --no-pool --output",
        "testing-rpm-write: passes --no-pool; the release's write puts the packages in the pool",
    ),
    (
        "stable-rpm-write",
        " --no-pool",
        "",
        "stable-rpm-write: does not pass --no-pool; the promotion writes the repodata only",
    ),
    (
        "stable-rpm-write",
        " --no-pool",
        " --no-pool --rpms x",
        "stable-rpm-write: passes --rpms; the promotion uploads no package",
    ),
    (
        "stable-rpm-read",
        "channels rpm repo read --suite stable ",
        "channels rpm repo read --suite testing ",
        "stable-rpm-read: `cargo xtask channels rpm repo read...` is not given `--suite stable`: the promotion reads both",
    ),
    (
        "stable-rpm-index",
        "--signed-sums \"$RUNNER_TEMP/rpm-signed/SHA256SUMS.signed\" --date",
        "--date",
        "stable-rpm-index: `cargo xtask channels rpm promote...` is not given --signed-sums \"$RUNNER_TEMP/rpm-signed/SHA256SUMS.signed\" ",
    ),
    (
        "stable-rpm-index",
        "--out \"$RUNNER_TEMP/rpm-next\" --version \"${TAG#v}\" --sums dist/SHA256SUMS ",
        "--out \"$RUNNER_TEMP/rpm-next\" --version \"${TAG#v}\" ",
        "stable-rpm-index: `cargo xtask channels rpm promote...` is not given --sums dist/SHA256SUMS ",
    ),
    (
        "testing-rpm-index",
        "--rpms \"$RUNNER_TEMP/rpm-signed\" ",
        "--rpms dist ",
        "testing-rpm-index: `cargo xtask channels rpm repo index...` is not given the signed packages",
    ),
    (
        "testing-rpm-index",
        "channels rpm repo index --suite testing ",
        "channels rpm repo index ",
        "testing-rpm-index: `cargo xtask channels rpm repo index...` is not given `--suite testing`",
    ),
    (
        "stable-rpm-index",
        "--dir \"$RUNNER_TEMP/rpm-next/stable\"",
        "--dir \"$RUNNER_TEMP/rpm-next/testing\"",
        "stable-rpm-index: the repodata is not signed in rpm-next/stable, the suite the job writes",
    ),
    (
        "testing-rpm-index",
        " --signed-at \"$(git show --no-patch --format=%ct HEAD)\"",
        " --signed-at \"$(date +%s)\"",
        "testing-rpm-index: the packages are not signed at the time of the commit",
    ),
    (
        "testing-rpm-index",
        "id: sign-rpms\n",
        "id: sign-the-rpms\n",
        "testing-rpm-index: the signing step does not have the id `sign-rpms`",
    ),
    (
        "testing-rpm-index",
        "id: sign\n",
        "id: sign-repo\n",
        "testing-rpm-index: the signing step does not have the id `sign`",
    ),
    (
        "testing-rpm-index",
        "APT_GPG_PASSPHRASE: ${{ secrets.APT_GPG_PASSPHRASE }}\n        run: |\n          umask 077\n          printf '%s' \"$APT_GPG_PRIVATE_KEY\" > \"$RUNNER_TEMP/rpm-key.asc\"\n          printf '%s' \"$APT_GPG_PASSPHRASE\" > \"$RUNNER_TEMP/rpm-passphrase\"\n          cargo xtask channels rpm sign ",
        "APT_GPG_PASSPHRASE: ${{ vars.APT_GPG_PASSPHRASE }}\n        run: |\n          umask 077\n          printf '%s' \"$APT_GPG_PRIVATE_KEY\" > \"$RUNNER_TEMP/rpm-key.asc\"\n          printf '%s' \"$APT_GPG_PASSPHRASE\" > \"$RUNNER_TEMP/rpm-passphrase\"\n          cargo xtask channels rpm sign ",
        "testing-rpm-index: the signing step `sign-rpms` does not read secrets.APT_GPG_PASSPHRASE",
    ),
    (
        "testing-rpm-index",
        "      - name: Merge the signed .rpms into the repodata",
        "      - name: Name a secret\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        env:\n          X: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n        run: echo\n      - name: Merge the signed .rpms into the repodata",
        "testing-rpm-index: the step `Name a secret` names a secret; only the gate and the two signing steps may",
    ),
    (
        "testing-rpm-read",
        "      - name: Gate on the settings",
        "      - name: Name the role before the gate\n        run: cargo xtask channels assume --role-arn x\n      - name: Gate on the settings",
        "testing-rpm-read: a step before the gate names channels assume",
    ),
    (
        "tap",
        "cargo xtask channels tap ",
        "cargo xtask channels rpm repo index ",
        "tap: runs `cargo xtask channels rpm repo index...`, which only the Index jobs of the suite testing of the rpm repository may",
    ),
    (
        "stable-rpm-write",
        "cargo xtask channels rpm repo write ",
        "cargo xtask channels rpm check-signed --dir x --version 1.2.3 --sums s && cargo xtask channels rpm repo write ",
        "stable-rpm-write: runs `cargo xtask channels rpm check-signed...`, which only the Write jobs of the suite testing of the rpm repository may",
    ),
    (
        "testing-rpm-write",
        "cargo xtask channels rpm repo write ",
        "cargo xtask channels rpm sign --rpms dist && cargo xtask channels rpm repo write ",
        "testing-rpm-write: runs `cargo xtask channels rpm sign...`, which only the Index jobs of the rpm repository may",
    ),
    (
        "testing-apt-index",
        "cargo xtask channels apt index ",
        "cargo xtask channels rpm repo sign --dir x && cargo xtask channels apt index ",
        "testing-apt-index: runs `cargo xtask channels rpm repo sign...`, which only the Index jobs of the rpm repository may",
    ),
];

#[test]
fn each_defect_in_the_rpm_jobs_is_named_by_the_lint() {
    // Catches: a pin of the rpm jobs that holds nothing, because the file passes whether or not
    // the thing it pins is there.
    each_named("release.yml", release_problems, DEFECTS);
}

#[test]
fn the_packages_are_signed_before_they_are_merged_and_the_repodata_after() {
    // Catches: a repodata that lists packages signed after the index was built (their digests
    // would differ), and a repomd.xml signed before it is complete.
    let text = text_of("release.yml");
    let sign_rpms = "      - name: Sign the .rpms with the signing subkey";
    let merge = "      - name: Merge the signed .rpms into the repodata";
    let sign_repo = "      - name: Sign repomd.xml of each architecture";
    let upload = "      - uses: actions/upload-artifact";
    let start = text.find("\n  testing-rpm-index:\n").expect("job");
    let (head, rest) = text.split_at(start);
    let at = |needle: &str| rest.find(needle).expect(needle);
    let (a, b, c, d) = (at(sign_rpms), at(merge), at(sign_repo), at(upload));
    assert!(a < b && b < c && c < d);
    // The merge before the package signature, then the repodata signature before the merge.
    let swapped = format!(
        "{head}{}{}{}{}{}",
        &rest[..a],
        &rest[b..c],
        &rest[a..b],
        &rest[c..d],
        &rest[d..]
    );
    let problems = release_problems(&swapped);
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("testing-rpm-index: it needs a step that signs the packages")),
        "{problems:?}"
    );
    let swapped = format!(
        "{head}{}{}{}{}",
        &rest[..b],
        &rest[c..d],
        &rest[b..c],
        &rest[d..]
    );
    let problems = release_problems(&swapped);
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("testing-rpm-index: it needs a step that signs the packages")),
        "{problems:?}"
    );
}

#[test]
fn a_job_that_is_missing_or_too_short_is_named() {
    // Catches: an rpm job deleted, and one with a gate but no merge, no signature or no role,
    // which the rules about the steps would otherwise skip.
    let doc = yaml::load("jobs: {}\n").expect("parses");
    let problems = rpm_problems(&doc);
    assert_eq!(problems.len(), 18);
    assert!(
        problems
            .iter()
            .all(|p| p.ends_with(": there is no such job"))
    );
    let short = "jobs:
  testing-rpm-read:
    steps:
      - id: gate
        run: echo
  testing-rpm-index:
    steps:
      - id: gate
        run: echo
  testing-rpm-write:
    steps:
      - id: gate
        run: echo
      - run: cargo xtask channels assume --role-arn x
        if: ${{ steps.gate.outputs.enabled == 'true' }}
      - run: cargo xtask channels rpm repo write --suite testing
        if: ${{ steps.gate.outputs.enabled == 'true' }}
";
    let problems = rpm_problems(&yaml::load(short).expect("parses"));
    assert!(
        problems.contains(
            &"testing-rpm-read: it needs one step that assumes the role and one that runs `cargo xtask channels rpm repo read...`"
                .to_owned()
        ),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.starts_with(
            "testing-rpm-index: it needs a step that signs the packages (`cargo xtask channels rpm sign...`), then one that merges (`cargo xtask channels rpm repo index...`)"
        )),
        "{problems:?}"
    );
    assert!(
        problems.contains(&"testing-rpm-write: the role assumed is not ${{ vars.APT_PUBLISH_ROLE_ARN }}, the environment's".to_owned()),
        "{problems:?}"
    );
}

#[test]
fn a_job_renamed_or_added_is_named_by_the_list_of_jobs() {
    // Catches: a fourth attempt, or an rpm job the lint does not hold.
    let text = text_of("release.yml");
    let renamed = text.replace("\n  testing-rpm-index-3:\n", "\n  testing-rpm-index-4:\n");
    let problems = release_problems(&renamed);
    assert!(
        problems.iter().any(|p| p.starts_with("jobs are")),
        "{problems:?}"
    );
    assert!(
        problems.contains(&"testing-rpm-index-3: there is no such job".to_owned()),
        "{problems:?}"
    );
}

#[test]
fn only_the_rpm_jobs_that_read_and_write_hold_the_cloud_token_and_the_index_job_never_does() {
    // Catches: `id-token: write` moving to the job that holds the signing key, and a job of the
    // rpm repository holding it under a condition that lets a run without a plan through.
    let text = text_of("release.yml");
    for job in [
        "testing-rpm-index",
        "testing-rpm-index-2",
        "testing-rpm-index-3",
        "stable-rpm-index",
        "stable-rpm-index-3",
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
    for job in JOBS.iter().filter(|j| j.kind != super::apt::Kind::Index) {
        let spoiled = in_job(
            &text,
            job.name,
            "needs.plan.outputs.release == 'true' && ",
            "",
        );
        assert!(
            lint("release.yml", &spoiled)
                .iter()
                .any(|p| p.starts_with(&format!(
                    "job {}: `id-token: write` needs the exact `if:",
                    job.name
                ))),
            "{}",
            job.name
        );
    }
}

#[test]
fn the_role_is_assumed_before_the_repodata_is_read_and_before_it_is_written() {
    // Catches: a read or write that runs without credentials.
    let text = text_of("release.yml");
    for (job, verb) in [
        (
            "testing-rpm-read",
            "      - name: Read the current repodata",
        ),
        (
            "testing-rpm-write",
            "      - name: Upload the .rpms once each",
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
            .find("\n  testing-rpm-")
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
}

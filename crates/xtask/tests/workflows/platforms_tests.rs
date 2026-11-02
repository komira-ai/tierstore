//! The release platforms in `artifacts.yml` and `ci.yml` ([`super::platforms`]): the files of
//! the repository pass, and each way of leaving a platform out is named.

use super::platforms::{RUNNERS, artifacts_platform_problems, ci_platform_problems, matches};
use super::text_of;
use xtask::package::Platform;
use xtask::release::ARTIFACTS;

fn platforms() -> Vec<&'static str> {
    Platform::ALL.iter().map(|p| p.name()).collect()
}

fn artifacts_problems_of(text: &str) -> Vec<String> {
    artifacts_platform_problems(text, &platforms(), &ARTIFACTS)
}

/// `text` with `replace` replaced by `with` inside the job `job` alone (from its key to the key
/// of the next job), which must hold it.
fn in_job(text: &str, job: &str, replace: &str, with: &str) -> String {
    let key = format!("\n  {job}:\n");
    let start = text.find(&key).unwrap_or_else(|| panic!("no job {job}")) + 1;
    let body = &text[start + key.len() - 1..];
    let end = body
        .match_indices("\n  ")
        .map(|(at, _)| at)
        .find(|&at| {
            let line = &body[at + 3..];
            line.starts_with(|c: char| c.is_ascii_alphanumeric())
                && line.split('\n').next().is_some_and(|l| l.ends_with(':'))
        })
        .map_or(text.len(), |at| start + key.len() - 1 + at);
    let slice = &text[start..end];
    assert!(
        slice.contains(replace),
        "{replace:?} is not in the job {job}"
    );
    format!(
        "{}{}{}",
        &text[..start],
        slice.replacen(replace, with, 1),
        &text[end..]
    )
}

#[test]
fn the_workflows_build_sign_and_check_every_release_platform() {
    // Catches: a release platform without its build job, its signing or its checks in
    // artifacts.yml, artifacts other than the ones a release waits for, and ci.yml without the
    // macOS build and tests.
    assert_eq!(
        artifacts_problems_of(&text_of("artifacts.yml")),
        Vec::<String>::new()
    );
    assert_eq!(
        ci_platform_problems(&text_of("ci.yml")),
        Vec::<String>::new()
    );
}

#[test]
fn every_platform_has_the_runner_of_its_own_os_and_architecture() {
    // Catches: a platform added to the release without a runner to build it on, and a runner
    // table that names a platform the release does not have.
    let mut listed: Vec<&str> = RUNNERS.iter().map(|(p, _)| *p).collect();
    listed.sort_unstable();
    let mut all = platforms();
    all.sort_unstable();
    assert_eq!(listed, all);
    assert!(RUNNERS.contains(&("macos-arm64", "macos-15")));
}

#[test]
fn a_download_pattern_takes_what_github_would_take() {
    // Catches: a pattern reader that takes too much (the sums or the bundles mixed into the
    // assets) or too little (a platform's assets left behind).
    for name in [
        "assets-linux-x86_64",
        "assets-linux-arm64",
        "assets-macos-arm64",
    ] {
        assert!(matches("assets-*", name), "{name}");
    }
    assert!(!matches("assets-*", "sha256sums"));
    assert!(!matches("assets-*", "bundles"));
    assert!(!matches("assets-linux-*", "assets-macos-arm64"));
    assert!(matches("assets-linux-*", "assets-linux-arm64"));
    assert!(matches("*", "bundles"));
    assert!(matches("bundles", "bundles"));
    assert!(!matches("bundles", "bundles-linux"));
    assert!(matches("a*b*c", "a-b-c"));
    assert!(!matches("a*b*c", "a-c"));
    assert!(!matches("ab*ba", "aba"));
}

/// `(job, text to replace in it, its replacement, what a problem must say)`.
const ARTIFACTS_DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "macos-arm64",
        "runs-on: macos-15",
        "runs-on: ubuntu-24.04-arm",
        "macos-arm64: runs on Some(\"ubuntu-24.04-arm\")",
    ),
    (
        "macos-arm64",
        "macos-arm64:\n",
        "mac:\n",
        "macos-arm64: no build job",
    ),
    (
        "macos-arm64",
        "--target \"$TARGET\" -p tierstore",
        "-p tierstore",
        "macos-arm64: no step builds tierstore",
    ),
    (
        "macos-arm64",
        "--platform macos-arm64 ",
        "--platform linux-arm64 ",
        "macos-arm64: no step packages with --platform macos-arm64",
    ),
    (
        "macos-arm64",
        "name: assets-macos-arm64",
        "name: assets-mac",
        "macos-arm64: uploads",
    ),
    (
        "sums",
        "needs: [linux-x86_64, linux-arm64, macos-arm64]",
        "needs: [linux-x86_64, linux-arm64]",
        "sums: does not need macos-arm64",
    ),
    (
        "sums",
        "pattern: assets-*",
        "pattern: assets-linux-*",
        "sums: no download takes assets-macos-arm64",
    ),
    (
        "attest",
        "pattern: assets-*",
        "pattern: assets-linux-*",
        "attest: no download takes assets-macos-arm64",
    ),
    (
        "verify",
        "pattern: assets-*",
        "pattern: assets-linux-*",
        "verify: no download takes assets-macos-arm64",
    ),
    (
        "verify",
        "pattern: assets-*",
        "pattern: '*'",
        "verify: the assets download `*` also takes sha256sums",
    ),
    (
        "attest",
        "sbom-path: dist/tierstore-${{ github.sha }}-macos-arm64.cdx.json",
        "sbom-path: dist/tierstore-${{ github.sha }}-linux-arm64.cdx.json",
        "attest: no SBOM attestation of dist/tierstore-${{ github.sha }}-macos-arm64.tar.gz",
    ),
    (
        "attest",
        "id: provenance-tierstore-macos-arm64",
        "id: provenance-mac",
        "attest: no provenance attestation `provenance-tierstore-macos-arm64`",
    ),
    (
        "attest",
        "-macos-arm64.tar.gz.sigstore.json\"",
        "-linux-arm64.tar.gz.sigstore.json\"",
        "attest: the bundle of macos-arm64 is not copied",
    ),
    (
        "attest",
        "name: bundles\n",
        "name: bundles-linux\n",
        "attest: uploads [\"bundles-linux\"]",
    ),
    (
        "verify",
        "name: bundles\n",
        "name: bundles-linux\n",
        "verify: does not download bundles",
    ),
    (
        "sums",
        "      contents: read\n",
        "      contents: read\n      id-token: write\n",
        "sums: id-token: write is held",
    ),
    (
        "macos-arm64",
        "      contents: read\n",
        "      contents: read\n      id-token: write\n",
        "macos-arm64: id-token: write is held",
    ),
    (
        "attest",
        "      id-token: write\n",
        "",
        "attest: id-token: write is not held",
    ),
    (
        "attest",
        "    steps:\n",
        "    steps:\n      - run: cargo xtask --help\n",
        "attest: runs repository code",
    ),
    (
        "attest",
        "    steps:\n",
        "    steps:\n      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n",
        "attest: runs repository code",
    ),
    (
        "attest",
        "id: provenance-tierstore-macos-arm64\n        uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2\n        with:\n          subject-path: dist/tierstore-${{ github.sha }}-macos-arm64.tar.gz",
        "id: provenance-tierstore-macos-arm64\n        uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2\n        with:\n          subject-checksums: dist/SHA256SUMS",
        "attest: no provenance attestation `provenance-tierstore-macos-arm64`",
    ),
    (
        "macos-arm64",
        "lookup-only: ${{ github.event_name == 'push' }}",
        "lookup-only: false",
        "macos-arm64: restores a cache on a push",
    ),
    (
        "macos-arm64",
        "--sha256 \"$CYCLONEDX_SHA256_AARCH64_MACOS\"",
        "--sha256 \"$CYCLONEDX_SHA256_MISSING\"",
        "macos-arm64: cargo-cyclonedx is not fetched against a SHA-256",
    ),
    (
        "attest",
        "    runs-on: ubuntu-24.04\n",
        "    runs-on: macos-15\n",
        "attest: does not run on an ubuntu runner",
    ),
];

#[test]
fn each_way_of_leaving_a_platform_out_of_artifacts_yml_is_named() {
    // Catches: a check above that passes whether or not the thing it holds is in the file: each
    // change below leaves artifacts.yml a valid workflow that drops the macos-arm64 build, its
    // signing or its checks, or lets a second job sign, and must be named.
    let text = text_of("artifacts.yml");
    for (job, replace, with, says) in ARTIFACTS_DEFECTS {
        let problems = artifacts_problems_of(&in_job(&text, job, replace, with));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn a_cyclonedx_pin_that_is_not_64_hex_digits_is_named() {
    // Catches: the checksum env of the macOS cargo-cyclonedx emptied or shortened while the step
    // still names it, so the fetch would check against nothing pinned.
    let text = text_of("artifacts.yml");
    let pin = "CYCLONEDX_SHA256_AARCH64_MACOS: 4c53dfa21e70b65bf7f8d2592aadde3bcb02c1a40b6ec63b877e5ca65a29e180";
    assert!(text.contains(pin), "the pin moved");
    for bad in [
        "CYCLONEDX_SHA256_AARCH64_MACOS: ''",
        "CYCLONEDX_SHA256_AARCH64_MACOS: 4c53dfa21e70b65b",
        "CYCLONEDX_SHA256_AARCH64_MACOS: 4C53DFA21E70B65BF7F8D2592AADDE3BCB02C1A40B6EC63B877E5CA65A29E180",
    ] {
        let problems = artifacts_problems_of(&text.replace(pin, bad));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("macos-arm64: cargo-cyclonedx is not fetched")),
            "{bad}: {problems:?}"
        );
    }
}

#[test]
fn artifacts_other_than_the_ones_a_release_waits_for_are_named() {
    // Catches: release.rs and artifacts.yml drifting apart (a release that waits for an artifact
    // no job uploads, or a job's upload a release never takes).
    let text = text_of("artifacts.yml");
    let fewer: Vec<&str> = ARTIFACTS
        .iter()
        .copied()
        .filter(|a| *a != "assets-macos-arm64")
        .collect();
    let problems = artifacts_platform_problems(&text, &platforms(), &fewer);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("not the artifacts a release waits for")),
        "{problems:?}"
    );
    let problems = artifacts_problems_of("jobs: [");
    assert_eq!(problems.len(), 1, "{problems:?}");
}

#[test]
fn a_ci_yml_without_the_macos_build_or_its_tests_is_named() {
    // Catches: the macOS job dropped from the pull-request gate, or kept but no longer building
    // the release binary or running the tests.
    let text = text_of("ci.yml");
    for (job, replace, with, says) in [
        (
            "macos",
            "runs-on: macos-15",
            "runs-on: ubuntu-latest",
            "no job runs on macos-15",
        ),
        (
            "macos",
            "--bin tierstore\n",
            "--lib\n",
            "does not build the release binary",
        ),
        (
            "macos",
            "cargo test --locked -p tierstore ",
            "cargo test --locked -p objstore-s3 ",
            "does not run the tests",
        ),
    ] {
        let problems = ci_platform_problems(&in_job(&text, job, replace, with));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
    assert_eq!(ci_platform_problems("jobs: [").len(), 1);
}

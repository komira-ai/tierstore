//! The release platforms in `artifacts.yml`, `release.yml` and `ci.yml` ([`super::platforms`]):
//! the files of the repository pass, and each way of leaving a platform out, of building, signing
//! or checking it with the wrong inputs, or of skipping a step that checks it, is named.

use super::platforms::{
    BUILDS, artifacts_platform_problems, attest_commands, build_commands, ci_platform_problems,
    matches, release_platform_problems,
};
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

/// For each `(job, replace, with, says)`: `file` with that one change in that job must give a
/// problem from `check` that contains `says`.
fn each_named(file: &str, check: fn(&str) -> Vec<String>, defects: &[(&str, &str, &str, &str)]) {
    let text = text_of(file);
    for (job, replace, with, says) in defects {
        let problems = check(&in_job(&text, job, replace, with));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{file} {job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn the_workflows_build_sign_and_check_every_release_platform() {
    // Catches: a release platform without its build job, its signing or its checks in
    // artifacts.yml or release.yml, artifacts other than the ones a release waits for, and ci.yml
    // without the macOS build and tests.
    assert_eq!(
        artifacts_problems_of(&text_of("artifacts.yml")),
        Vec::<String>::new()
    );
    assert_eq!(
        release_platform_problems(&text_of("release.yml"), &platforms()),
        Vec::<String>::new()
    );
    assert_eq!(
        ci_platform_problems(&text_of("ci.yml")),
        Vec::<String>::new()
    );
}

#[test]
fn every_platform_has_the_runner_and_the_rust_target_of_its_own_os_and_architecture() {
    // Catches: a platform added to the release without a build to make it, a build table that
    // names a platform the release does not have, and a platform built for another target.
    let mut listed: Vec<&str> = BUILDS.iter().map(|b| b.platform).collect();
    listed.sort_unstable();
    let mut all = platforms();
    all.sort_unstable();
    assert_eq!(listed, all);
    let mac = BUILDS
        .iter()
        .find(|b| b.platform == "macos-arm64")
        .expect("the macOS build");
    assert_eq!(
        (mac.runner, mac.target, mac.pin),
        (
            "macos-15",
            "aarch64-apple-darwin",
            "CYCLONEDX_SHA256_AARCH64_MACOS"
        )
    );
    let problems = artifacts_platform_problems(&text_of("artifacts.yml"), &["riscv64"], &ARTIFACTS);
    assert!(
        problems.contains(&"riscv64: no runner, target or pin for its build".to_owned()),
        "{problems:?}"
    );
}

#[test]
fn the_build_and_signing_commands_name_the_platform_and_nothing_else() {
    // Catches: a command template that would accept another platform's tarball, or that does not
    // check the binary it packages and the binary unpacked from the package.
    let mac = &BUILDS[2];
    let commands = build_commands(mac);
    assert!(
        commands
            .iter()
            .any(|c| c.contains("--platform macos-arm64 "))
    );
    assert!(
        commands
            .iter()
            .any(|c| c.contains("tierstore-${GITHUB_SHA}-macos-arm64.tar.gz"))
    );
    assert!(
        commands
            .iter()
            .any(|c| c.contains("--sha256 \"$CYCLONEDX_SHA256_AARCH64_MACOS\""))
    );
    let checked: Vec<&String> = commands
        .iter()
        .filter(|c| c.starts_with("cargo xtask build-commit"))
        .collect();
    assert_eq!(checked.len(), 2, "the built binary and the unpacked one");
    assert!(!commands.iter().any(|c| c.contains("linux")));
    let attest = attest_commands(&["macos-arm64"]);
    assert_eq!(attest.len(), 3);
    assert!(attest[2].ends_with("-macos-arm64.tar.gz.sigstore.json\""));
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

const MAC: &str = "macos-arm64";
const PIN: &str = "CYCLONEDX_SHA256_AARCH64_MACOS: 4c53dfa21e70b65bf7f8d2592aadde3bcb02c1a40b6ec63b877e5ca65a29e180";

/// Changes to the macOS build job: each leaves a valid workflow that builds, checks or packages
/// the wrong thing, for the wrong target, from a dirty target, or not at all.
const BUILD_DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        MAC,
        "runs-on: macos-15",
        "runs-on: ubuntu-24.04-arm",
        "macos-arm64: runs on Some(\"ubuntu-24.04-arm\")",
    ),
    (MAC, "macos-arm64:\n", "mac:\n", "macos-arm64: no build job"),
    (
        MAC,
        "TARGET: aarch64-apple-darwin",
        "TARGET: x86_64-apple-darwin",
        "macos-arm64: TARGET is Some(\"x86_64-apple-darwin\")",
    ),
    (
        MAC,
        "      TARGET: aarch64-apple-darwin\n",
        "",
        "macos-arm64: TARGET is None",
    ),
    (
        MAC,
        "--target \"$TARGET\" -p tierstore",
        "-p tierstore",
        "macos-arm64: does not run `cargo build --locked --release --target",
    ),
    (
        MAC,
        "--describe binaries --target \"$TARGET\"",
        "--describe binaries --target aarch64-unknown-linux-gnu",
        "macos-arm64: does not run `cargo cyclonedx --format json",
    ),
    (
        MAC,
        "--describe binaries --target \"$TARGET\"",
        "--describe binaries",
        "macos-arm64: does not run `cargo cyclonedx --format json",
    ),
    (
        MAC,
        "build-commit \"target/$TARGET/release/tierstore\"",
        "build-commit \"target/release/tierstore\"",
        "macos-arm64: does not run `cargo xtask build-commit \"target/$TARGET",
    ),
    (
        MAC,
        "\"$GITHUB_SHA\" --version",
        "\"0123456789abcdef0123456789abcdef01234567\" --version",
        "macos-arm64: does not run `cargo xtask build-commit \"target/$TARGET",
    ),
    (
        MAC,
        "--binary \"target/$TARGET/release/tierstore\"",
        "--binary \"target/release/tierstore\"",
        "macos-arm64: does not run `cargo xtask package",
    ),
    (
        MAC,
        "--commit \"$GITHUB_SHA\" --out dist",
        "--commit \"$GITHUB_HEAD_SHA\" --out dist",
        "macos-arm64: does not run `cargo xtask package",
    ),
    (
        MAC,
        "--platform macos-arm64 ",
        "--platform linux-arm64 ",
        "macos-arm64: does not run `cargo xtask package",
    ),
    (
        MAC,
        "-macos-arm64.tar.gz\" -C",
        "-linux-arm64.tar.gz\" -C",
        "macos-arm64: does not run `tar -xzf",
    ),
    (
        MAC,
        "build-commit \"$RUNNER_TEMP/unpacked/tierstore\"",
        "build-commit \"target/$TARGET/release/tierstore\"",
        "macos-arm64: does not run `cargo xtask build-commit \"$RUNNER_TEMP/unpacked/tierstore\"",
    ),
    (
        MAC,
        "      - run: mkdir \"$RUNNER_TEMP/unpacked\"\n",
        "",
        "macos-arm64: does not run `mkdir",
    ),
    (
        MAC,
        "      - run: cargo cyclonedx --version\n",
        "      - run: cargo cyclonedx --version\n      - run: cargo cyclonedx --version\n",
        "macos-arm64: runs a command twice",
    ),
    (
        MAC,
        "      - run: cargo cyclonedx --version\n",
        "      - run: cargo cyclonedx --version\n      - run: ./target/aarch64-apple-darwin/release/tierstore --help\n",
        "macos-arm64: runs `./target/aarch64-apple-darwin/release/tierstore --help`, which is not one of its commands",
    ),
    (
        MAC,
        "--sha256 \"$CYCLONEDX_SHA256_AARCH64_MACOS\"",
        "--sha256 \"$CYCLONEDX_SHA256_AARCH64_LINUX\"",
        "macos-arm64: does not run `cargo xtask fetch",
    ),
    (
        MAC,
        "name: assets-macos-arm64",
        "name: assets-mac",
        "macos-arm64: uploads",
    ),
    (
        MAC,
        "path: dist/\n",
        "path: target/\n",
        "macos-arm64: uploads",
    ),
    (
        MAC,
        "lookup-only: ${{ github.event_name == 'push' }}",
        "lookup-only: false",
        "macos-arm64: restores a cache on a push",
    ),
    (
        MAC,
        "      - run: rustup toolchain install\n",
        "      - run: rustup toolchain install\n      - uses: actions/cache@0400d5f644dc74513175e3cd8d07132dd4860809 # v4.2.4\n        with:\n          path: target\n          key: k\n",
        "macos-arm64: uses actions/cache@0400d5f644dc74513175e3cd8d07132dd4860809, which is not one of its actions",
    ),
    (
        MAC,
        "      TARGET: aarch64-apple-darwin\n",
        "      TARGET: aarch64-apple-darwin\n      CYCLONEDX_SHA256_AARCH64_MACOS: 0000000000000000000000000000000000000000000000000000000000000000\n",
        "macos-arm64: sets CYCLONEDX_SHA256_AARCH64_MACOS",
    ),
    (
        MAC,
        "      - run: cargo build --locked --release --target \"$TARGET\"",
        "      - env:\n          TARGET: x86_64-apple-darwin\n        run: cargo build --locked --release --target \"$TARGET\"",
        "macos-arm64: the step `cargo build --locked --release --target \"$TARGET\" -p tierstore --features server --bin tierstore` sets its own env [\"TARGET\"]",
    ),
    (
        MAC,
        "      - name: SBOM\n",
        "      - name: SBOM\n        env:\n          TARGET: x86_64-apple-darwin\n",
        "macos-arm64: the step `SBOM` sets its own env [\"TARGET\"]",
    ),
    (
        MAC,
        "      - name: The packaged tierstore names the commit and the version\n",
        "      - name: The packaged tierstore names the commit and the version\n        env:\n          TIERSTORE_BUILD_VERSION_OVERRIDE: 9.9.9\n",
        "macos-arm64: the step `The packaged tierstore names the commit and the version` sets its own env",
    ),
    (
        MAC,
        "      TARGET: aarch64-apple-darwin\n",
        "      TARGET: aarch64-apple-darwin\n      CARGO_BUILD_TARGET: x86_64-apple-darwin\n",
        "macos-arm64: its env sets [\"CARGO_BUILD_TARGET\", \"TARGET\", \"TIERSTORE_BUILD_VERSION_OVERRIDE\"]",
    ),
    (
        MAC,
        "      - name: Install cargo-cyclonedx (pinned version and checksum)\n",
        "      - name: Install cargo-cyclonedx (pinned version and checksum)\n        env:\n          CYCLONEDX_SHA256_AARCH64_MACOS: 0000000000000000000000000000000000000000000000000000000000000000\n",
        "the step `Install cargo-cyclonedx (pinned version and checksum)`: sets CYCLONEDX_SHA256_AARCH64_MACOS",
    ),
];

/// Changes that let a required job or step be skipped or fail unseen.
const EXECUTION_DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        MAC,
        "    runs-on: macos-15\n",
        "    if: false\n    runs-on: macos-15\n",
        "macos-arm64: runs under `if: Some(\"false\")`",
    ),
    (
        MAC,
        "    runs-on: macos-15\n",
        "    continue-on-error: true\n    runs-on: macos-15\n",
        "macos-arm64: has continue-on-error",
    ),
    (
        MAC,
        "      - run: cargo build --locked",
        "      - if: false\n        run: cargo build --locked",
        "macos-arm64: the step `cargo build --locked",
    ),
    (
        MAC,
        "      - name: tierstore names the commit and the version\n",
        "      - name: tierstore names the commit and the version\n        continue-on-error: true\n",
        "macos-arm64: the step `tierstore names the commit and the version` has continue-on-error",
    ),
    (
        "sums",
        "    runs-on: ubuntu-24.04\n",
        "    if: false\n    runs-on: ubuntu-24.04\n",
        "sums: runs under `if: Some(\"false\")`",
    ),
    (
        "verify",
        "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n",
        "    if: ${{ github.event_name == 'push' }}\n",
        "verify: runs under",
    ),
    (
        "attest",
        "      - name: Refuse listed files that differ from the sums\n",
        "      - name: Refuse listed files that differ from the sums\n        if: false\n",
        "attest: the step `Refuse listed files that differ from the sums` has an `if:`",
    ),
];

/// Changes to the jobs that sum, sign and check: a platform's files left out, mixed with other
/// artifacts, put where the next step does not look, signed for another file, or a signing job
/// that runs something built here.
const CONSUMER_DEFECTS: &[(&str, &str, &str, &str)] = &[
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
        "sums",
        "          path: dist\n          merge-multiple: true",
        "          path: assets\n          merge-multiple: true",
        "sums: the assets download is not into dist",
    ),
    (
        "attest",
        "pattern: assets-*",
        "pattern: assets-linux-*",
        "attest: no download takes assets-macos-arm64",
    ),
    (
        "attest",
        "merge-multiple: true",
        "merge-multiple: false",
        "attest: the assets download does not merge the platforms into one directory",
    ),
    (
        "attest",
        "          name: sha256sums\n          path: dist",
        "          name: sha256sums\n          path: published",
        "attest: does not download sha256sums into dist",
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
        "verify",
        "          name: bundles\n",
        "          name: bundles-linux\n",
        "verify: does not download bundles into dist",
    ),
    (
        "verify",
        "          name: sha256sums\n          path: published",
        "          name: sha256sums\n          path: dist",
        "verify: does not download sha256sums into published",
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
        "id: provenance-tierstore-macos-arm64\n        uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2\n        with:\n          subject-path: dist/tierstore-${{ github.sha }}-macos-arm64.tar.gz",
        "id: provenance-tierstore-macos-arm64\n        uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2\n        with:\n          subject-checksums: dist/SHA256SUMS",
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
        "attest: uploads [(Some(\"bundles-linux\")",
    ),
    (
        "attest",
        "path: dist/*.sigstore.json",
        "path: dist/",
        "attest: uploads [(Some(\"bundles\"), Some(\"dist/\"))]",
    ),
    (
        "sums",
        "      contents: read\n",
        "      contents: read\n      id-token: write\n",
        "sums: id-token: write is held",
    ),
    (
        MAC,
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
        "attest: runs `cargo xtask --help`, which is not one of its commands",
    ),
    (
        "attest",
        "    steps:\n",
        "    steps:\n      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n",
        "attest: uses actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1, which is not one of its actions",
    ),
    (
        "attest",
        "      - uses: actions/upload-artifact@",
        "      - run: tar -xzf dist/tierstore-${GITHUB_SHA}-linux-x86_64.tar.gz tierstore && ./tierstore --version\n      - uses: actions/upload-artifact@",
        "attest: runs `tar -xzf dist/tierstore-${GITHUB_SHA}-linux-x86_64.tar.gz tierstore && ./tierstore --version`, which is not one of its commands",
    ),
    (
        "attest",
        "    runs-on: ubuntu-24.04\n",
        "    runs-on: macos-15\n",
        "attest: does not run on an ubuntu runner",
    ),
];

#[test]
fn each_wrong_input_of_the_macos_build_is_named() {
    // Catches: a check that matches a word in a command and so passes a wrong target, SBOM
    // target, binary, commit, platform or tarball; a build that no longer checks the binary
    // unpacked from its tarball; a cache that dirties the signed build; and a pin shadowed by a
    // job's or a step's own env.
    each_named("artifacts.yml", artifacts_problems_of, BUILD_DEFECTS);
}

#[test]
fn each_way_of_skipping_a_required_job_or_step_is_named() {
    // Catches: a job or step that still holds the right command but can be skipped (`if:`) or can
    // fail without failing the run (`continue-on-error`).
    each_named("artifacts.yml", artifacts_problems_of, EXECUTION_DEFECTS);
}

#[test]
fn each_way_of_leaving_the_macos_files_out_of_the_sums_the_signing_or_the_check_is_named() {
    // Catches: the Mac files not summed, signed or verified; downloads that put them where the
    // next step does not look; a second job that can sign; and a signing job that runs anything
    // but the signing actions and its three kinds of commands (such as a binary built here).
    each_named("artifacts.yml", artifacts_problems_of, CONSUMER_DEFECTS);
}

#[test]
fn a_cyclonedx_pin_that_is_not_64_hex_digits_is_named() {
    // Catches: the checksum env of the macOS cargo-cyclonedx emptied or shortened while the step
    // still names it, so the fetch would check against nothing pinned.
    let text = text_of("artifacts.yml");
    assert!(text.contains(PIN), "the pin moved");
    for bad in [
        "CYCLONEDX_SHA256_AARCH64_MACOS: ''",
        "CYCLONEDX_SHA256_AARCH64_MACOS: 4c53dfa21e70b65b",
        "CYCLONEDX_SHA256_AARCH64_MACOS: 4C53DFA21E70B65BF7F8D2592AADDE3BCB02C1A40B6EC63B877E5CA65A29E180",
    ] {
        let problems = artifacts_problems_of(&text.replace(PIN, bad));
        assert!(
            problems.iter().any(|p| p.contains(
                "macos-arm64: the cargo-cyclonedx pin CYCLONEDX_SHA256_AARCH64_MACOS is not"
            )),
            "{bad}: {problems:?}"
        );
    }
}

#[test]
fn commands_out_of_order_are_named() {
    // Catches: the tarball unpacked before it is packaged (the check of the unpacked binary would
    // then read a file of another run or none).
    let text = text_of("artifacts.yml");
    let mkdir = "      - run: mkdir \"$RUNNER_TEMP/unpacked\"\n";
    let moved = in_job(&text, MAC, mkdir, "");
    let moved = in_job(
        &moved,
        MAC,
        "      - run: cargo cyclonedx --version\n",
        &format!("{mkdir}      - run: cargo cyclonedx --version\n"),
    );
    let problems = artifacts_problems_of(&moved);
    assert!(
        problems.contains(&"macos-arm64: runs its commands out of order".to_owned()),
        "{problems:?}"
    );
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

fn release_problems_of(text: &str) -> Vec<String> {
    release_platform_problems(text, &platforms())
}

#[test]
fn a_release_yml_that_does_not_take_the_macos_files_where_it_checks_them_is_named() {
    // Catches: release.yml left on the old names (`assets-linux-*`, `bundles-linux`), which would
    // publish no Mac files or fail to find the bundles, and downloads into another directory or
    // unmerged, which the release check would not find.
    each_named(
        "release.yml",
        release_problems_of,
        &[
            (
                "verify",
                "pattern: assets-*",
                "pattern: assets-linux-*",
                "release.yml verify: no download takes assets-macos-arm64",
            ),
            (
                "verify",
                "          name: bundles\n",
                "          name: bundles-linux\n",
                "release.yml verify: does not download bundles into dist",
            ),
            (
                "verify",
                "          pattern: assets-*\n          path: dist",
                "          pattern: assets-*\n          path: assets",
                "release.yml verify: the assets download is not into dist",
            ),
            (
                "verify",
                "merge-multiple: true",
                "merge-multiple: false",
                "release.yml verify: the assets download does not merge",
            ),
            (
                "verify",
                "          name: sha256sums\n          path: published",
                "          name: sha256sums\n          path: dist",
                "release.yml verify: does not download sha256sums into published",
            ),
        ],
    );
    assert_eq!(release_problems_of("jobs: [").len(), 1);
}

#[test]
fn a_ci_yml_without_the_macos_build_or_every_test_or_that_can_skip_them_is_named() {
    // Catches: the macOS job dropped from the pull-request gate; kept but no longer building the
    // release binary; its tests only compiled (`--no-run`) or filtered to a few; and the job or a
    // step skipped or allowed to fail.
    let test = "cargo test --locked -p tierstore -p objstore-s3 -p tierstore-testkit --features tierstore/s3 --no-fail-fast";
    each_named(
        "ci.yml",
        ci_platform_problems,
        &[
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
                "ci.yml macos: does not run `cargo build --locked --release",
            ),
            (
                "macos",
                "--no-fail-fast\n",
                "--no-fail-fast --no-run\n",
                "ci.yml macos: does not run `cargo test",
            ),
            (
                "macos",
                "--no-fail-fast\n",
                "--no-fail-fast -- docs_tests\n",
                "ci.yml macos: does not run `cargo test",
            ),
            (
                "macos",
                "-p tierstore -p objstore-s3",
                "-p objstore-s3",
                "ci.yml macos: does not run `cargo test",
            ),
            (
                "macos",
                "    runs-on: macos-15\n",
                "    if: false\n    runs-on: macos-15\n",
                "ci.yml macos: runs under `if: Some(\"false\")`",
            ),
            (
                "macos",
                &format!("      - run: {test}\n"),
                &format!("      - run: {test}\n        continue-on-error: true\n"),
                "ci.yml macos: the step",
            ),
            (
                "macos",
                &format!("      - run: {test}\n"),
                &format!("      - if: false\n        run: {test}\n"),
                "has an `if:`",
            ),
        ],
    );
    assert_eq!(ci_platform_problems("jobs: [").len(), 1);
}

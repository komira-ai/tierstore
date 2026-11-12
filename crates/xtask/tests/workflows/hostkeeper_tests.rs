//! The hostkeeper packages and their release index in `artifacts.yml` and `release.yml`
//! ([`super::hostkeeper`], and the build and signing commands of [`super::platforms`]): the files
//! of the repository pass, a pull request builds and sums the Linux packages and never signs them
//! or starts the macOS job, a push to main or a run by hand builds the macOS package, and each way
//! of leaving out a step that makes, signs or checks a package or the index is named.

use super::hostkeeper::{
    PACKAGE_VERSION, SUMS_INDEX, SUMS_PATHS, artifacts_index_problems, regenerate,
    release_index_problems, verify_index,
};
use super::lint::{SIGN_IF, entry};
use super::platforms::{
    BUILDS, CODESIGN, NOT_FOR_A_PULL_REQUEST, SUMS_IF, artifacts_platform_problems, build_commands,
    package_copy,
};
use super::platforms_tests::{each_named, in_job};
use super::text_of;
use super::yaml::{self, Node};
use xtask::package::Platform;
use xtask::release::ARTIFACTS;

fn platforms() -> Vec<&'static str> {
    Platform::ALL.iter().map(|p| p.name()).collect()
}

fn platform_problems_of(text: &str) -> Vec<String> {
    artifacts_platform_problems(text, &platforms(), &ARTIFACTS)
}

/// The text at `path` (keys, one level each) of the job `name` of the workflow `file`.
fn job_value(doc: &Node, name: &str, path: &[&str]) -> Option<String> {
    let mut here = entry(doc, "jobs").and_then(|j| entry(j, name))?;
    for key in path {
        here = entry(here, key)?;
    }
    here.as_str().map(str::to_owned)
}

#[test]
fn the_workflows_write_sign_and_check_the_index_of_the_hostkeeper_packages() {
    // Catches: artifacts.yml or release.yml without the step that writes the index, hands it on,
    // makes it again or takes each package from it, as these files are.
    assert_eq!(
        artifacts_index_problems(&text_of("artifacts.yml")),
        Vec::<String>::new()
    );
    assert_eq!(
        release_index_problems(&text_of("release.yml")),
        Vec::<String>::new()
    );
}

#[test]
fn a_pull_request_builds_and_sums_the_linux_x86_64_package_and_never_signs_it_or_starts_a_beta_job()
{
    // Catches: a pull request that does not build or unpack the linux-x86_64 hostkeeper package,
    // whose sums job would wait for (or refuse without) the arm64 or macOS package, that signs
    // anything, or that starts a beta job (linux-arm64, macos-arm64). The index step's own answer
    // for a pull request (the linux-x86_64 package, no index written) is the unit test of
    // `release hostkeeper-index`.
    let doc = yaml::load(&text_of("artifacts.yml")).expect("artifacts.yml");
    let triggers = super::lint::names(entry(&doc, "on"));
    assert!(triggers.iter().any(|t| t == "pull_request"), "{triggers:?}");
    for (build, condition) in BUILDS.iter().zip([
        None,
        Some(NOT_FOR_A_PULL_REQUEST),
        Some(NOT_FOR_A_PULL_REQUEST),
    ]) {
        let name = build.platform;
        assert_eq!(
            job_value(&doc, name, &["if"]).as_deref(),
            condition,
            "{name}: runs for a pull request only if it is linux-x86_64"
        );
        let linux = build;
        if name == "macos-arm64" {
            continue;
        }
        let commands = build_commands(linux);
        assert!(
            commands.iter().any(|c| c.starts_with(
                "cargo xtask release hostkeeper-package --binary \"target/$TARGET/release/tierstore\""
            ) && c.contains(&format!("--platform {name} --out dist"))),
            "{name}: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c
                .starts_with("cargo xtask verify-assets hostkeeper-package dist")
                && c.contains(&format!("--platform {name} "))),
            "{name}: {commands:?}"
        );
        assert!(!commands.iter().any(|c| c == CODESIGN), "{name}");
    }
    assert_eq!(job_value(&doc, "sums", &["if"]).as_deref(), Some(SUMS_IF));
    assert!(SUMS_IF.contains(
        "(github.event_name == 'pull_request' && needs.linux-arm64.result == 'skipped' && needs.macos-arm64.result == 'skipped')"
    ));
    assert!(SUMS_INDEX.contains("--event \"$GITHUB_EVENT_NAME\""));
    for signer in ["attest", "verify"] {
        assert_eq!(
            job_value(&doc, signer, &["if"]).as_deref(),
            Some(SIGN_IF),
            "{signer} runs for a push to main alone"
        );
    }
    assert_eq!(
        platform_problems_of(&text_of("artifacts.yml")),
        Vec::<String>::new()
    );
}

#[test]
fn a_main_or_manual_run_builds_the_macos_package_in_the_macos_job_and_its_index_needs_it() {
    // Catches: the macOS package built nowhere, built without the check of its binary's code
    // signature, or a sums job that writes an index on a push or a run by hand without the
    // macOS job's success. That the index step refuses a dist without the macOS package is the
    // unit test of `release hostkeeper-index`.
    let mac = &BUILDS[2];
    assert_eq!(mac.platform, "macos-arm64");
    assert_eq!(mac.job_if, Some(NOT_FOR_A_PULL_REQUEST));
    let commands = build_commands(mac);
    let package = commands
        .iter()
        .position(|c| {
            c.starts_with("cargo xtask release hostkeeper-package")
                && c.contains("--platform macos-arm64 --out dist")
        })
        .expect("the macOS job makes the macOS package");
    let unpack = commands
        .iter()
        .position(|c| {
            c.starts_with("cargo xtask verify-assets hostkeeper-package dist")
                && c.contains("--platform macos-arm64 ")
        })
        .expect("and unpacks it");
    assert!(package < unpack);
    assert_eq!(
        commands.last().map(String::as_str),
        Some(CODESIGN),
        "the signature of the unpacked binary is checked last"
    );
    assert!(SUMS_IF.contains("needs.macos-arm64.result == 'success'"));
    assert_eq!(
        regenerate("$GITHUB_SHA"),
        "cargo xtask release hostkeeper-index dist --commit \"$GITHUB_SHA\" --event \"$GITHUB_EVENT_NAME\""
    );
    assert_eq!(
        package_copy("macos-arm64"),
        "cp \"$BUNDLE\" \"dist/tierstore-${PACKAGE_VERSION}-macos-arm64.tar.zst.sigstore.json\""
    );
}

const SUMS_STEP: &str = "      - name: The release index of the hostkeeper packages\n        id: index\n        run: cargo xtask release hostkeeper-index dist --commit \"$GITHUB_SHA\" --event \"$GITHUB_EVENT_NAME\" --output \"$GITHUB_OUTPUT\"\n";

/// Changes to `sums`, `attest` and `verify` that leave the index out of the sums, away from the
/// signing job, or unchecked.
const INDEX_DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "sums",
        SUMS_STEP,
        "",
        "sums: does not run `cargo xtask release hostkeeper-index dist --commit \"$GITHUB_SHA\" --event \"$GITHUB_EVENT_NAME\" --output \"$GITHUB_OUTPUT\"`",
    ),
    (
        "sums",
        "--event \"$GITHUB_EVENT_NAME\" --output",
        "--event push --output",
        "sums: does not run `cargo xtask release hostkeeper-index",
    ),
    (
        "sums",
        "        id: index\n",
        "",
        "sums: the step `cargo xtask release hostkeeper-index",
    ),
    (
        "sums",
        "      package-version: ${{ steps.index.outputs.package-version }}\n",
        "",
        "sums: output package-version is None",
    ),
    (
        "sums",
        "            dist/index.json\n",
        "",
        "sums: the sha256sums upload takes Some(\"dist/SHA256SUMS\\n\")",
    ),
    (
        "attest",
        "    needs: [version, sums]\n",
        "    needs: [version]\n",
        "attest: does not need sums",
    ),
    (
        "verify",
        "      - name: The release index, made again from the packages\n        run: cargo xtask release hostkeeper-index dist --commit \"$GITHUB_SHA\" --event \"$GITHUB_EVENT_NAME\"\n",
        "",
        "verify: does not run `cargo xtask release hostkeeper-index dist --commit \"$GITHUB_SHA\" --event \"$GITHUB_EVENT_NAME\"`",
    ),
    (
        "verify",
        "        run: cargo xtask verify-assets hostkeeper-index dist --commit \"$GITHUB_SHA\" --scratch \"$RUNNER_TEMP/hostkeeper\"\n",
        "        run: cargo xtask verify-assets hostkeeper-package dist\n",
        "verify: does not run `cargo xtask verify-assets hostkeeper-index",
    ),
    (
        "verify",
        "        run: cargo xtask release check dist --commit \"$GITHUB_SHA\"\n",
        "        run: cargo xtask release check dist\n",
        "verify: does not run `cargo xtask release check dist --commit \"$GITHUB_SHA\"`",
    ),
];

#[test]
fn each_way_of_leaving_the_index_out_of_the_sums_the_signing_or_the_check_is_named() {
    // Catches: an index the sums do not list, written after them, under another event, not handed
    // to the signing job or not uploaded with the sums, a signing job that cannot read the
    // packages' version, and a verify job that does not make the index again or take each
    // package from it.
    each_named("artifacts.yml", artifacts_index_problems, INDEX_DEFECTS);
}

#[test]
fn an_index_written_after_the_sums_are_assembled_is_named() {
    // Catches: the index written after the assembly, so the sums the attest job signs do not list
    // it, though every command is there.
    let text = text_of("artifacts.yml");
    let moved = in_job(&text, "sums", SUMS_STEP, "");
    let moved = in_job(
        &moved,
        "sums",
        "      - run: cargo xtask verify-assets sums dist\n",
        &format!("{SUMS_STEP}      - run: cargo xtask verify-assets sums dist\n"),
    );
    let problems = artifacts_index_problems(&moved);
    assert_eq!(
        problems,
        [format!(
            "sums: runs `{SUMS_INDEX}` after `cargo xtask sums assemble dist...`"
        )]
    );
    let verify_moved = in_job(
        &text,
        "verify",
        "      - name: The release index, made again from the packages\n        run: cargo xtask release hostkeeper-index dist --commit \"$GITHUB_SHA\" --event \"$GITHUB_EVENT_NAME\"\n",
        "",
    );
    let verify_moved = in_job(
        &verify_moved,
        "verify",
        "      - name: Check the files against SHA256SUMS\n",
        "      - run: cargo xtask release hostkeeper-index dist --commit \"$GITHUB_SHA\" --event \"$GITHUB_EVENT_NAME\"\n      - name: Check the files against SHA256SUMS\n",
    );
    assert_eq!(
        artifacts_index_problems(&verify_moved),
        [format!(
            "verify: runs `{}` after `cargo xtask sums assemble dist --expect...`",
            regenerate("$GITHUB_SHA")
        )]
    );
    assert!(PACKAGE_VERSION.contains("steps.index.outputs"));
    assert_eq!(SUMS_PATHS.lines().count(), 2);
}

/// Changes to `release.yml` that leave the index unmade or a package unchecked before publishing.
const RELEASE_DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "verify",
        "      - name: The release index, made again from the packages\n        run: cargo xtask release hostkeeper-index dist --commit \"$COMMIT\" --event \"$GITHUB_EVENT_NAME\"\n",
        "",
        "release.yml verify: does not run `cargo xtask release hostkeeper-index dist --commit \"$COMMIT\"",
    ),
    (
        "verify",
        "        run: cargo xtask verify-assets hostkeeper-index dist --commit \"$COMMIT\" --scratch \"$RUNNER_TEMP/hostkeeper\"\n",
        "        run: cargo xtask verify-assets sums dist\n",
        "release.yml verify: does not run `cargo xtask verify-assets hostkeeper-index",
    ),
    (
        "publish",
        "        run: cargo xtask verify-assets hostkeeper-index dist --commit \"$COMMIT\" --scratch \"$RUNNER_TEMP/hostkeeper\"\n",
        "        run: cargo xtask verify-assets sums dist\n",
        "release.yml publish: does not run `cargo xtask verify-assets hostkeeper-index",
    ),
    (
        "publish",
        "        run: cargo xtask release publish dist",
        "        run: cargo xtask release check dist",
        "release.yml publish: does not run `cargo xtask release publish...`",
    ),
];

#[test]
fn a_release_yml_that_does_not_make_the_index_again_or_check_each_package_is_named() {
    // Catches: a release whose index is the build's file taken on trust instead of made again from
    // the packages (the published sums would then not hold it to the signed bytes), or whose
    // packages are published without being taken from the index as a host takes them.
    each_named("release.yml", release_index_problems, RELEASE_DEFECTS);
    assert_eq!(
        verify_index("$COMMIT"),
        "cargo xtask verify-assets hostkeeper-index dist --commit \"$COMMIT\" --scratch \"$RUNNER_TEMP/hostkeeper\""
    );
}

/// Changes to the build and signing jobs that make, unpack or sign a hostkeeper package wrongly
/// or not at all.
const PACKAGE_DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "macos-arm64",
        "      - name: The packaged binary carries a valid code signature\n",
        "      - name: The packaged binary carries a valid code signature\n        if: false\n",
        "macos-arm64: the step `The packaged binary carries a valid code signature` has an `if:`",
    ),
    (
        "macos-arm64",
        "        run: codesign --verify --strict",
        "        run: codesign --verify",
        "macos-arm64: does not run `codesign --verify --strict",
    ),
    (
        "linux-arm64",
        "--min-hostkeeper 0.1.0 --platform linux-arm64 --out dist",
        "--min-hostkeeper 0.1.0 --platform linux-x86_64 --out dist",
        "linux-arm64: runs `cargo xtask release hostkeeper-package --binary \"target/$TARGET/release/tierstore\" --version \"$RELEASE_VERSION+${GITHUB_SHA::12}\" --min-hostkeeper 0.1.0 --platform linux-x86_64 --out dist`, which is not one of its commands",
    ),
    (
        "linux-x86_64",
        "--version \"$RELEASE_VERSION+${GITHUB_SHA::12}\" --min-hostkeeper",
        "--version \"$RELEASE_VERSION\" --min-hostkeeper",
        "linux-x86_64: does not run `cargo xtask release hostkeeper-package",
    ),
    (
        "linux-x86_64",
        "        run: cargo xtask verify-assets hostkeeper-package dist --version \"$RELEASE_VERSION+${GITHUB_SHA::12}\" --platform linux-x86_64 --scratch \"$RUNNER_TEMP/hostkeeper\"\n",
        "        run: cargo xtask verify-assets sums dist\n",
        "linux-x86_64: does not run `cargo xtask verify-assets hostkeeper-package",
    ),
    (
        "attest",
        "          subject-path: dist/tierstore-${{ needs.sums.outputs.package-version }}-macos-arm64.tar.zst\n",
        "          subject-path: dist/tierstore-*-macos-arm64.tar.zst\n",
        "attest: no provenance attestation `provenance-hostkeeper-macos-arm64` of dist/tierstore-${{ needs.sums.outputs.package-version }}-macos-arm64.tar.zst alone",
    ),
    (
        "attest",
        "      - id: provenance-hostkeeper-linux-arm64\n",
        "      - id: provenance-linux-arm64\n",
        "attest: no provenance attestation `provenance-hostkeeper-linux-arm64`",
    ),
    (
        "attest",
        "        run: cp \"$BUNDLE\" \"dist/tierstore-${PACKAGE_VERSION}-macos-arm64.tar.zst.sigstore.json\"\n",
        "        run: cp \"$BUNDLE\" \"dist/tierstore-${PACKAGE_VERSION}-linux-arm64.tar.zst.sigstore.json\"\n",
        "attest: the bundle of the hostkeeper package of macos-arm64 is not copied as",
    ),
    (
        "attest",
        "          BUNDLE: ${{ steps.provenance-hostkeeper-linux-x86_64.outputs.bundle-path }}\n          PACKAGE_VERSION: ${{ needs.sums.outputs.package-version }}\n",
        "          BUNDLE: ${{ steps.provenance-hostkeeper-linux-x86_64.outputs.bundle-path }}\n          PACKAGE_VERSION: ${{ needs.version.outputs.version }}\n",
        "attest: the bundle of the hostkeeper package of linux-x86_64 is not copied as",
    ),
];

#[test]
fn each_wrong_or_missing_step_of_a_hostkeeper_package_is_named() {
    // Catches: a package built for another platform's name, under a version the binary does not
    // report, not unpacked as a host unpacks it, a Mac binary whose signature is not checked or
    // whose check can be skipped, and a package signed by a glob (which would also sign a file
    // the sums do not list), under another step, or whose bundle is copied under another name.
    each_named("artifacts.yml", platform_problems_of, PACKAGE_DEFECTS);
}

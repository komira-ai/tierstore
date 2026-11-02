//! The release platforms in the workflows: every platform a release is built for
//! (`xtask::package::Platform::ALL`) has its build job in `artifacts.yml` on a GitHub-hosted
//! runner of its own platform, and every later job, here and in `release.yml`, reads, signs and
//! checks its files; `ci.yml` builds and tests on macOS, the one platform whose build the `test`
//! job (Linux) does not cover.
//!
//! The commands of a build job and of the signing job are pinned whole ([`build_commands`],
//! [`attest_commands`]): a check that looks for a word in a command passes a command that names a
//! wrong target, binary, commit or test selection, so these compare the command lines exactly.
//!
//! [`artifacts_platform_problems`] refuses `artifacts.yml` when:
//! - a platform has no job of its name, or that job runs on another runner than [`BUILDS`] gives,
//!   its `TARGET` is not the platform's Rust target, its `env` holds anything but `TARGET` and
//!   the version, a step of it sets an `env` of its own (which would change the target, the
//!   version or a pin of the commands below without changing their text), its `run:` steps are not exactly
//!   [`build_commands`] in that order (the build, the SBOM, the check of the built binary, the
//!   package, the unpack of the tarball with the runner's own `tar` and the check of the binary
//!   unpacked from it), it uses an action other than checkout, a cache restore that restores
//!   nothing on a push (`lookup-only`), a cache save and the upload, or it uploads another
//!   artifact than `assets-<platform>` from `dist/`;
//! - the cargo-cyclonedx checksum a build job names is not 64 lowercase hex digits in the file's
//!   `env`, or any job or step sets a `CYCLONEDX_` variable of its own (which would shadow the pin);
//! - a job other than `attest` and `verify` has an `if:`, those two have another than the
//!   signing condition, any job or step has `continue-on-error`, or a step that is not a cache
//!   save has an `if:` (a required step that can be skipped or fail unseen is no gate);
//! - `sums` does not need every build job; a download of `sums`, `attest` or `verify` does not
//!   take every platform's `assets-<platform>` into `dist` merged, or takes `sha256sums` or
//!   `bundles` with them; `attest` does not take the sums into `dist`, or `verify` the sums into
//!   `published` and the bundles into `dist`;
//! - `attest` lacks, for any platform, the SBOM attestation of its tarball, the provenance
//!   attestation of its tarball alone, or the copy of that attestation's bundle under the
//!   tarball's name; uses an action other than the download, the two attest actions and the
//!   upload; runs a command other than [`attest_commands`] (nothing built in this repository runs
//!   while it holds `id-token: write`); runs on another runner than ubuntu; or does not upload the
//!   bundles as `bundles`;
//! - any job but `attest` holds `id-token: write`;
//! - the artifacts the jobs upload are not exactly `xtask::release::ARTIFACTS`.
//!
//! [`release_platform_problems`] refuses `release.yml` when its `verify` job does not download
//! every platform's assets merged into `dist`, the sums into `published` and `bundles` into
//! `dist`. [`pull_request_macos_problems`] refuses any workflow a pull request starts that has a
//! job on a macOS runner without [`NOT_FOR_A_PULL_REQUEST`]: no macOS runner is part of a pull
//! request's checks. The macOS build and its tests run in `artifacts.yml` on a push and by hand
//! (`workflow_dispatch`), whose triggers [`TRIGGERS`] pins.

use super::lint::{SIGN_IF, entries, entry, names, runs};
use super::yaml::{self, Node, Value};

/// A platform's build job: its runner (GitHub-hosted, of the platform's own OS and architecture,
/// so the binary runs where it is built), its Rust target, the variable of the file's `env` that
/// pins the SHA-256 of its cargo-cyclonedx, the `if:` of the job (none: it runs for every event),
/// and whether it runs the tests (the platform whose tests no pull-request check runs).
pub struct Build {
    pub platform: &'static str,
    pub runner: &'static str,
    pub target: &'static str,
    pub pin: &'static str,
    pub job_if: Option<&'static str>,
    pub tests: bool,
}

/// The `if:` of a job on a macOS runner in a workflow a pull request starts: never for a pull
/// request.
pub const NOT_FOR_A_PULL_REQUEST: &str = "${{ github.event_name != 'pull_request' }}";

/// The `if:` of `sums`: for a pull request the macOS job is skipped and the Linux files are
/// summed; for a push or a run by hand every build job must have succeeded.
pub const SUMS_IF: &str = "${{ !cancelled() && needs.linux-x86_64.result == 'success' && needs.linux-arm64.result == 'success' && (needs.macos-arm64.result == 'success' || (github.event_name == 'pull_request' && needs.macos-arm64.result == 'skipped')) }}";

/// The triggers of `artifacts.yml`: a pull request, a push to main, and by hand (the macOS build
/// of a branch, tried before it merges).
pub const TRIGGERS: [&str; 3] = ["pull_request", "push", "workflow_dispatch"];

/// The tests the macOS build job runs: every test of the crates the binary is made of, none left
/// out (no `--no-run`, no filter).
pub const MAC_TESTS: &str = "cargo test --locked -p tierstore -p objstore-s3 -p tierstore-testkit --features tierstore/s3 --no-fail-fast";

/// The build job of every release platform.
pub const BUILDS: &[Build] = &[
    Build {
        platform: "linux-x86_64",
        runner: "ubuntu-24.04",
        target: "x86_64-unknown-linux-gnu",
        pin: "CYCLONEDX_SHA256_X86_64_LINUX",
        job_if: None,
        tests: false,
    },
    Build {
        platform: "linux-arm64",
        runner: "ubuntu-24.04-arm",
        target: "aarch64-unknown-linux-gnu",
        pin: "CYCLONEDX_SHA256_AARCH64_LINUX",
        job_if: None,
        tests: false,
    },
    Build {
        platform: "macos-arm64",
        runner: "macos-15",
        target: "aarch64-apple-darwin",
        pin: "CYCLONEDX_SHA256_AARCH64_MACOS",
        job_if: Some(NOT_FOR_A_PULL_REQUEST),
        tests: true,
    },
];

/// The artifact that holds the provenance bundles of every platform's tarball.
const BUNDLES: &str = "bundles";

/// The artifact that holds `SHA256SUMS`.
const SUMS: &str = "sha256sums";

/// The `run:` commands of the build job of `b`, in order.
pub fn build_commands(b: &Build) -> Vec<String> {
    let platform = b.platform;
    let pin = b.pin;
    let mut commands = vec![
        "rustup toolchain install".to_owned(),
        format!(
            "cargo xtask fetch --url \"https://github.com/CycloneDX/cyclonedx-rust-cargo/releases/download/cargo-cyclonedx-$CYCLONEDX_VERSION/cargo-cyclonedx-$TARGET.tar.xz\" --sha256 \"${pin}\" --out \"$HOME/.cargo/bin/cargo-cyclonedx\" --extract tar.xz --member \"cargo-cyclonedx-$TARGET/cargo-cyclonedx\""
        ),
        "cargo cyclonedx --version".to_owned(),
        "cargo build --locked --release --target \"$TARGET\" -p tierstore --features server --bin tierstore".to_owned(),
        "cargo xtask build-commit \"target/$TARGET/release/tierstore\" \"$GITHUB_SHA\" --version \"$TIERSTORE_BUILD_VERSION_OVERRIDE\"".to_owned(),
        "cargo cyclonedx --format json --spec-version 1.5 --describe binaries --target \"$TARGET\"".to_owned(),
        "git diff --exit-code -- Cargo.lock".to_owned(),
        format!(
            "cargo xtask package --binary \"target/$TARGET/release/tierstore\" --sbom crates/tierstore/tierstore_bin.cdx.json --platform {platform} --commit \"$GITHUB_SHA\" --out dist"
        ),
        "mkdir \"$RUNNER_TEMP/unpacked\"".to_owned(),
        format!(
            "tar -xzf \"dist/tierstore-${{GITHUB_SHA}}-{platform}.tar.gz\" -C \"$RUNNER_TEMP/unpacked\" tierstore"
        ),
        "cargo xtask build-commit \"$RUNNER_TEMP/unpacked/tierstore\" \"$GITHUB_SHA\" --version \"$TIERSTORE_BUILD_VERSION_OVERRIDE\"".to_owned(),
    ];
    if b.tests {
        commands.insert(3, MAC_TESTS.to_owned());
    }
    commands
}

/// The `run:` commands of `attest` for `platforms`, in order: the two checks of the sums, then
/// the copy of each platform's bundle.
pub fn attest_commands(platforms: &[&str]) -> Vec<String> {
    let mut commands = vec![
        "/usr/bin/sha256sum --check --strict <<< \"$SUMS_SHA256  dist/SHA256SUMS\"".to_owned(),
        "/usr/bin/sha256sum --check --strict SHA256SUMS".to_owned(),
    ];
    for platform in platforms {
        commands.push(format!(
            "cp \"$BUNDLE\" \"dist/tierstore-${{GITHUB_SHA}}-{platform}.tar.gz.sigstore.json\""
        ));
    }
    commands
}

/// Whether the artifact `name` is taken by the download `pattern`, where `*` stands for any run
/// of characters (the only wildcard these workflows use).
pub fn matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let tail: Vec<&str> = parts.collect();
    let Some((last, middle)) = tail.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

/// The steps of `job`.
fn steps(job: Option<&Node>) -> &[Node] {
    match job.and_then(|j| entry(j, "steps")).map(|s| &s.value) {
        Some(Value::Seq(items)) => items,
        _ => &[],
    }
}

/// The text at `path` (keys, one level each) under `node`.
fn at<'a>(node: &'a Node, path: &[&str]) -> Option<&'a str> {
    let mut here = node;
    for key in path {
        here = entry(here, key)?;
    }
    here.as_str()
}

/// The steps of `job` that use the action `action` (`<owner>/<name>@`).
fn using<'a>(job: Option<&'a Node>, action: &str) -> Vec<&'a Node> {
    steps(job)
        .iter()
        .filter(|s| at(s, &["uses"]).is_some_and(|u| u.starts_with(action)))
        .collect()
}

/// The `run:` commands of `job`, each without its trailing newline.
fn commands(job: Option<&Node>) -> Vec<&str> {
    runs(job).into_iter().map(str::trim_end).collect()
}

/// One line per difference of the commands `got` of `name` from `want`: each one missing, each
/// one that is not wanted, and an order other than `want`'s.
fn command_problems(name: &str, got: &[&str], want: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    for w in want {
        if !got.contains(&w.as_str()) {
            problems.push(format!("{name}: does not run `{w}`"));
        }
    }
    for g in got {
        if !want.iter().any(|w| w == g) {
            problems.push(format!(
                "{name}: runs `{g}`, which is not one of its commands"
            ));
        }
    }
    if problems.is_empty() && got.len() == want.len() && got.iter().zip(want).any(|(g, w)| g != w) {
        problems.push(format!("{name}: runs its commands out of order"));
    }
    if problems.is_empty() && got.len() != want.len() {
        problems.push(format!("{name}: runs a command twice"));
    }
    problems
}

/// One line per action of `job` (named `name`) that is not one of `allowed` (`<owner>/<name>@`).
fn action_problems(name: &str, job: Option<&Node>, allowed: &[&str]) -> Vec<String> {
    steps(job)
        .iter()
        .filter_map(|s| at(s, &["uses"]))
        .filter(|u| !allowed.iter().any(|a| u.starts_with(a)))
        .map(|u| format!("{name}: uses {u}, which is not one of its actions"))
        .collect()
}

/// One line per way `job` (named `name`) can be skipped or fail unseen: a job `if:` other than
/// `job_if`, `continue-on-error` on it or a step, or an `if:` on a step that is not a cache save.
fn execution_problems(name: &str, job: &Node, job_if: Option<&str>) -> Vec<String> {
    let mut problems = Vec::new();
    let condition = at(job, &["if"]);
    if condition != job_if {
        problems.push(format!(
            "{name}: runs under `if: {condition:?}`, not {job_if:?}"
        ));
    }
    if entry(job, "continue-on-error").is_some() {
        problems.push(format!("{name}: has continue-on-error"));
    }
    for step in steps(Some(job)) {
        let label = at(step, &["name"])
            .or_else(|| at(step, &["uses"]))
            .or_else(|| at(step, &["run"]))
            .unwrap_or_default();
        if entry(step, "continue-on-error").is_some() {
            problems.push(format!("{name}: the step `{label}` has continue-on-error"));
        }
        let saves = at(step, &["uses"]).is_some_and(|u| u.starts_with("actions/cache/save@"));
        if entry(step, "if").is_some() && !saves {
            problems.push(format!("{name}: the step `{label}` has an `if:`"));
        }
    }
    problems
}

const DOWNLOAD: &str = "actions/download-artifact@";
const UPLOAD: &str = "actions/upload-artifact@";

/// One line per way the downloads of the job `name` do not take every platform's assets into
/// `dist`, merged, alone; and do not take each artifact of `named` (`(artifact, directory)`)
/// into its directory.
fn download_problems(
    name: &str,
    job: Option<&Node>,
    platforms: &[&str],
    named: &[(&str, &str)],
) -> Vec<String> {
    let downloads = using(job, DOWNLOAD);
    let by_pattern: Vec<&&Node> = downloads
        .iter()
        .filter(|s| at(s, &["with", "pattern"]).is_some())
        .collect();
    let mut problems = Vec::new();
    for platform in platforms {
        let assets = format!("assets-{platform}");
        if !by_pattern
            .iter()
            .any(|s| at(s, &["with", "pattern"]).is_some_and(|p| matches(p, &assets)))
        {
            problems.push(format!("{name}: no download takes {assets}"));
        }
    }
    for step in &by_pattern {
        let pattern = at(step, &["with", "pattern"]).unwrap_or_default();
        for other in [SUMS, BUNDLES] {
            if matches(pattern, other) {
                problems.push(format!(
                    "{name}: the assets download `{pattern}` also takes {other}"
                ));
            }
        }
        if at(step, &["with", "path"]) != Some("dist") {
            problems.push(format!("{name}: the assets download is not into dist"));
        }
        if at(step, &["with", "merge-multiple"]) != Some("true") {
            problems.push(format!(
                "{name}: the assets download does not merge the platforms into one directory"
            ));
        }
    }
    for (artifact, directory) in named {
        let into = downloads
            .iter()
            .find(|s| at(s, &["with", "name"]) == Some(*artifact))
            .and_then(|s| at(s, &["with", "path"]));
        if into != Some(*directory) {
            problems.push(format!(
                "{name}: does not download {artifact} into {directory} (into {into:?})"
            ));
        }
    }
    problems
}

/// Whether `value` is 64 lowercase hex digits.
fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One line per way the build job of `b` (`build`) in `doc` is not the build of its platform.
fn build_problems(doc: &Node, b: &Build, build: &Node) -> Vec<String> {
    let platform = b.platform;
    let mut problems = Vec::new();
    let runner = at(build, &["runs-on"]);
    if runner != Some(b.runner) {
        problems.push(format!(
            "{platform}: runs on {runner:?}, not {:?}, the runner of its platform",
            b.runner
        ));
    }
    let target = at(build, &["env", "TARGET"]);
    if target != Some(b.target) {
        problems.push(format!(
            "{platform}: TARGET is {target:?}, not {:?}, the Rust target of its platform",
            b.target
        ));
    }
    let mut keys: Vec<&str> = entry(build, "env")
        .map_or(&[][..], entries)
        .iter()
        .filter_map(|(k, _)| k.as_str())
        .collect();
    keys.sort_unstable();
    if keys != ["TARGET", "TIERSTORE_BUILD_VERSION_OVERRIDE"] {
        problems.push(format!(
            "{platform}: its env sets {keys:?}, not TARGET and TIERSTORE_BUILD_VERSION_OVERRIDE alone"
        ));
    }
    for step in steps(Some(build)) {
        if let Some(env) = entry(step, "env") {
            let label = at(step, &["name"])
                .or_else(|| at(step, &["run"]))
                .or_else(|| at(step, &["uses"]))
                .unwrap_or_default();
            let set: Vec<&str> = entries(env)
                .iter()
                .filter_map(|(k, _)| k.as_str())
                .collect();
            problems.push(format!(
                "{platform}: the step `{label}` sets its own env {set:?}; a build step takes the job's"
            ));
        }
    }
    problems.extend(command_problems(
        platform,
        &commands(Some(build)),
        &build_commands(b),
    ));
    problems.extend(action_problems(
        platform,
        Some(build),
        &[
            "actions/checkout@",
            "actions/cache/restore@",
            "actions/cache/save@",
            UPLOAD,
        ],
    ));
    let clean = using(Some(build), "actions/cache/restore@")
        .iter()
        .all(|s| at(s, &["with", "lookup-only"]) == Some("${{ github.event_name == 'push' }}"));
    if !clean {
        problems.push(format!(
            "{platform}: restores a cache on a push; the signed binary is built from a clean target"
        ));
    }
    if !at(doc, &["env", b.pin]).is_some_and(is_sha256) {
        problems.push(format!(
            "{platform}: the cargo-cyclonedx pin {} is not 64 lowercase hex digits in the file's env",
            b.pin
        ));
    }
    let uploads: Vec<(Option<&str>, Option<&str>)> = using(Some(build), UPLOAD)
        .iter()
        .map(|s| (at(s, &["with", "name"]), at(s, &["with", "path"])))
        .collect();
    let want = format!("assets-{platform}");
    if uploads != [(Some(want.as_str()), Some("dist/"))] {
        problems.push(format!(
            "{platform}: uploads {uploads:?}, not [\"{want}\"] from dist/"
        ));
    }
    problems
}

/// One line per `CYCLONEDX_` variable a job or a step of `jobs` sets for itself: it would shadow
/// the pin of the file's `env` that the check above reads.
fn shadow_problems(jobs: &[(Node, Node)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or_default();
        let mut scopes = vec![(name.to_owned(), entry(job, "env"))];
        for step in steps(Some(job)) {
            let label = at(step, &["name"])
                .or_else(|| at(step, &["run"]))
                .unwrap_or_default();
            scopes.push((format!("{name}: the step `{label}`"), entry(step, "env")));
        }
        for (scope, env) in scopes {
            for (key, _) in env.map_or(&[][..], entries) {
                let key = key.as_str().unwrap_or_default();
                if key.starts_with("CYCLONEDX_") {
                    problems.push(format!(
                        "{scope}: sets {key}, which shadows the file's cargo-cyclonedx pins"
                    ));
                }
            }
        }
    }
    problems
}

/// One line per way `artifacts.yml` (`text`) does not build, sum, sign and check every one of
/// `platforms`, with `artifacts` the artifacts a release waits for.
pub fn artifacts_platform_problems(
    text: &str,
    platforms: &[&str],
    artifacts: &[&str],
) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    let job = |name: &str| {
        jobs.iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let mut problems = Vec::new();
    let triggers = names(entry(&doc, "on"));
    if triggers != TRIGGERS {
        problems.push(format!(
            "artifacts.yml: triggers are {triggers:?}, not {TRIGGERS:?}"
        ));
    }
    for platform in platforms {
        let Some(b) = BUILDS.iter().find(|b| b.platform == *platform) else {
            problems.push(format!(
                "{platform}: no runner, target or pin for its build"
            ));
            continue;
        };
        match job(platform) {
            Some(build) => problems.extend(build_problems(&doc, b, build)),
            None => problems.push(format!("{platform}: no build job")),
        }
    }
    problems.extend(shadow_problems(jobs));
    for (name, body) in jobs {
        let name = name.as_str().unwrap_or_default();
        let job_if = match name {
            "attest" | "verify" => Some(SIGN_IF),
            "sums" => Some(SUMS_IF),
            _ => BUILDS
                .iter()
                .find(|b| b.platform == name)
                .and_then(|b| b.job_if),
        };
        problems.extend(execution_problems(name, body, job_if));
    }

    let needs = names(job("sums").and_then(|j| entry(j, "needs")));
    for platform in platforms {
        if !needs.iter().any(|n| n == platform) {
            problems.push(format!("sums: does not need {platform}"));
        }
    }
    problems.extend(download_problems("sums", job("sums"), platforms, &[]));
    problems.extend(download_problems(
        "attest",
        job("attest"),
        platforms,
        &[(SUMS, "dist")],
    ));
    problems.extend(download_problems(
        "verify",
        job("verify"),
        platforms,
        &[(SUMS, "published"), (BUNDLES, "dist")],
    ));
    problems.extend(attest_problems(job("attest"), platforms));

    for (name, body) in jobs {
        let name = name.as_str().unwrap_or_default();
        let signs = at(body, &["permissions", "id-token"]) == Some("write");
        if signs != (name == "attest") {
            problems.push(format!(
                "{name}: id-token: write is {}; attest alone holds it",
                if signs { "held" } else { "not held" }
            ));
        }
    }

    let mut uploaded: Vec<&str> = jobs
        .iter()
        .flat_map(|(_, body)| using(Some(body), UPLOAD))
        .filter_map(|s| at(s, &["with", "name"]))
        .collect();
    uploaded.sort_unstable();
    let mut wanted = artifacts.to_vec();
    wanted.sort_unstable();
    if uploaded != wanted {
        problems.push(format!(
            "the jobs upload {uploaded:?}, not the artifacts a release waits for, {wanted:?}"
        ));
    }
    problems
}

/// One line per way `attest` does not sign every one of `platforms`, or does more than sign.
fn attest_problems(attest: Option<&Node>, platforms: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    let sbom_steps = using(attest, "actions/attest-sbom@");
    let provenance_steps = using(attest, "actions/attest-build-provenance@");
    for platform in platforms {
        let stem = format!("tierstore-${{{{ github.sha }}}}-{platform}");
        let tarball = format!("dist/{stem}.tar.gz");
        let sbom = format!("dist/{stem}.cdx.json");
        let attested = sbom_steps.iter().any(|s| {
            at(s, &["with", "subject-path"]) == Some(tarball.as_str())
                && at(s, &["with", "sbom-path"]) == Some(sbom.as_str())
        });
        if !attested {
            problems.push(format!(
                "attest: no SBOM attestation of {tarball} with {sbom}"
            ));
        }
        let id = format!("provenance-tierstore-{platform}");
        let own = provenance_steps.iter().any(|s| {
            at(s, &["id"]) == Some(id.as_str())
                && at(s, &["with", "subject-path"]) == Some(tarball.as_str())
        });
        if !own {
            problems.push(format!(
                "attest: no provenance attestation `{id}` of {tarball} alone"
            ));
        }
        let output = format!("${{{{ steps.{id}.outputs.bundle-path }}}}");
        let copy = format!(
            "cp \"$BUNDLE\" \"dist/tierstore-${{GITHUB_SHA}}-{platform}.tar.gz.sigstore.json\""
        );
        let copied = steps(attest).iter().any(|s| {
            at(s, &["env", "BUNDLE"]) == Some(output.as_str())
                && at(s, &["run"]).is_some_and(|r| r.trim_end() == copy)
        });
        if !copied {
            problems.push(format!(
                "attest: the bundle of {platform} is not copied as `{copy}` from {output}"
            ));
        }
    }
    problems.extend(action_problems(
        "attest",
        attest,
        &[
            DOWNLOAD,
            "actions/attest-build-provenance@",
            "actions/attest-sbom@",
            UPLOAD,
        ],
    ));
    problems.extend(command_problems(
        "attest",
        &commands(attest),
        &attest_commands(platforms),
    ));
    if !attest
        .and_then(|j| at(j, &["runs-on"]))
        .is_some_and(|r| r.starts_with("ubuntu-"))
    {
        problems.push("attest: does not run on an ubuntu runner".to_owned());
    }
    let bundle_uploads: Vec<(Option<&str>, Option<&str>)> = using(attest, UPLOAD)
        .iter()
        .map(|s| (at(s, &["with", "name"]), at(s, &["with", "path"])))
        .collect();
    if bundle_uploads != [(Some(BUNDLES), Some("dist/*.sigstore.json"))] {
        problems.push(format!(
            "attest: uploads {bundle_uploads:?}, not [\"{BUNDLES}\"] from dist/*.sigstore.json"
        ));
    }
    problems
}

/// One line per way the `verify` job of `release.yml` (`text`) does not take every one of
/// `platforms`' assets into `dist` merged, the sums into `published` and the bundles into `dist`.
pub fn release_platform_problems(text: &str, platforms: &[&str]) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let verify = entry(&doc, "jobs").and_then(|j| entry(j, "verify"));
    download_problems(
        "release.yml verify",
        verify,
        platforms,
        &[(SUMS, "published"), (BUNDLES, "dist")],
    )
}

/// One line per job of the workflow `file` (`text`) that a pull request runs on a macOS runner:
/// the workflow has a `pull_request` trigger and the job, on a runner whose name starts with
/// `macos`, has another `if:` than [`NOT_FOR_A_PULL_REQUEST`].
pub fn pull_request_macos_problems(file: &str, text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![format!("{file}: {e}")],
    };
    if !names(entry(&doc, "on")).iter().any(|t| t == "pull_request") {
        return Vec::new();
    }
    entry(&doc, "jobs")
        .map_or(&[][..], entries)
        .iter()
        .filter(|(_, j)| at(j, &["runs-on"]).is_some_and(|r| r.starts_with("macos")))
        .filter(|(_, j)| at(j, &["if"]) != Some(NOT_FOR_A_PULL_REQUEST))
        .map(|(name, _)| {
            format!(
                "{file}: the job {} runs on macOS for a pull request; no macOS runner is part of a pull request's checks",
                name.as_str().unwrap_or_default()
            )
        })
        .collect()
}

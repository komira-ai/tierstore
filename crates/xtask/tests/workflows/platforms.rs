//! The release platforms in the workflows: every platform a release is built for
//! (`xtask::package::Platform::ALL`) has its build job in `artifacts.yml` on a GitHub-hosted
//! runner of its own platform, and every later job reads, signs and checks its files; `ci.yml`
//! builds and tests on macOS, the one platform whose build the `test` job (Linux) does not cover.
//!
//! [`artifacts_platform_problems`] refuses `artifacts.yml` when:
//! - a platform has no job of its name, or that job runs on another runner than [`RUNNERS`]
//!   gives, restores a cache on a push (the signed binary is built from a clean target), installs
//!   cargo-cyclonedx without a SHA-256 the file pins at the top (64 hex digits), builds without
//!   `--target "$TARGET"`, packages under another `--platform`, or uploads another artifact than
//!   `assets-<platform>`;
//! - `sums` does not need every build job, or a download of `sums`, `attest` or `verify` does
//!   not take every platform's `assets-<platform>`, or takes `sha256sums` or `bundles` with them;
//! - `attest` lacks, for any platform, the SBOM attestation of its tarball, the provenance
//!   attestation of its tarball alone, or the copy of that attestation's bundle under the
//!   tarball's name; or `attest` runs repository code (a checkout, a `cargo`) or runs on another
//!   runner than ubuntu; or `attest` does not upload the bundles as `bundles`, or `verify` does
//!   not download them;
//! - any job but `attest` holds `id-token: write`;
//! - the artifacts the jobs upload are not exactly `xtask::release::ARTIFACTS`, the ones a
//!   release waits for.
//!
//! [`ci_platform_problems`] refuses `ci.yml` without a job on `macos-15` that builds the release
//! binary and runs the tests.

use super::lint::{entries, entry, names, runs};
use super::yaml::{self, Node, Value};

/// The runner of each platform's build job: GitHub-hosted, of the platform's own OS and
/// architecture, so that the binary runs where it is built.
pub const RUNNERS: &[(&str, &str)] = &[
    ("linux-x86_64", "ubuntu-24.04"),
    ("linux-arm64", "ubuntu-24.04-arm"),
    ("macos-arm64", "macos-15"),
];

/// The artifact that holds the provenance bundles of every platform's tarball.
const BUNDLES: &str = "bundles";

/// The artifact that holds `SHA256SUMS`.
const SUMS: &str = "sha256sums";

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

const DOWNLOAD: &str = "actions/download-artifact@";
const UPLOAD: &str = "actions/upload-artifact@";

/// One line per way the downloads of the job `name` miss a platform's assets or take the sums
/// or the bundles with them.
fn download_problems(name: &str, job: Option<&Node>, platforms: &[&str]) -> Vec<String> {
    let patterns: Vec<&str> = using(job, DOWNLOAD)
        .iter()
        .filter_map(|s| at(s, &["with", "pattern"]))
        .collect();
    let mut problems = Vec::new();
    for platform in platforms {
        let assets = format!("assets-{platform}");
        if !patterns.iter().any(|p| matches(p, &assets)) {
            problems.push(format!("{name}: no download takes {assets}"));
        }
    }
    for other in [SUMS, BUNDLES] {
        if let Some(p) = patterns.iter().find(|p| matches(p, other)) {
            problems.push(format!(
                "{name}: the assets download `{p}` also takes {other}"
            ));
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
    for platform in platforms {
        let build = job(platform);
        if build.is_none() {
            problems.push(format!("{platform}: no build job"));
            continue;
        }
        let want = RUNNERS.iter().find(|(p, _)| p == platform).map(|(_, r)| *r);
        let runner = build.and_then(|j| at(j, &["runs-on"]));
        if runner != want {
            problems.push(format!(
                "{platform}: runs on {runner:?}, not {want:?}, the runner of its platform"
            ));
        }
        let clean = using(build, "actions/cache/restore@")
            .iter()
            .all(|s| at(s, &["with", "lookup-only"]) == Some("${{ github.event_name == 'push' }}"));
        if !clean {
            problems.push(format!(
                "{platform}: restores a cache on a push; the signed binary is built from a clean target"
            ));
        }
        let pinned = runs(build)
            .into_iter()
            .filter(|r| r.contains("cargo xtask fetch") && r.contains("cargo-cyclonedx"))
            .filter_map(|r| r.split("--sha256 \"$").nth(1)?.split('"').next())
            .any(|var| {
                at(&doc, &["env", var]).is_some_and(|v| {
                    v.len() == 64 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                })
            });
        if !pinned {
            problems.push(format!(
                "{platform}: cargo-cyclonedx is not fetched against a SHA-256 pinned in the file's env"
            ));
        }
        let built = runs(build).into_iter().any(|r| {
            r.contains("cargo build --locked --release --target \"$TARGET\" -p tierstore --features server --bin tierstore")
        });
        if !built {
            problems.push(format!(
                "{platform}: no step builds tierstore for --target \"$TARGET\""
            ));
        }
        let packaged = runs(build).into_iter().any(|r| {
            r.contains("cargo xtask package") && r.contains(&format!("--platform {platform} "))
        });
        if !packaged {
            problems.push(format!(
                "{platform}: no step packages with --platform {platform}"
            ));
        }
        let uploads: Vec<&str> = using(build, UPLOAD)
            .iter()
            .filter_map(|s| at(s, &["with", "name"]))
            .collect();
        if uploads != [format!("assets-{platform}").as_str()] {
            problems.push(format!(
                "{platform}: uploads {uploads:?}, not [\"assets-{platform}\"]"
            ));
        }
    }

    let sums = job("sums");
    let needs = names(sums.and_then(|j| entry(j, "needs")));
    for platform in platforms {
        if !needs.iter().any(|n| n == platform) {
            problems.push(format!("sums: does not need {platform}"));
        }
    }
    for name in ["sums", "attest", "verify"] {
        problems.extend(download_problems(name, job(name), platforms));
    }

    let attest = job("attest");
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
    if !attest
        .and_then(|j| at(j, &["runs-on"]))
        .is_some_and(|r| r.starts_with("ubuntu-"))
    {
        problems.push("attest: does not run on an ubuntu runner".to_owned());
    }
    if !using(attest, "actions/checkout@").is_empty()
        || runs(attest).iter().any(|r| r.contains("cargo"))
    {
        problems.push("attest: runs repository code while it holds id-token: write".to_owned());
    }
    let bundle_uploads: Vec<&str> = using(attest, UPLOAD)
        .iter()
        .filter_map(|s| at(s, &["with", "name"]))
        .collect();
    if bundle_uploads != [BUNDLES] {
        problems.push(format!(
            "attest: uploads {bundle_uploads:?}, not [\"{BUNDLES}\"]"
        ));
    }
    let verify_downloads: Vec<&str> = using(job("verify"), DOWNLOAD)
        .iter()
        .filter_map(|s| at(s, &["with", "name"]))
        .collect();
    if !verify_downloads.contains(&BUNDLES) {
        problems.push(format!("verify: does not download {BUNDLES}"));
    }

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

/// One line per way `ci.yml` (`text`) does not build the release binary and run the tests on
/// macos-15.
pub fn ci_platform_problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    let mac = jobs
        .iter()
        .find(|(_, j)| at(j, &["runs-on"]) == Some("macos-15"))
        .map(|(_, j)| j);
    let Some(mac) = mac else {
        return vec!["ci.yml: no job runs on macos-15".to_owned()];
    };
    let mut problems = Vec::new();
    let ran = runs(Some(mac));
    if !ran.iter().any(|r| {
        r.contains("cargo build --locked --release -p tierstore --features server --bin tierstore")
    }) {
        problems.push("ci.yml: the macos-15 job does not build the release binary".to_owned());
    }
    if !ran
        .iter()
        .any(|r| r.contains("cargo test --locked -p tierstore "))
    {
        problems.push("ci.yml: the macos-15 job does not run the tests of tierstore".to_owned());
    }
    problems
}

//! `xtask release desired --macos-pkg` (and `--skip-superseded`, as the deploy of release.yml runs
//! it) end to end: the real `xtask` binary runs with the stand-in `gh` (src/bin/gh-fake.rs) as both
//! `--gh` and `--curl`, answered from files under the test's scratch directory, and the lines it
//! appends to `--output` are read.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "example/tierstore";
const VERSION: &str = "1.2.3";
const FAKE: &str = env!("CARGO_BIN_EXE_gh-fake");
const PKG: &[u8] = b"bytes of the signed pkg";

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn put(dir: &Path, path: &str, bytes: &[u8]) {
    let file = dir.join(path);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("parents");
    std::fs::write(file, bytes).expect("write");
}

/// A scratch directory that holds a release `v1.2.3` whose `SHA256SUMS` lists both `.deb`s and
/// the `.pkg`, and a testing suite that lists both `.deb`s with those digests.
fn prepared(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-desired-mac-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("dir");
    put(
        &d,
        &format!("repos/{REPO}/releases/tags/v{VERSION}"),
        format!(
            "{{\"tag_name\":\"v{VERSION}\",\"target_commitish\":\"{COMMIT}\",\"draft\":false,\"prerelease\":false,\"assets\":[{{\"name\":\"SHA256SUMS\",\"size\":1}}]}}"
        )
        .as_bytes(),
    );
    let pkg = format!("tierstore-{VERSION}-macos-arm64.pkg");
    let mut sums = format!("{}  {pkg}\n", sha(PKG));
    for arch in ["amd64", "arm64"] {
        let deb = format!("tierstore_{VERSION}_{arch}.deb");
        let digest = sha(deb.as_bytes());
        sums.push_str(&format!("{digest}  {deb}\n"));
        put(
            &d,
            &format!("curl-replies/apt.example.org/dists/testing/main/binary-{arch}/Packages"),
            format!(
                "Package: tierstore\nVersion: {VERSION}\nArchitecture: {arch}\nFilename: pool/tierstore/{deb}\nSize: 1\nSHA256: {digest}\n\n"
            )
            .as_bytes(),
        );
    }
    put(
        &d,
        &format!("release-assets/v{VERSION}/SHA256SUMS"),
        sums.as_bytes(),
    );
    put(&d, &format!("release-assets/v{VERSION}/{pkg}"), PKG);
    d
}

fn desired(d: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .current_dir(d)
        .args(["release", "desired", "--repo", REPO, "--version", VERSION])
        .args(["--min-hostkeeper", "0.1.225", "--dir", "desired"])
        .args(["--apt-url", "https://apt.example.org"])
        .args(["--output", "github-output", "--gh", FAKE, "--curl", FAKE])
        .args(extra)
        .output()
        .expect("run xtask")
}

/// The JSON of the `key=` line of the `--output` file.
fn line(d: &Path, key: &str) -> Option<Value> {
    let text = std::fs::read_to_string(d.join("github-output")).expect("output");
    let json = text
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))?;
    Some(serde_json::from_str(json).expect("json"))
}

#[test]
fn release_desired_with_macos_pkg_writes_one_targets_with_the_apt_lists_and_the_pkg_entry() {
    // Asserts: the flag reaches the verb; the one `targets` line carries the apt lists the verb
    // writes without the flag, unchanged, plus a `macos-arm64` entry of installer `macos-pkg`
    // with the version, the sha256 and the size of the downloaded `.pkg` and the pkg manifest;
    // and there is no second targets line for the Macs, so every host group is set the same
    // targets and the coordinator never refuses the second set as `release_exists`.
    let d = prepared("with");
    let o = desired(&d, &["--macos-pkg"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let text = std::fs::read_to_string(d.join("github-output")).expect("output");
    assert_eq!(text.lines().count(), 3, "{text}");
    let mac = line(&d, "targets").expect("targets");
    let apt_only = prepared("with-apt-only");
    let o = desired(&apt_only, &[]);
    assert_eq!(o.status.code(), Some(0));
    let linux = line(&apt_only, "targets").expect("targets");
    assert_eq!(mac["linux-x86_64"], linux["linux-x86_64"]);
    assert_eq!(mac["linux-arm64"], linux["linux-arm64"]);
    assert_eq!(mac.as_object().expect("object").len(), 3, "{mac}");
    let entries = mac["macos-arm64"]["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["installer"], "macos-pkg");
    assert_eq!(entry["repository"], "komira-testing");
    assert_eq!(entry["packageVersion"], VERSION);
    assert_eq!(entry["digest"], sha(PKG));
    assert_eq!(entry["bytes"], PKG.len().to_string());
    assert_eq!(entry["manifest"]["package"], "com.komira.tierstore");
    assert_eq!(entry["manifest"]["run"], serde_json::json!(["tierstore"]));
    assert_eq!(entry["manifest"]["minHostkeeper"], "0.1.225");
    assert_eq!(
        entry["manifest"]["params"]["service_manager"]["text"],
        "launchd"
    );
    assert_eq!(
        std::fs::read(d.join(format!("desired/tierstore-{VERSION}-macos-arm64.pkg"))).expect("pkg"),
        PKG
    );
}

#[test]
fn release_desired_without_macos_pkg_writes_no_macos_list_and_asks_for_no_pkg() {
    // Asserts: without the flag the output has `set`, `package-version` and `targets`, the
    // targets have no `macos-arm64` list, and the `.pkg` is not downloaded.
    let d = prepared("without");
    let o = desired(&d, &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let text = std::fs::read_to_string(d.join("github-output")).expect("output");
    assert!(text.contains("set=true\n"), "{text}");
    assert!(text.contains("package-version=1.2.3\n"), "{text}");
    assert_eq!(text.lines().count(), 3, "{text}");
    let targets = line(&d, "targets").expect("targets");
    assert!(targets.get("macos-arm64").is_none(), "{targets}");
    assert!(
        !d.join(format!("desired/tierstore-{VERSION}-macos-arm64.pkg"))
            .exists()
    );
}

/// `prepared(name)` with the list of published releases holding `tags`.
fn listing(name: &str, tags: &[&str]) -> PathBuf {
    let d = prepared(name);
    let releases: Vec<String> = tags
        .iter()
        .map(|t| format!("{{\"tag_name\":\"{t}\",\"draft\":false,\"prerelease\":false}}"))
        .collect();
    put(
        &d,
        &format!("repos/{REPO}/releases?per_page=100"),
        format!("[{}]", releases.join(",")).as_bytes(),
    );
    d
}

#[test]
fn release_desired_with_skip_superseded_sets_nothing_when_a_newer_release_is_published() {
    // Catches: `--skip-superseded` not reaching the verb from the command line, so a deploy that
    // waited behind a newer release sets the older one; a superseded release failing the job
    // instead of handing on `set=false`; and the suite or the `.pkg` being read for a release that
    // is not set.
    let d = listing("superseded", &["v1.2.3", "v1.2.4", "v1.2.2"]);
    let o = desired(&d, &["--skip-superseded", "--macos-pkg"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        "v1.2.3 is superseded by v1.2.4, the newest published release of example/tierstore: nothing is set\nset=false\n"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("github-output")).expect("output"),
        "set=false\n"
    );
    assert!(!d.join("desired").exists());
}

#[test]
fn release_desired_with_skip_superseded_sets_the_newest_release() {
    // Catches: `--skip-superseded` refusing the newest published release itself, which would
    // leave every release undeployed.
    let d = listing("newest", &["v1.2.2", "v1.2.3"]);
    let o = desired(&d, &["--skip-superseded", "--macos-pkg"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let text = std::fs::read_to_string(d.join("github-output")).expect("output");
    assert!(
        text.starts_with("set=true\npackage-version=1.2.3\n"),
        "{text}"
    );
    let targets = line(&d, "targets").expect("targets");
    assert!(targets.get("macos-arm64").is_some(), "{text}");
}

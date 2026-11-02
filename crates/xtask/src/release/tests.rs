use std::cell::RefCell;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::publish::{is_release_tag, notes};
use super::*;
use crate::testdir::TestDir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "example/tierstore";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-release-unit"), name)
}

/// What GitHub is made to answer, and what was asked of `gh`.
#[derive(Default)]
struct FakeGh {
    /// `(path, answer)`: a path that is not here answers 404.
    api: Vec<(String, Value)>,
    /// The releases of the repository, as `repos/<repo>/releases` lists them.
    releases: RefCell<Vec<Value>>,
    /// Every `gh <args>` run.
    ran: RefCell<Vec<Vec<String>>>,
    /// A file `release create` silently does not attach.
    drops: Option<String>,
}

impl Gh for FakeGh {
    fn api(&self, path: &str) -> Result<Option<Value>, Error> {
        if path == format!("repos/{REPO}/releases?per_page=100") {
            return Ok(Some(Value::Array(self.releases.borrow().clone())));
        }
        Ok(self
            .api
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, v)| v.clone()))
    }

    fn run(&self, args: &[String]) -> Result<String, Error> {
        self.ran.borrow_mut().push(args.to_vec());
        let verb = (args[0].as_str(), args[1].as_str());
        let tag = args[2].clone();
        match verb {
            ("release", "create") => {
                let assets: Vec<Value> = args
                    .iter()
                    .skip_while(|a| *a != "--draft")
                    .skip(1)
                    .map(|p| {
                        PathBuf::from(p)
                            .file_name()
                            .expect("name")
                            .to_string_lossy()
                            .into_owned()
                    })
                    .filter(|n| Some(n) != self.drops.as_ref())
                    .map(|n| json!({ "name": n }))
                    .collect();
                self.releases
                    .borrow_mut()
                    .push(json!({ "tag_name": tag, "draft": true, "assets": assets }));
            }
            ("release", "delete") => self
                .releases
                .borrow_mut()
                .retain(|r| r["tag_name"] != tag.as_str()),
            ("release", "edit") => {
                for r in self.releases.borrow_mut().iter_mut() {
                    if r["tag_name"] == tag.as_str() {
                        r["draft"] = json!(false);
                    }
                }
            }
            _ => panic!("unexpected gh {args:?}"),
        }
        Ok(String::new())
    }
}

/// The in-toto statement bundle of a provenance attestation over one tarball.
fn bundle(sha: &Sha256) -> String {
    let statement = json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": "asset", "digest": { "sha256": sha.to_string() } }],
        "predicateType": bundles::PROVENANCE,
        "predicate": {},
    });
    let bundle = json!({
        "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
        "verificationMaterial": {},
        "dsseEnvelope": {
            "payloadType": "application/vnd.in-toto+json",
            "payload": STANDARD.encode(statement.to_string()),
            "signatures": [],
        },
    });
    format!("{bundle}\n")
}

/// `dir` holding the assets of `COMMIT`, their sums and a bundle per tarball.
fn assets(dir: &Path) {
    assets_of(dir, &expected_assets(COMMIT));
}

/// `dir` holding the files `names` (assets of any commit), a `SHA256SUMS` that lists exactly
/// them and a bundle per tarball among them: a directory that passes `verify-assets sums`
/// whatever the set is, so that `check` is what judges the set.
fn assets_of(dir: &Path, names: &[String]) {
    let mut sums = String::new();
    let mut names = names.to_vec();
    names.sort();
    for name in &names {
        let bytes = format!("bytes of {name}");
        std::fs::write(dir.join(name), &bytes).expect("asset");
        let sha = Sha256::of(bytes.as_bytes());
        sums.push_str(&format!("{sha}  {name}\n"));
        if name.ends_with(".tar.gz") {
            std::fs::write(dir.join(bundles::name_of(name)), bundle(&sha)).expect("bundle");
        }
    }
    std::fs::write(dir.join(SUMS), sums).expect("sums");
}

fn refused(result: Result<Vec<String>, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(problems)) => problems,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_assets_of_a_commit_are_a_tarball_and_an_sbom_per_platform() {
    // Catches: an asset name that drifts from `<binary>-<commit>-<platform>`, which the host
    // installer pins, or a platform left out.
    assert_eq!(
        expected_assets(COMMIT),
        [
            format!("tierstore-{COMMIT}-linux-arm64.cdx.json"),
            format!("tierstore-{COMMIT}-linux-arm64.tar.gz"),
            format!("tierstore-{COMMIT}-linux-x86_64.cdx.json"),
            format!("tierstore-{COMMIT}-linux-x86_64.tar.gz"),
            format!("tierstore-{COMMIT}-macos-arm64.cdx.json"),
            format!("tierstore-{COMMIT}-macos-arm64.tar.gz"),
        ]
    );
}

#[test]
fn a_release_without_the_macos_arm64_tarball_or_sbom_or_with_an_extra_file_is_refused() {
    // Catches: a release check that passes a dist directory built for the Linux platforms
    // alone (the macos-arm64 job's files never downloaded), one that lacks only the Mac SBOM,
    // and one that holds a tarball of a platform that is not released, each with sums that
    // match their files, so the check of the set, not of the digests, is what refuses them.
    let mac = format!("tierstore-{COMMIT}-macos-arm64.tar.gz");
    let mac_sbom = format!("tierstore-{COMMIT}-macos-arm64.cdx.json");
    let all = expected_assets(COMMIT);
    let without = |gone: &[&String]| -> Vec<String> {
        all.iter().filter(|n| !gone.contains(n)).cloned().collect()
    };

    let dir = scratch("check-no-macos");
    assets_of(&dir, &without(&[&mac, &mac_sbom]));
    assert_eq!(
        refused(check(&dir, COMMIT)),
        [
            format!("{SUMS} does not list {mac_sbom}"),
            format!("{SUMS} does not list {mac}"),
        ]
    );

    let dir = scratch("check-no-macos-tarball");
    assets_of(&dir, &without(&[&mac]));
    assert_eq!(
        refused(check(&dir, COMMIT)),
        [format!("{SUMS} does not list {mac}")]
    );

    let dir = scratch("check-no-macos-sbom");
    assets_of(&dir, &without(&[&mac_sbom]));
    assert_eq!(
        refused(check(&dir, COMMIT)),
        [format!("{SUMS} does not list {mac_sbom}")]
    );

    let extra = format!("tierstore-{COMMIT}-macos-x86_64.tar.gz");
    let dir = scratch("check-unlisted-platform");
    let mut more = all.clone();
    more.push(extra.clone());
    assets_of(&dir, &more);
    assert_eq!(
        refused(check(&dir, COMMIT)),
        [format!(
            "{SUMS} lists {extra}, which is not an asset of {COMMIT}"
        )]
    );

    let dir = scratch("check-no-macos-bundle");
    assets(&dir);
    std::fs::remove_file(dir.join(bundles::name_of(&mac))).expect("rm");
    assert_eq!(
        refused(check(&dir, COMMIT)),
        [format!("{mac}: no provenance bundle {mac}.sigstore.json")]
    );
}

#[test]
fn the_build_run_artifacts_are_each_platforms_assets_the_sums_and_one_bundles() {
    // Catches: a platform whose files a release does not wait for (locate would pass a run
    // that lost them, and the release would fail later or ship without them), and the bundle
    // artifact still under a name of the Linux platforms alone.
    let mut want: Vec<String> = Platform::ALL
        .iter()
        .map(|p| format!("assets-{}", p.name()))
        .collect();
    want.extend(["sha256sums".to_owned(), "bundles".to_owned()]);
    assert_eq!(ARTIFACTS.to_vec(), want);
}

#[test]
fn a_complete_directory_passes_the_check() {
    // Catches: the check refusing the set it is made for.
    let dir = scratch("check-ok");
    assets(&dir);
    let printed = check(&dir, COMMIT).expect("check");
    assert!(
        printed.last().is_some_and(|l| l.contains("6 files")),
        "{printed:?}"
    );
}

#[test]
fn a_directory_that_differs_from_the_set_of_the_commit_is_refused() {
    // Catches: a release of another commit's files, a missing platform, an extra file, a
    // tarball without its bundle, and a bundle under another tarball's name.
    let arm = format!("tierstore-{COMMIT}-linux-arm64.tar.gz");
    let x86 = format!("tierstore-{COMMIT}-linux-x86_64.tar.gz");
    let other = "fedcba9876543210fedcba9876543210fedcba98";

    let dir = scratch("check-other-commit");
    assets(&dir);
    let problems = refused(check(&dir, other));
    assert!(
        problems
            .iter()
            .any(|p| p.contains("does not list") && p.contains(other)),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("which is not an asset of")),
        "{problems:?}"
    );

    let dir = scratch("check-no-bundle");
    assets(&dir);
    std::fs::remove_file(dir.join(bundles::name_of(&arm))).expect("rm");
    let problems = refused(check(&dir, COMMIT));
    assert_eq!(
        problems,
        [format!("{arm}: no provenance bundle {arm}.sigstore.json")]
    );

    let dir = scratch("check-swapped-bundle");
    assets(&dir);
    std::fs::copy(
        dir.join(bundles::name_of(&x86)),
        dir.join(bundles::name_of(&arm)),
    )
    .expect("copy");
    let problems = refused(check(&dir, COMMIT));
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("subject digest"), "{problems:?}");

    let dir = scratch("check-extra-file");
    assets(&dir);
    std::fs::write(dir.join("notes.txt"), b"x").expect("extra");
    assert!(matches!(check(&dir, COMMIT), Err(Error::Refused(_))));

    let dir = scratch("check-short-sha");
    assets(&dir);
    assert!(matches!(check(&dir, "0123456789ab"), Err(Error::Usage(_))));
}

/// The answers of a repository whose `COMMIT` is on main with both runs green and every
/// artifact kept, under the run ids 11 (the build) and 22 (the tests).
fn good_repo() -> FakeGh {
    let run = |id: u64| {
        json!({ "workflow_runs": [{
            "id": id, "head_sha": COMMIT, "head_branch": "main",
            "event": "push", "conclusion": "success",
        }]})
    };
    let runs = |workflow: &str| {
        format!(
            "repos/{REPO}/actions/workflows/{workflow}/runs?head_sha={COMMIT}&event=push&branch=main&status=success&per_page=20"
        )
    };
    let artifacts: Vec<Value> = ARTIFACTS
        .iter()
        .map(|n| json!({ "name": n, "expired": false }))
        .collect();
    FakeGh {
        api: vec![
            (
                format!("repos/{REPO}/compare/main...{COMMIT}"),
                json!({ "status": "behind" }),
            ),
            (runs("artifacts.yml"), run(11)),
            (runs("ci.yml"), run(22)),
            (
                format!("repos/{REPO}/actions/runs/11/artifacts?per_page=100"),
                json!({ "artifacts": artifacts }),
            ),
        ],
        ..FakeGh::default()
    }
}

#[test]
fn locate_names_the_build_run_of_a_commit_on_main_whose_tests_passed() {
    // Catches: a locate that refuses the good case, or does not hand the run id on.
    let dir = scratch("locate-ok");
    let output = dir.join("out");
    let printed = locate(&good_repo(), REPO, COMMIT, Some(&output)).expect("locate");
    assert_eq!(printed.last().map(String::as_str), Some("run-id=11"));
    assert_eq!(
        std::fs::read_to_string(&output).expect("out"),
        "run-id=11\n"
    );
}

#[test]
fn locate_refuses_what_a_release_must_not_be_cut_from() {
    // Catches: each way a commit can fail to be releasable being accepted: off main (the
    // compare says `ahead` or `diverged`, or GitHub does not know it), no green build run, no
    // green test run, a run of another commit, and an expired artifact.
    for status in ["ahead", "diverged"] {
        let mut gh = good_repo();
        gh.api[0].1 = json!({ "status": status });
        let e = locate(&gh, REPO, COMMIT, None).expect_err(status);
        assert!(e.to_string().contains("is not on main"), "{e}");
    }
    let mut gh = good_repo();
    gh.api.remove(0);
    assert!(locate(&gh, REPO, COMMIT, None).is_err(), "unknown commit");

    let mut gh = good_repo();
    gh.api.retain(|(p, _)| !p.contains("ci.yml"));
    let problems = refused(locate(&gh, REPO, COMMIT, None));
    assert_eq!(
        problems,
        [format!(
            "no successful push run of ci.yml on main for {COMMIT}"
        )]
    );

    let mut gh = good_repo();
    gh.api.retain(|(p, _)| !p.contains("artifacts.yml"));
    gh.api.retain(|(p, _)| !p.contains("ci.yml"));
    let problems = refused(locate(&gh, REPO, COMMIT, None));
    assert_eq!(problems.len(), 2, "{problems:?}");

    let mut gh = good_repo();
    gh.api[1].1["workflow_runs"][0]["head_sha"] = json!("ffffffffffffffffffffffffffffffffffffffff");
    let problems = refused(locate(&gh, REPO, COMMIT, None));
    assert!(problems[0].contains("artifacts.yml"), "{problems:?}");

    let mut gh = good_repo();
    gh.api[1].1["workflow_runs"][0]["event"] = json!("pull_request");
    assert!(locate(&gh, REPO, COMMIT, None).is_err(), "not a push run");

    let mut gh = good_repo();
    gh.api[3].1["artifacts"][4]["expired"] = json!(true);
    let problems = refused(locate(&gh, REPO, COMMIT, None));
    assert_eq!(problems, ["run 11 holds no unexpired artifact bundles"]);

    let mut gh = good_repo();
    gh.api[3].1["artifacts"][2]["expired"] = json!(true);
    let problems = refused(locate(&gh, REPO, COMMIT, None));
    assert_eq!(
        problems,
        ["run 11 holds no unexpired artifact assets-macos-arm64"]
    );

    let mut gh = good_repo();
    gh.api[3].1["artifacts"] = json!([]);
    assert_eq!(
        refused(locate(&gh, REPO, COMMIT, None)).len(),
        ARTIFACTS.len()
    );

    assert!(matches!(
        locate(&good_repo(), REPO, "main", None),
        Err(Error::Usage(_))
    ));
}

#[test]
fn a_release_tag_is_v_and_three_numbers() {
    // Catches: a tag that is not a version (a branch name, a prerelease, a leading zero)
    // accepted for a release.
    for good in ["v0.2.1", "v1.0.0", "v10.20.30"] {
        assert!(is_release_tag(good), "{good}");
    }
    for bad in [
        "",
        "v",
        "0.2.1",
        "v1.2",
        "v1.2.3.4",
        "v01.2.3",
        "v1.2.x",
        "v1.2.3-rc1",
        "main",
        "v1..3",
    ] {
        assert!(!is_release_tag(bad), "{bad}");
    }
}

#[test]
fn the_notes_name_the_commit_the_run_the_signer_and_the_extra_paragraphs() {
    // Catches: notes that leave out where the files came from or how to verify them, or drop
    // the paragraph a maintainer passed (the first release says where tierstore came from).
    let text = notes(
        REPO,
        "v0.3.0",
        COMMIT,
        11,
        &["Moved from another repository.".to_owned()],
    );
    for part in [
        "tierstore v0.3.0, built from commit 0123456789abcdef0123456789abcdef01234567 by run 11",
        "Moved from another repository.",
        "--signer-workflow example/tierstore/.github/workflows/artifacts.yml",
        "--source-ref refs/heads/main --source-digest 0123456789abcdef0123456789abcdef01234567 --deny-self-hosted-runners\n```",
        "SHA256SUMS",
    ] {
        assert!(text.contains(part), "{part}\n{text}");
    }
}

fn publish_in(gh: &FakeGh, dir: &Path, tag: &str) -> Result<Vec<String>, Error> {
    // A file beside the directory, named after it: not one of the files the directory holds.
    let notes_file = dir.with_extension("notes.md");
    publish(gh, dir, REPO, tag, COMMIT, 11, &notes_file, &[])
}

#[test]
fn publish_creates_a_draft_with_every_file_reads_it_back_and_publishes_it() {
    // Catches: a release that is published before its assets are read back, one that misses a
    // file, and a target other than the commit.
    let dir = scratch("publish-ok");
    assets(&dir);
    let gh = FakeGh::default();
    let printed = publish_in(&gh, &dir, "v0.3.0").expect("publish");
    assert!(
        printed
            .last()
            .is_some_and(|l| l.contains("published v0.3.0")),
        "{printed:?}"
    );
    let ran = gh.ran.borrow();
    assert_eq!(ran.len(), 2, "{ran:?}");
    let create = &ran[0];
    assert!(
        create.windows(2).any(|w| w == ["--target", COMMIT]),
        "{create:?}"
    );
    assert!(create.contains(&"--draft".to_owned()));
    // 6 assets (a tarball and an SBOM per platform) + 3 bundles + the sums.
    assert_eq!(
        create
            .iter()
            .filter(|a| Path::new(a).starts_with(&*dir))
            .count(),
        10
    );
    assert_eq!(ran[1][..3], ["release", "edit", "v0.3.0"]);
    assert!(ran[1].contains(&"--draft=false".to_owned()));
    assert_eq!(gh.releases.borrow()[0]["draft"], false);
}

#[test]
fn publish_refuses_a_release_or_a_tag_that_exists_and_a_bad_tag() {
    // Catches: a second release for a tag, a release over a tag someone made, and a tag that
    // is not a version.
    let dir = scratch("publish-exists");
    assets(&dir);

    let gh = FakeGh::default();
    gh.releases
        .borrow_mut()
        .push(json!({ "tag_name": "v0.3.0", "draft": false, "assets": [] }));
    let e = publish_in(&gh, &dir, "v0.3.0").expect_err("release exists");
    assert!(e.to_string().contains("release v0.3.0 exists"), "{e}");
    assert!(gh.ran.borrow().is_empty());

    let gh = FakeGh {
        api: vec![(
            format!("repos/{REPO}/git/ref/tags/v0.3.0"),
            json!({ "ref": "refs/tags/v0.3.0" }),
        )],
        ..FakeGh::default()
    };
    let e = publish_in(&gh, &dir, "v0.3.0").expect_err("tag exists");
    assert!(e.to_string().contains("tag v0.3.0 exists already"), "{e}");
    assert!(gh.ran.borrow().is_empty());

    let gh = FakeGh::default();
    assert!(matches!(
        publish_in(&gh, &dir, "latest"),
        Err(Error::Usage(_))
    ));
    assert!(gh.ran.borrow().is_empty());
}

#[test]
fn publish_replaces_a_draft_a_failed_run_left() {
    // Catches: a re-run that fails on its own earlier draft, or adds files to it.
    let dir = scratch("publish-redraft");
    assets(&dir);
    let gh = FakeGh::default();
    gh.releases
        .borrow_mut()
        .push(json!({ "tag_name": "v0.3.0", "draft": true, "assets": [{ "name": "stale" }] }));
    let printed = publish_in(&gh, &dir, "v0.3.0").expect("publish");
    assert!(
        printed
            .iter()
            .any(|l| l.contains("removed the draft v0.3.0")),
        "{printed:?}"
    );
    let ran = gh.ran.borrow();
    assert_eq!(ran[0][..3], ["release", "delete", "v0.3.0"]);
    assert_eq!(gh.releases.borrow().len(), 1);
}

#[test]
fn publish_deletes_a_draft_that_lacks_a_file_and_publishes_nothing() {
    // Catches: a release published with an asset missing (the upload dropped one).
    let dir = scratch("publish-short");
    assets(&dir);
    let gh = FakeGh {
        drops: Some(SUMS.to_owned()),
        ..FakeGh::default()
    };
    let e = publish_in(&gh, &dir, "v0.3.0").expect_err("short draft");
    assert!(e.to_string().contains("it was deleted"), "{e}");
    assert!(gh.releases.borrow().is_empty(), "the draft was deleted");
    assert!(
        gh.ran.borrow().iter().all(|a| a[1] != "edit"),
        "never published"
    );
}

#[test]
fn publish_checks_the_directory_before_it_asks_github_anything() {
    // Catches: a release made from a directory that fails the check.
    let dir = scratch("publish-bad-dir");
    assets(&dir);
    std::fs::remove_file(dir.join(SUMS)).expect("rm");
    let gh = FakeGh::default();
    assert!(publish_in(&gh, &dir, "v0.3.0").is_err());
    assert!(gh.ran.borrow().is_empty());
}

#[test]
fn a_reply_that_is_not_a_list_is_an_empty_list() {
    // Catches: a missing or malformed reply from GitHub read as holding releases or runs.
    assert!(array(&None).is_empty());
    assert!(array(&Some(json!({ "a": 1 }))).is_empty());
    assert_eq!(array(&Some(json!([1, 2]))).len(), 2);
}

#[test]
fn locate_refuses_a_run_of_another_branch_or_one_that_did_not_succeed() {
    // Catches: a run on a branch other than main, or that failed, standing in for the build
    // of the commit (GitHub's query filters both, and the verb does not rely on it alone).
    for (key, value) in [("head_branch", "feature"), ("conclusion", "failure")] {
        let mut gh = good_repo();
        gh.api[1].1["workflow_runs"][0][key] = json!(value);
        let problems = refused(locate(&gh, REPO, COMMIT, None));
        assert!(problems[0].contains("artifacts.yml"), "{key}: {problems:?}");
    }
    let mut gh = good_repo();
    gh.api[1].1 = json!({ "workflow_runs": "none" });
    assert!(locate(&gh, REPO, COMMIT, None).is_err());
}

#[test]
fn publish_leaves_the_releases_of_other_tags_alone() {
    // Catches: a published release of another tag, or a draft of one, taken for this tag's:
    // neither blocks a publish, and neither is deleted.
    let dir = scratch("publish-others");
    assets(&dir);
    let gh = FakeGh::default();
    gh.releases
        .borrow_mut()
        .push(json!({ "tag_name": "v0.2.0", "draft": false, "assets": [] }));
    gh.releases
        .borrow_mut()
        .push(json!({ "tag_name": "v0.2.1", "draft": true, "assets": [] }));
    gh.releases
        .borrow_mut()
        .push(json!({ "tag_name": "v0.3.0", "draft": true }));
    let printed = publish_in(&gh, &dir, "v0.3.0").expect("publish");
    assert!(
        printed
            .last()
            .is_some_and(|l| l.contains("published v0.3.0"))
    );
    let kept: Vec<String> = gh
        .releases
        .borrow()
        .iter()
        .map(|r| r["tag_name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(kept, ["v0.2.0", "v0.2.1", "v0.3.0"]);
}

#[test]
fn locate_without_an_output_file_only_prints() {
    // Catches: a locate that needs `--output`, and one that prints the run id wrongly without.
    let printed = locate(&good_repo(), REPO, COMMIT, None).expect("locate");
    assert_eq!(printed.last().map(String::as_str), Some("run-id=11"));
}

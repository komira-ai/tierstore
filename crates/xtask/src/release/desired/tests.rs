use std::cell::RefCell;

use serde_json::{Value, json};

use super::*;
use crate::channels::apt::index::sha256_hex;
use crate::channels::apt::package_names;
use crate::channels::aws::Answer;
use crate::fetch::Sha256;
use crate::sums::{Line, SUMS, render};
use crate::testdir::TestDir;

const REPO: &str = "example/tierstore";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const VERSION: &str = "0.4.0";
const TAG: &str = "v0.4.0";
/// The run of `release.yml` of the tests of `--after-run`.
const RUN: &str = "4242";
/// The hostkeeper floor of every entry: the lab's, `release.yml`'s `--min-hostkeeper`.
const FLOOR: &str = "0.1.225";
const URL: &str = "https://packages.komira.ai/apt";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside.parent().expect("deps").join("xtask-desired-unit"),
        name,
    )
}

/// The bytes of the `.deb` named `name` in the fake release.
fn deb_bytes(name: &str) -> Vec<u8> {
    format!("bytes of {name}").into_bytes()
}

/// The architecture a package file is for (`tierstore_<version>_<arch>.deb`).
fn arch_of(name: &str) -> &str {
    name.trim_end_matches(".deb")
        .rsplit('_')
        .next()
        .unwrap_or_default()
}

/// The `SHA256SUMS` of the release of `version`: a line for each `.deb`.
fn sums_of(version: &str) -> Vec<u8> {
    let lines: Vec<Line> = package_names(version)
        .expect("names")
        .into_iter()
        .map(|n| Line {
            sha256: Sha256::of(&deb_bytes(&n)),
            name: n,
        })
        .collect();
    render(&lines).into_bytes()
}

/// The release `tag` as GitHub's API answers for it, with `SHA256SUMS` among its assets.
fn release_of(tag: &str) -> Value {
    json!({
        "tag_name": tag,
        "target_commitish": COMMIT,
        "draft": false,
        "prerelease": false,
        "assets": [{ "name": SUMS, "size": 10 }],
    })
}

/// What GitHub is made to answer, and what was asked of it.
struct FakeGh {
    /// The answer for each `gh api` path; a path not listed is a 404.
    answers: Vec<(String, Value)>,
    /// The bytes `gh release download` writes as `SHA256SUMS`.
    sums: Vec<u8>,
    /// Every `gh api` path asked.
    asked: RefCell<Vec<String>>,
    /// Every `gh <args>` run.
    ran: RefCell<Vec<Vec<String>>>,
}

impl Gh for FakeGh {
    fn api(&self, path: &str) -> Result<Option<Value>, Error> {
        self.asked.borrow_mut().push(path.to_owned());
        Ok(self
            .answers
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, v)| v.clone()))
    }

    fn run(&self, args: &[String]) -> Result<String, Error> {
        self.ran.borrow_mut().push(args.to_vec());
        std::fs::write(Path::new(&args[6]).join(SUMS), &self.sums).expect("write");
        Ok(String::new())
    }
}

/// The path of the release `tag` in GitHub's API.
fn tag_path(tag: &str) -> String {
    format!("repos/{REPO}/releases/tags/{tag}")
}

/// GitHub with the published release [`TAG`].
fn good() -> FakeGh {
    released(VERSION)
}

/// GitHub with the published release `v<version>`.
fn released(version: &str) -> FakeGh {
    let tag = format!("v{version}");
    FakeGh {
        answers: vec![(tag_path(&tag), release_of(&tag))],
        sums: sums_of(version),
        asked: RefCell::new(Vec::new()),
        ran: RefCell::new(Vec::new()),
    }
}

/// `gh` that answers for the jobs of [`RUN`] with `jobs` and lists `releases`.
fn after(mut gh: FakeGh, jobs: Value, releases: Value) -> FakeGh {
    let count = jobs.as_array().map_or(0, Vec::len);
    gh.answers.push((
        format!("repos/{REPO}/actions/runs/{RUN}/jobs?per_page=100"),
        json!({ "total_count": count, "jobs": jobs }),
    ));
    gh.answers
        .push((format!("repos/{REPO}/releases?per_page=100"), releases));
    gh
}

/// A published release of the list of releases.
fn listed(tag: &str) -> Value {
    json!({ "tag_name": tag, "draft": false, "prerelease": false })
}

/// The jobs of a release run whose `publish` ended with `conclusion`.
fn run_jobs(conclusion: &str) -> Value {
    json!([
        { "name": "plan", "conclusion": "success" },
        { "name": "publish", "conclusion": conclusion },
        { "name": "promote-check", "conclusion": "success" },
    ])
}

/// The testing suite as the CDN serves it: the `Packages` text per URL, or a refusal.
struct Cdn {
    packages: Vec<(String, Result<String, String>)>,
    asked: RefCell<Vec<String>>,
}

impl Host for Cdn {
    fn run(&self, _program: &Path, args: &[String]) -> Result<Answer, Error> {
        let url = args.last().expect("a url").clone();
        self.asked.borrow_mut().push(url.clone());
        let at = args.iter().position(|a| a == "--output").expect("--output");
        match self.packages.iter().find(|(u, _)| *u == url) {
            Some((_, Ok(text))) => {
                std::fs::write(&args[at + 1], text).expect("write");
                Ok(Answer {
                    ok: true,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            Some((_, Err(why))) => Ok(Answer {
                ok: false,
                stdout: String::new(),
                stderr: format!("{why}\n"),
            }),
            None => panic!("unexpected url {url}"),
        }
    }
}

/// The `Packages` stanza of the testing suite listing `name` at `version` with `digest`.
fn stanza(name: &str, version: &str, digest: &str) -> String {
    let arch = arch_of(name);
    format!(
        "Package: tierstore\nVersion: {version}\nArchitecture: {arch}\nFilename: pool/tierstore/{name}\nSize: 1\nSHA256: {digest}\n\n"
    )
}

/// The testing suite listing the `.deb`s of each of `versions`, each with the digest of its bytes.
fn testing(versions: &[&str]) -> Cdn {
    let mut text: Vec<(String, String)> = Vec::new();
    for version in versions {
        for name in package_names(version).expect("names") {
            let url = format!(
                "{URL}/dists/testing/main/binary-{}/Packages",
                arch_of(&name)
            );
            let entry = stanza(&name, version, &sha256_hex(&deb_bytes(&name)));
            match text.iter_mut().find(|(u, _)| *u == url) {
                Some((_, t)) => t.push_str(&entry),
                None => text.push((url, entry)),
            }
        }
    }
    Cdn {
        packages: text.into_iter().map(|(u, t)| (u, Ok(t))).collect(),
        asked: RefCell::new(Vec::new()),
    }
}

/// A request of `--version version`, into no directory yet.
fn by_version(version: &'static str) -> Request<'static> {
    Request {
        repo: REPO,
        version,
        after_run: "",
        min_hostkeeper: FLOOR,
        dir: Path::new(""),
        apt_url: URL,
        curl: Path::new("curl"),
        output: None,
    }
}

/// `request` with `dir` and `output`.
fn at<'a>(request: Request<'a>, dir: &'a Path, output: Option<&'a Path>) -> Request<'a> {
    Request {
        dir,
        output,
        ..request
    }
}

/// The lines of a refusal.
fn refused<T: std::fmt::Debug>(result: Result<T, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(lines)) => lines,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The message of a failure.
fn failed<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    match result {
        Err(Error::Failed(text)) => text,
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// The message of a usage error.
fn usage<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    match result {
        Err(Error::Usage(text)) => text,
        other => panic!("expected a usage error, got {other:?}"),
    }
}

/// The `apt` entry the coordinator is given for the `.deb` of `arch` of the release `version`.
fn apt_entry(version: &str, arch: &str) -> Value {
    let digest = Sha256::of(&deb_bytes(&format!("tierstore_{version}_{arch}.deb"))).to_string();
    desired_apt::entry(version, &digest, FLOOR)
}

#[test]
fn a_release_by_version_is_set_as_its_plain_version_with_one_apt_entry_per_linux_platform() {
    // Catches: the archive-only targets the apt hosts cannot install (every host held
    // installer_dropped and nothing updates), the package version `X.Y.Z+<commit>` that no apt
    // host runs, a release index handed on beside the apt targets, a repository entry other than
    // the farm nodes' komira-testing, a macOS list (no package a Mac's daemon installs), and
    // targets other than those the lab stage of release.yml sets for the same version, which the
    // coordinator would refuse as release_exists.
    let s = scratch("by-version");
    let output = s.join("output");
    let gh = good();
    let cdn = testing(&[VERSION]);
    let dir = s.join("d");
    let printed = desired(&gh, &cdn, &at(by_version(VERSION), &dir, Some(&output))).expect("set");
    let written = std::fs::read_to_string(&output).expect("the output");
    let lines: Vec<&str> = written.lines().collect();
    assert_eq!(lines.len(), 3, "{written}");
    assert_eq!(lines[..2], ["set=true", "package-version=0.4.0"]);
    let targets: Value =
        serde_json::from_str(lines[2].strip_prefix("targets=").expect("targets")).expect("JSON");
    assert_eq!(
        targets,
        json!({
            "linux-x86_64": { "entries": [apt_entry(VERSION, "amd64")] },
            "linux-arm64": { "entries": [apt_entry(VERSION, "arm64")] },
        })
    );
    for platform in ["linux-x86_64", "linux-arm64"] {
        let entry = &targets[platform]["entries"][0];
        assert_eq!(entry["installer"], "apt", "{platform}");
        assert_eq!(entry["repository"], "komira-testing", "{platform}");
        assert_eq!(entry["packageVersion"], VERSION, "{platform}");
    }
    assert!(!written.contains("release-index"), "{written}");
    assert_eq!(
        printed,
        vec![
            format!(
                "release {TAG} ({COMMIT}): the testing suite lists tierstore {VERSION} for each of 2 architectures, each with the digest SHA256SUMS lists"
            ),
            "set=true".to_owned(),
            "package-version=0.4.0".to_owned(),
            lines[2].to_owned(),
        ]
    );
    // The lab's verb, given the same release, floor and suite, writes the same two lines.
    let lab_output = s.join("lab-output");
    desired_apt::desired_apt(
        &good(),
        &testing(&[VERSION]),
        &desired_apt::Request {
            curl: Path::new("curl"),
            repo: REPO,
            version: VERSION,
            min_hostkeeper: FLOOR,
            dir: &s.join("lab"),
            apt_url: URL,
        },
        Some(&lab_output),
    )
    .expect("the lab's targets");
    assert_eq!(
        std::fs::read_to_string(&lab_output).expect("the lab's output"),
        lines[1..].join("\n") + "\n"
    );
    // The suite of each architecture was read, and the release's sums alone downloaded.
    assert_eq!(
        cdn.asked.borrow().as_slice(),
        [
            format!("{URL}/dists/testing/main/binary-amd64/Packages"),
            format!("{URL}/dists/testing/main/binary-arm64/Packages"),
        ]
    );
    assert_eq!(gh.ran.borrow().len(), 1);
    assert_eq!(gh.ran.borrow()[0][8], SUMS);
    // Without --output the same lines are only printed.
    let again = desired(
        &good(),
        &testing(&[VERSION]),
        &at(by_version(VERSION), &dir, None),
    )
    .expect("set");
    assert_eq!(again, printed);
}

#[test]
fn after_a_run_that_published_the_newest_published_release_is_set() {
    // Catches: a release run that published one setting nothing, or setting an older release, a
    // draft, a pre-release or a tag that is not vX.Y.Z; and the run's jobs not being asked.
    let s = scratch("after-run");
    let output = s.join("output");
    let releases = json!([
        { "tag_name": "v0.5.0", "draft": true, "prerelease": false },
        { "tag_name": "v0.6.0", "draft": false, "prerelease": true },
        { "tag_name": "nightly", "draft": false, "prerelease": false },
        { "tag_name": "v0.9", "draft": false, "prerelease": false },
        listed(TAG),
        listed("v0.3.12"),
    ]);
    let gh = after(good(), run_jobs("success"), releases);
    let dir = s.join("d");
    let request = Request {
        after_run: RUN,
        ..at(by_version(""), &dir, Some(&output))
    };
    let printed = desired(&gh, &testing(&["0.3.12", VERSION]), &request).expect("set");
    assert_eq!(printed[1..3], ["set=true", "package-version=0.4.0"]);
    assert_eq!(
        gh.asked.borrow().as_slice(),
        [
            format!("repos/{REPO}/actions/runs/{RUN}/jobs?per_page=100"),
            format!("repos/{REPO}/releases?per_page=100"),
            tag_path(TAG),
        ]
    );
    assert!(
        std::fs::read_to_string(&output)
            .expect("the output")
            .starts_with("set=true\npackage-version=0.4.0\ntargets=")
    );
}

#[test]
fn a_rollback_by_hand_sets_the_older_release_the_testing_suite_still_lists() {
    // Catches: a rollback that sets the newest release instead of the one named, or that pins the
    // digest of another version's .deb from the same Packages file.
    let s = scratch("rollback");
    let output = s.join("output");
    let dir = s.join("d");
    let older = "0.3.12";
    desired(
        &released(older),
        &testing(&[older, VERSION]),
        &at(by_version("0.3.12"), &dir, Some(&output)),
    )
    .expect("set");
    let written = std::fs::read_to_string(&output).expect("the output");
    let lines: Vec<&str> = written.lines().collect();
    assert_eq!(lines[..2], ["set=true", "package-version=0.3.12"]);
    let targets: Value =
        serde_json::from_str(lines[2].strip_prefix("targets=").expect("targets")).expect("JSON");
    assert_eq!(
        targets,
        json!({
            "linux-x86_64": { "entries": [apt_entry(older, "amd64")] },
            "linux-arm64": { "entries": [apt_entry(older, "arm64")] },
        })
    );
}

#[test]
fn the_newest_release_is_the_highest_version_by_its_numbers() {
    // Catches: versions compared as text, where 0.9.0 is above 0.10.0, and a list with no
    // published vX.Y.Z read as some release.
    let gh = after(
        good(),
        run_jobs("success"),
        json!([
            listed("v0.9.0"),
            listed("v0.10.0"),
            listed("v0.10.1"),
            listed("v0.2.0")
        ]),
    );
    assert_eq!(newest(&gh, REPO).expect("one"), "0.10.1");
    for releases in [json!([]), json!([listed("latest")]), json!({})] {
        let gh = after(good(), run_jobs("success"), releases.clone());
        assert_eq!(
            failed(newest(&gh, REPO)),
            "example/tierstore has no published release vX.Y.Z",
            "{releases}"
        );
    }
}

#[test]
fn after_a_run_that_published_nothing_nothing_is_set_and_nothing_is_read() {
    // Catches: a release run that planned no release (its publish skipped), whose publish failed
    // or was cancelled, or that has no publish job, setting the newest release again (it would
    // undo a rollback by hand after every merge).
    for jobs in [
        run_jobs("skipped"),
        run_jobs("failure"),
        run_jobs("cancelled"),
        json!([{ "name": "plan", "conclusion": "success" }]),
        json!([{ "name": "publish-check", "conclusion": "success" }]),
    ] {
        let s = scratch("after-run-none");
        let output = s.join("output");
        let dir = s.join("d");
        let gh = after(good(), jobs.clone(), json!([listed(TAG)]));
        let cdn = testing(&[VERSION]);
        let request = Request {
            after_run: RUN,
            ..at(by_version(""), &dir, Some(&output))
        };
        assert_eq!(
            desired(&gh, &cdn, &request).expect("passes"),
            vec![
                format!(
                    "run {RUN} of {REPO} did not publish a release (publish did not succeed): nothing is set"
                ),
                "set=false".to_owned(),
            ],
            "{jobs}"
        );
        assert_eq!(
            std::fs::read_to_string(&output).expect("the output"),
            "set=false\n"
        );
        assert!(gh.ran.borrow().is_empty(), "{jobs}");
        assert!(cdn.asked.borrow().is_empty(), "{jobs}");
        assert!(!dir.exists(), "{jobs}");
        assert_eq!(gh.asked.borrow().len(), 1, "{jobs}");
        // Without --output nothing is written.
        let request = Request {
            output: None,
            ..request
        };
        assert_eq!(
            desired(&gh, &cdn, &request).expect("passes")[1],
            "set=false"
        );
    }
}

#[test]
fn a_run_github_does_not_know_or_lists_in_more_than_one_page_is_refused() {
    // Catches: a run id of another repository, and a run whose publish job is on a page that
    // was not read, being taken for a run that published nothing.
    let s = scratch("after-run-bad");
    let dir = s.join("d");
    let request = Request {
        after_run: RUN,
        ..at(by_version(""), &dir, None)
    };
    let cdn = testing(&[VERSION]);
    let gh = good();
    assert_eq!(
        failed(desired(&gh, &cdn, &request)),
        "example/tierstore has no run 4242"
    );
    let mut gh = after(good(), run_jobs("success"), json!([listed(TAG)]));
    gh.answers[1].1["total_count"] = json!(101);
    assert_eq!(
        failed(desired(&gh, &cdn, &request)),
        "run 4242 of example/tierstore has 101 jobs; one page of 3 was read"
    );
    gh.answers[1].1 = json!({ "jobs": run_jobs("success") });
    assert_eq!(
        failed(desired(&gh, &cdn, &request)),
        "run 4242 of example/tierstore has null jobs; one page of 3 was read"
    );
    assert!(gh.ran.borrow().is_empty());
    assert!(cdn.asked.borrow().is_empty());
}

#[test]
fn a_request_without_exactly_one_way_or_with_a_malformed_flag_is_a_usage_error() {
    // Catches: a run by hand that also names a run (or a run after a release that also names a
    // version) picking one silently, neither picking anything, a run id that is not a number
    // reaching GitHub's API path, a tag (with its v), a package version, a short or padded
    // version reaching GitHub, where it could name another release than the operator meant; and a
    // floor or an apt URL out of their form passing on a run after a release that published
    // nothing, where the entries that would carry them are never built.
    let s = scratch("usage");
    let dir = s.join("d");
    let gh = good();
    let cdn = testing(&[VERSION]);
    let both = Request {
        after_run: RUN,
        ..at(by_version(VERSION), &dir, None)
    };
    let neither = at(by_version(""), &dir, None);
    for request in [both, neither] {
        assert_eq!(
            usage(desired(&gh, &cdn, &request)),
            "give one of --version and --after-run, not both and not neither"
        );
    }
    for run in ["42/../1", "-1", "4242 "] {
        let request = Request {
            after_run: run,
            ..neither
        };
        assert_eq!(
            usage(desired(&gh, &cdn, &request)),
            format!("--after-run {run:?} is not the id of a workflow run")
        );
    }
    for version in ["v0.4.0", "0.4", "0.4.0+0123456789ab", "0.04.0", "0.4.0.1"] {
        assert_eq!(
            usage(desired(&gh, &cdn, &at(by_version(version), &dir, None))),
            format!(
                "--version {version:?} is not <major>.<minor>.<patch>, a release tag without its v"
            )
        );
    }
    let after_run = Request {
        after_run: RUN,
        ..neither
    };
    for floor in ["", "v0.1.225", "0.1", "0.1.225+0123456789ab"] {
        let request = Request {
            min_hostkeeper: floor,
            ..after_run
        };
        assert_eq!(
            usage(desired(&gh, &cdn, &request)),
            format!("--min-hostkeeper {floor:?} is not <major>.<minor>.<patch>")
        );
    }
    for url in [
        "http://packages.komira.ai/apt",
        "https://packages.komira.ai/apt/",
        "",
    ] {
        let request = Request {
            apt_url: url,
            ..after_run
        };
        assert_eq!(
            usage(desired(&gh, &cdn, &request)),
            format!("--apt-url {url:?} is not an https URL without a trailing /")
        );
    }
    assert!(gh.asked.borrow().is_empty());
    assert!(gh.ran.borrow().is_empty());
    assert!(cdn.asked.borrow().is_empty());
}

#[test]
fn a_release_the_testing_suite_does_not_list_with_its_digests_is_refused_and_nothing_handed_on() {
    // Catches: a version set whose .deb the farm nodes cannot install from komira-testing (the
    // suite lists none, or another build under that name), and an output written for a refused
    // release, which the next job would set.
    let s = scratch("not-in-testing");
    let dir = s.join("d");
    let output = s.join("output");
    let request = at(by_version(VERSION), &dir, Some(&output));
    let lines = refused(desired(&good(), &testing(&["0.3.12"]), &request));
    assert_eq!(
        lines,
        vec![
            "the testing suite lists no tierstore_0.4.0_amd64.deb: the release is not published there",
            "the testing suite lists no tierstore_0.4.0_arm64.deb: the release is not published there",
        ]
    );
    assert!(!output.exists());
    let mut other = testing(&[VERSION]);
    let amd64 = format!("{URL}/dists/testing/main/binary-amd64/Packages");
    let wrong = "ab".repeat(32);
    other.packages[0] = (
        amd64,
        Ok(stanza("tierstore_0.4.0_amd64.deb", VERSION, &wrong)),
    );
    let lines = refused(desired(&good(), &other, &request));
    assert_eq!(
        lines,
        vec![format!(
            "the testing suite lists tierstore_0.4.0_amd64.deb with sha256 {wrong}, but SHA256SUMS lists {}",
            Sha256::of(&deb_bytes("tierstore_0.4.0_amd64.deb"))
        )]
    );
    assert!(!output.exists());
}

#[test]
fn a_version_with_no_published_release_or_no_sums_is_refused_before_the_suite_is_read() {
    // Catches: a version that was never released, a draft, a pre-release, a release made at a
    // branch name, or one without the digests the suite is held to, being set as desired.
    let s = scratch("none");
    let dir = s.join("d");
    let request = at(by_version(VERSION), &dir, None);
    let cdn = testing(&[VERSION]);
    let gh = FakeGh {
        answers: Vec::new(),
        ..good()
    };
    assert_eq!(
        failed(desired(&gh, &cdn, &request)),
        "example/tierstore has no release v0.4.0: --version names a released version of tierstore"
    );
    for (key, value, says) in [
        (
            "draft",
            json!(true),
            "is a draft or a pre-release, not a released version",
        ),
        (
            "prerelease",
            json!(true),
            "is a draft or a pre-release, not a released version",
        ),
        (
            "target_commitish",
            json!("main"),
            "names \"main\" as its commit, not a full commit SHA",
        ),
        // Forty characters, but not lower-case hex.
        (
            "target_commitish",
            json!(COMMIT.to_ascii_uppercase()),
            "as its commit, not a full commit SHA",
        ),
        (
            "target_commitish",
            json!(null),
            "names \"\" as its commit, not a full commit SHA",
        ),
    ] {
        let mut gh = good();
        gh.answers[0].1[key] = value.clone();
        let message = failed(desired(&gh, &cdn, &request));
        assert!(
            message.starts_with("release v0.4.0 of example/tierstore ") && message.ends_with(says),
            "{key} {value}: {message}"
        );
        assert!(gh.ran.borrow().is_empty(), "{key} {value}");
    }
    let mut gh = good();
    gh.answers[0].1["assets"] = json!([]);
    assert_eq!(
        refused(desired(&gh, &cdn, &request)),
        vec!["release v0.4.0 has no SHA256SUMS: there are no digests to hold the testing suite to"]
    );
    assert!(gh.ran.borrow().is_empty());
    assert!(cdn.asked.borrow().is_empty());
}

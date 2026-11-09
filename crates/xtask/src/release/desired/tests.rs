use std::cell::RefCell;
use std::ops::Deref;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hostkeeper_config::daemon::Sha256Hex;
use hostkeeper_config::index::PackageEntry;
use hostkeeper_config::names::{ServiceName, Version};
use serde_json::{Value, json};

use super::*;
use crate::release::hostkeeper_package::archive as package;
use crate::sums::{Line, render};
use crate::testdir::TestDir;

const REPO: &str = "example/tierstore";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const VERSION: &str = "0.4.0";
const TAG: &str = "v0.4.0";
/// The version of the hostkeeper packages of release [`TAG`] at [`COMMIT`].
const PACKAGES: &str = "0.4.0+0123456789ab";
/// The run of `release.yml` of the tests of `--after-run`.
const RUN: &str = "4242";

/// Gives `path` and every directory below it mode 0700: hostkeeper's unpacker leaves an unpacked
/// version read-only (0555), which `remove_dir_all` cannot remove.
fn open_up(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            open_up(&entry.path());
        }
    }
}

/// An empty directory of its own for one test, opened up when it drops, with its subdirectory
/// `d` for the release's files.
struct Scratch(TestDir, PathBuf);

impl Scratch {
    /// `<scratch>/d`.
    fn d(&self) -> &Path {
        &self.1
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        open_up(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let beside = std::env::current_exe().expect("test binary");
    let base = beside.parent().expect("deps").join("xtask-desired-unit");
    // A run that was killed left read-only directories that the scratch cannot replace.
    open_up(&base.join(name));
    let dir = TestDir::under(base, name);
    let d = dir.join("d");
    Scratch(dir, d)
}

/// The package of `platform` as `version`, with a stand-in binary, needing hostkeeper 0.0.9.
fn archive_of(platform: Platform, version: &str) -> Vec<u8> {
    package(platform, version, "0.0.9", b"stand-in tierstore\0").expect("the package")
}

/// The provenance bundle whose one subject has the SHA-256 `sha`.
fn bundle_of(sha: &Sha256) -> Vec<u8> {
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
    format!("{bundle}\n").into_bytes()
}

/// The index entry of the package `bytes` of `platform`.
fn entry_of(platform: Platform, bytes: &[u8]) -> PackageEntry {
    PackageEntry {
        platform: host_platform(platform),
        sha256: Sha256Hex::parse(&Sha256::of(bytes).to_string()).expect("a digest"),
        bytes: bytes.len() as u64,
    }
}

/// The canonical index of `service` at `version` listing the packages `packages`.
fn index_of(service: &str, version: &str, packages: &[(Platform, Vec<u8>)]) -> String {
    ReleaseIndex::new(
        ServiceName::new(service).expect("a service"),
        Version::new(version).expect("a version"),
        packages.iter().map(|(p, b)| entry_of(*p, b)).collect(),
    )
    .expect("an index")
    .to_json()
}

/// The good package of each of `platforms`.
fn packages(platforms: &[Platform]) -> Vec<(Platform, Vec<u8>)> {
    platforms
        .iter()
        .map(|p| (*p, archive_of(*p, PACKAGES)))
        .collect()
}

/// The `SHA256SUMS` of a release whose index is `index`.
fn sums_of(index: &[u8]) -> Vec<u8> {
    render(&[Line {
        sha256: Sha256::of(index),
        name: INDEX.to_owned(),
    }])
    .into_bytes()
}

/// The release as GitHub's API answers for it, with `files` as its assets.
fn release_of(files: &[(String, Vec<u8>)]) -> Value {
    let assets: Vec<Value> = files
        .iter()
        .map(|(name, bytes)| json!({ "name": name, "size": bytes.len() }))
        .collect();
    json!({
        "tag_name": TAG,
        "target_commitish": COMMIT,
        "draft": false,
        "prerelease": false,
        "assets": assets,
    })
}

/// What GitHub is made to answer, and what was asked of it.
struct FakeGh {
    /// The answer for each `gh api` path; a path not listed is a 404.
    answers: Vec<(String, Value)>,
    /// The files of the release: what `gh release download` writes of the patterns it is given.
    files: Vec<(String, Vec<u8>)>,
    /// What `gh release download` fails with, if it fails.
    fails: Option<String>,
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
        if let Some(why) = &self.fails {
            return Err(Error::Failed(why.clone()));
        }
        let dir = Path::new(&args[6]);
        let wanted: Vec<&String> = args.iter().skip(8).step_by(2).collect();
        for (name, bytes) in &self.files {
            if wanted.contains(&name) {
                std::fs::write(dir.join(name), bytes).expect("write a downloaded file");
            }
        }
        Ok(String::new())
    }
}

/// The path of the release [`TAG`] in GitHub's API.
fn tag_path() -> String {
    format!("repos/{REPO}/releases/tags/{TAG}")
}

/// A release of `index` (its bytes) with the `SHA256SUMS` that lists it, the packages
/// `packages` and a bundle of each.
fn released(index: &[u8], packages: &[(Platform, Vec<u8>)]) -> FakeGh {
    let mut files = vec![
        (INDEX.to_owned(), index.to_vec()),
        (SUMS.to_owned(), sums_of(index)),
    ];
    for (platform, bytes) in packages {
        let name = file_name(PACKAGES, *platform);
        files.push((bundles::name_of(&name), bundle_of(&Sha256::of(bytes))));
        files.push((name, bytes.clone()));
    }
    FakeGh {
        answers: vec![(tag_path(), release_of(&files))],
        files,
        fails: None,
        asked: RefCell::new(Vec::new()),
        ran: RefCell::new(Vec::new()),
    }
}

/// A release whose index lists all three platforms.
fn good() -> FakeGh {
    let packages = packages(&Platform::ALL);
    released(
        index_of("tierstore", PACKAGES, &packages).as_bytes(),
        &packages,
    )
}

/// `gh`'s release, edited by `edit`.
fn with_release(mut gh: FakeGh, edit: impl Fn(&mut Value)) -> FakeGh {
    let at = gh
        .answers
        .iter_mut()
        .find(|(p, _)| *p == tag_path())
        .expect("a release");
    edit(&mut at.1);
    gh
}

/// `gh` with the file `name` of the release replaced by `bytes` (its asset keeps its size).
fn with_file(mut gh: FakeGh, name: &str, bytes: Vec<u8>) -> FakeGh {
    let at = gh
        .files
        .iter_mut()
        .find(|(n, _)| n == name)
        .expect("a file");
    at.1 = bytes;
    gh
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

/// A request of `--version version`, into no directory yet.
fn by_version(version: &'static str) -> Request<'static> {
    Request {
        repo: REPO,
        version,
        after_run: "",
        dir: Path::new(""),
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

fn no_package_line(platform: &str) -> String {
    format!(
        "index.json: the release index has no package for {platform} (no_package_for_platform): a host of {platform} would be ordered nothing, so it is not set"
    )
}

/// The download of `names` of release [`TAG`] into `dir`.
fn download_of(dir: &Path, names: &[String]) -> Vec<String> {
    let mut args: Vec<String> = ["release", "download", TAG, "--repo", REPO, "--dir"]
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
    args.push(dir.display().to_string());
    for name in names {
        args.push("--pattern".to_owned());
        args.push(name.clone());
    }
    args
}

/// The targets of the packages `packages` (needing hostkeeper 0.0.9), written out by hand.
fn targets_of(packages: &[(Platform, Vec<u8>)]) -> String {
    let mut sorted: Vec<&(Platform, Vec<u8>)> = packages.iter().collect();
    sorted.sort_by_key(|(p, _)| p.name());
    let lists: Vec<String> = sorted
        .iter()
        .map(|(platform, bytes)| {
            let name = file_name(PACKAGES, *platform);
            let bundle = bundle_of(&Sha256::of(bytes));
            let channel = format!("tierstore/{PACKAGES}/{name}");
            let github = format!("{REPO}/releases/download/{TAG}/{name}");
            format!(
                "\"{p}\":{{\"entries\":[{{\"attestation\":{{\"bytes\":\"{bb}\",\"digest\":\"{bd}\",\"locations\":[{{\"locator\":\"{channel}.sigstore.json\",\"store\":\"channel\"}},{{\"locator\":\"{github}.sigstore.json\",\"store\":\"github\"}}]}},\"bytes\":\"{ab}\",\"digest\":\"{ad}\",\"installer\":\"hostkeeper-archive\",\"locations\":[{{\"locator\":\"{channel}\",\"store\":\"channel\"}},{{\"locator\":\"{github}\",\"store\":\"github\"}}],\"manifest\":{{\"minHostkeeper\":\"0.0.9\"}}}}]}}",
                p = platform.name(),
                bb = bundle.len(),
                bd = Sha256::of(&bundle),
                ab = bytes.len(),
                ad = Sha256::of(bytes),
            )
        })
        .collect();
    format!("{{{}}}", lists.join(","))
}

#[test]
fn a_release_of_three_platforms_hands_on_its_version_its_index_and_its_targets() {
    // Catches: a version handed to the coordinator that is not the one the packages carry (the
    // tag alone, or another commit's), an index or digest that is not index.json's, targets that
    // name another archive, bundle, location or floor than the release's, and the downloads
    // asking for other files or another release.
    let s = scratch("good");
    let dir = s.join("desired");
    let output = s.join("output");
    let gh = good();
    let packages = packages(&Platform::ALL);
    let index = index_of("tierstore", PACKAGES, &packages);
    let digest = Sha256::of(index.as_bytes());
    let lines = [
        "set=true".to_owned(),
        format!("package-version={PACKAGES}"),
        format!("release-index={index}"),
        format!("release-index-digest={digest}"),
        format!("targets={}", targets_of(&packages)),
    ];
    let printed = desired(&gh, &at(by_version(VERSION), &dir, Some(&output))).expect("passes");
    let mut expected = vec![format!(
        "release v0.4.0 ({COMMIT}): index.json lists tierstore {PACKAGES} for each of 3 platforms; each package is the one it lists, attested, and unpacks as a host unpacks it"
    )];
    expected.extend(lines.iter().cloned());
    assert_eq!(printed, expected);
    assert_eq!(
        std::fs::read_to_string(&output).expect("the output"),
        format!("{}\n", lines.join("\n"))
    );
    let names: Vec<String> = Platform::ALL
        .iter()
        .map(|p| file_name(PACKAGES, *p))
        .flat_map(|n| [bundles::name_of(&n), n])
        .collect();
    assert_eq!(
        *gh.ran.borrow(),
        vec![
            download_of(&dir, &[INDEX.to_owned(), SUMS.to_owned()]),
            download_of(&dir, &names),
        ]
    );
    assert_eq!(*gh.asked.borrow(), vec![tag_path()]);
    // Without --output the same lines are printed and nothing else is written.
    let s = scratch("good-no-output");
    let again = desired(&good(), &at(by_version(VERSION), s.d(), None)).expect("passes");
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
    let request = Request {
        after_run: RUN,
        ..at(by_version(""), s.d(), Some(&output))
    };
    let printed = desired(&gh, &request).expect("passes");
    assert_eq!(printed[1], "set=true");
    assert_eq!(printed[2], format!("package-version={PACKAGES}"));
    assert_eq!(
        gh.asked.borrow()[..2],
        [
            format!("repos/{REPO}/actions/runs/{RUN}/jobs?per_page=100"),
            format!("repos/{REPO}/releases?per_page=100"),
        ]
    );
    assert!(
        std::fs::read_to_string(&output)
            .expect("the output")
            .starts_with("set=true\n")
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
fn after_a_run_that_published_nothing_nothing_is_set_and_nothing_is_downloaded() {
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
        let gh = after(good(), jobs.clone(), json!([listed(TAG)]));
        let request = Request {
            after_run: RUN,
            ..at(by_version(""), s.d(), Some(&output))
        };
        assert_eq!(
            desired(&gh, &request).expect("passes"),
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
        assert!(!s.join("d").exists(), "{jobs}");
        assert_eq!(gh.asked.borrow().len(), 1, "{jobs}");
        // Without --output nothing is written.
        let request = Request {
            output: None,
            ..request
        };
        assert_eq!(desired(&gh, &request).expect("passes")[1], "set=false");
    }
}

#[test]
fn a_run_github_does_not_know_or_lists_in_more_than_one_page_is_refused() {
    // Catches: a run id of another repository, and a run whose publish job is on a page that
    // was not read, being taken for a run that published nothing.
    let s = scratch("after-run-bad");
    let request = Request {
        after_run: RUN,
        ..at(by_version(""), s.d(), None)
    };
    let gh = good();
    assert_eq!(
        failed(desired(&gh, &request)),
        "example/tierstore has no run 4242"
    );
    let mut gh = after(good(), run_jobs("success"), json!([listed(TAG)]));
    gh.answers[1].1["total_count"] = json!(101);
    assert_eq!(
        failed(desired(&gh, &request)),
        "run 4242 of example/tierstore has 101 jobs; one page of 3 was read"
    );
    gh.answers[1].1 = json!({ "jobs": run_jobs("success") });
    assert_eq!(
        failed(desired(&gh, &request)),
        "run 4242 of example/tierstore has null jobs; one page of 3 was read"
    );
    assert!(gh.ran.borrow().is_empty());
}

#[test]
fn a_request_without_exactly_one_way_or_with_a_malformed_one_is_a_usage_error() {
    // Catches: a run by hand that also names a run (or a run after a release that also names a
    // version) picking one silently, neither picking anything, a run id that is not a number
    // reaching GitHub's API path, and a tag (with its v), a package version, a short or padded
    // version reaching GitHub, where it could name another release than the operator meant.
    let s = scratch("usage");
    let gh = good();
    let both = Request {
        after_run: RUN,
        ..at(by_version(VERSION), s.d(), None)
    };
    let neither = at(by_version(""), s.d(), None);
    for request in [both, neither] {
        assert_eq!(
            usage(desired(&gh, &request)),
            "give one of --version and --after-run, not both and not neither"
        );
    }
    for run in ["42/../1", "-1", "4242 "] {
        let request = Request {
            after_run: run,
            ..neither
        };
        assert_eq!(
            usage(desired(&gh, &request)),
            format!("--after-run {run:?} is not the id of a workflow run")
        );
    }
    for version in ["v0.4.0", "0.4", "0.4.0+0123456789ab", "0.04.0", "0.4.0.1"] {
        assert_eq!(
            usage(desired(&gh, &at(by_version(version), s.d(), None))),
            format!(
                "--version {version:?} is not <major>.<minor>.<patch>, a release tag without its v"
            )
        );
    }
    assert!(gh.asked.borrow().is_empty());
    assert!(gh.ran.borrow().is_empty());
}

#[test]
fn an_index_that_lacks_a_platform_is_refused_and_nothing_is_handed_on() {
    // Catches: a version set whose index has no macOS package (every Mac would be ordered
    // nothing while the Linux hosts converge), or no package for another platform; and an
    // output written for a refused release, which the next job would set.
    let cases: [(&[Platform], &[&str]); 3] = [
        (
            &[Platform::LinuxArm64, Platform::LinuxX86_64],
            &["macos-arm64"],
        ),
        (
            &[Platform::LinuxX86_64, Platform::MacosArm64],
            &["linux-arm64"],
        ),
        (&[Platform::LinuxX86_64], &["linux-arm64", "macos-arm64"]),
    ];
    for (listed, lacking) in cases {
        let s = scratch("lacks");
        let output = s.join("output");
        let packages = packages(listed);
        let gh = released(
            index_of("tierstore", PACKAGES, &packages).as_bytes(),
            &packages,
        );
        let lines = refused(desired(&gh, &at(by_version(VERSION), s.d(), Some(&output))));
        let expected: Vec<String> = lacking.iter().copied().map(no_package_line).collect();
        assert_eq!(lines, expected, "{listed:?}");
        assert!(!output.exists(), "{listed:?}: nothing is handed on");
        assert_eq!(
            gh.ran.borrow().len(),
            1,
            "{listed:?}: no package downloaded"
        );
    }
}

#[test]
fn a_version_with_no_published_release_is_refused_before_anything_is_downloaded() {
    // Catches: a version that was never released, a draft or a pre-release set as desired.
    let s = scratch("none");
    let request = at(by_version(VERSION), s.d(), None);
    let gh = FakeGh {
        answers: Vec::new(),
        ..good()
    };
    assert_eq!(
        failed(desired(&gh, &request)),
        "example/tierstore has no release v0.4.0: --version names a released version of tierstore"
    );
    assert!(gh.ran.borrow().is_empty());
    for key in ["draft", "prerelease"] {
        let gh = with_release(good(), |r| r[key] = json!(true));
        assert_eq!(
            failed(desired(&gh, &request)),
            "release v0.4.0 of example/tierstore is a draft or a pre-release, not a released version",
            "{key}"
        );
        assert!(gh.ran.borrow().is_empty(), "{key}");
    }
}

#[test]
fn a_release_whose_commit_is_not_a_full_sha_is_refused() {
    // Catches: a release made at a branch name, or one whose commit cannot give the package
    // version (its first 12 digits), being read as if it named a commit.
    let s = scratch("commit");
    let request = at(by_version(VERSION), s.d(), None);
    let upper = COMMIT.to_ascii_uppercase();
    let upper_shown = format!("{upper:?}");
    for (commit, shown) in [
        (json!("main"), "\"main\""),
        (json!(upper), upper_shown.as_str()),
        (json!(null), "\"\""),
    ] {
        let gh = with_release(good(), |r| r["target_commitish"] = commit.clone());
        assert_eq!(
            failed(desired(&gh, &request)),
            format!(
                "release v0.4.0 of example/tierstore names {shown} as its commit, not a full commit SHA"
            )
        );
    }
}

/// `good()` with the assets named `names` dropped from the release.
fn without(names: &[&str]) -> FakeGh {
    with_release(good(), |r| {
        let kept: Vec<Value> = r["assets"]
            .as_array()
            .expect("assets")
            .iter()
            .filter(|a| !names.contains(&a["name"].as_str().expect("name")))
            .cloned()
            .collect();
        r["assets"] = Value::Array(kept);
    })
}

#[test]
fn a_release_without_its_index_or_its_sums_is_refused_before_anything_is_downloaded() {
    // Catches: a release made before the hostkeeper packages (no index.json) set as desired,
    // which no host can take, and one without the sums the index is checked against.
    let s = scratch("no-index");
    let request = at(by_version(VERSION), s.d(), None);
    let line = |name: &str| {
        format!("release v0.4.0 has no {name}: it has no hostkeeper packages for a host to take")
    };
    for (names, expected) in [
        (&[INDEX][..], vec![line(INDEX)]),
        (&[SUMS][..], vec![line(SUMS)]),
        (&[INDEX, SUMS][..], vec![line(INDEX), line(SUMS)]),
    ] {
        let gh = without(names);
        assert_eq!(refused(desired(&gh, &request)), expected, "{names:?}");
        assert!(gh.ran.borrow().is_empty(), "{names:?}");
    }
}

#[test]
fn a_listed_package_that_is_not_an_asset_or_not_its_size_or_has_no_bundle_is_refused() {
    // Catches: an index whose packages were not all published (a host would fetch nothing), a
    // published package of another size than the index names, and a package without the
    // attestation bundle every archive entry carries.
    let s = scratch("assets");
    let request = at(by_version(VERSION), s.d(), None);
    let x86 = file_name(PACKAGES, Platform::LinuxX86_64);
    let mac = file_name(PACKAGES, Platform::MacosArm64);
    let gh = without(&[&x86]);
    assert_eq!(
        refused(desired(&gh, &request)),
        vec![format!(
            "index.json lists {x86}, which is not an asset of the release"
        )]
    );
    let bundle = bundles::name_of(&mac);
    let gh = without(&[&bundle]);
    assert_eq!(
        refused(desired(&gh, &request)),
        vec![format!(
            "{mac} has no bundle {bundle} in the release: every archive entry carries its attestation"
        )]
    );
    let gh = with_release(good(), |r| {
        for a in r["assets"].as_array_mut().expect("assets") {
            if a["name"] == x86.as_str() {
                a["size"] = json!(1);
            }
        }
    });
    let size = archive_of(Platform::LinuxX86_64, PACKAGES).len();
    assert_eq!(
        refused(desired(&gh, &request)),
        vec![format!(
            "{x86}: the release's asset has Some(1) bytes, not the {size} index.json lists"
        )]
    );
}

#[test]
fn a_downloaded_package_or_bundle_that_is_not_the_one_the_index_lists_is_refused() {
    // Catches: a package swapped after the index was made (the index's digest would be set for
    // other bytes), a bundle that attests another file, and a bundle that is not a bundle; every
    // problem is listed, and nothing is handed on.
    let s = scratch("swapped");
    let output = s.join("output");
    let request = at(by_version(VERSION), s.d(), Some(&output));
    let arm = file_name(PACKAGES, Platform::LinuxArm64);
    let mac = file_name(PACKAGES, Platform::MacosArm64);
    let other = archive_of(Platform::LinuxArm64, "0.4.0+0123456789aa");
    let listed = entry_of(
        Platform::LinuxArm64,
        &archive_of(Platform::LinuxArm64, PACKAGES),
    );
    let gh = with_file(good(), &arm, other.clone());
    let gh = with_file(gh, &bundles::name_of(&mac), b"not a bundle\n".to_vec());
    let mut expected = vec![format!(
        "{arm}: sha256 {} and {} bytes, not the sha256 {} and {} bytes index.json lists (package_digest_mismatch)",
        Sha256::of(&other),
        other.len(),
        listed.sha256.as_str(),
        listed.bytes
    )];
    // The bundle of the listed package does not attest the bytes downloaded in its place.
    let bundle = bundle_of(&Sha256::of(&archive_of(Platform::LinuxArm64, PACKAGES)));
    let subject = bundles::check(&bundle, &Sha256::of(&other));
    assert!(!subject.is_empty());
    expected.extend(subject.iter().map(|p| format!("{arm}.sigstore.json: {p}")));
    expected.push(format!(
        "{mac}.sigstore.json: is not JSON: expected ident at line 1 column 2"
    ));
    assert_eq!(refused(desired(&gh, &request)), expected);
    assert!(!output.exists());
}

#[test]
fn a_package_hostkeeper_does_not_unpack_is_refused_with_its_reason() {
    // Catches: targets handed on for a package whose manifest names another version (a host
    // refuses it at the unpack, after the coordinator ordered it), and a floor read without the
    // checks a host makes.
    let s = scratch("unpack");
    let mut packages = packages(&Platform::ALL);
    packages[0].1 = package(
        packages[0].0,
        "0.4.0+0123456789aa",
        "0.0.9",
        b"stand-in tierstore\0",
    )
    .expect("a package");
    let gh = released(
        index_of("tierstore", PACKAGES, &packages).as_bytes(),
        &packages,
    );
    let text = failed(desired(&gh, &at(by_version(VERSION), s.d(), None)));
    let name = file_name(PACKAGES, packages[0].0);
    assert!(
        text.starts_with(&format!(
            "{}: hostkeeper does not unpack it: ",
            s.join("d").join(&name).display()
        )),
        "{text}"
    );
    assert!(text.contains("0.4.0+0123456789aa"), "{text}");
}

#[test]
fn a_failed_download_or_a_download_without_a_file_is_reported() {
    // Catches: a failed `gh release download` read as an empty release, and a file that the
    // download did not write read as empty.
    let s = scratch("download");
    let request = at(by_version(VERSION), s.d(), None);
    let gh = FakeGh {
        fails: Some("gh: HTTP 502".to_owned()),
        ..good()
    };
    assert_eq!(failed(desired(&gh, &request)), "gh: HTTP 502");
    for name in [INDEX.to_owned(), file_name(PACKAGES, Platform::LinuxX86_64)] {
        let s = scratch("download-missing");
        let dir = s.join("d");
        let mut gh = good();
        gh.files.retain(|(n, _)| *n != name);
        match desired(&gh, &at(by_version(VERSION), &dir, None)) {
            Err(Error::Spawn { what, .. }) => {
                assert_eq!(what, format!("read {}", dir.join(&name).display()));
            }
            other => panic!("{name}: {other:?}"),
        }
    }
    let s = scratch("download-no-bundle");
    let dir = s.join("d");
    let bundle = bundles::name_of(&file_name(PACKAGES, Platform::LinuxArm64));
    let mut gh = good();
    gh.files.retain(|(n, _)| *n != bundle);
    match desired(&gh, &at(by_version(VERSION), &dir, None)) {
        Err(Error::Spawn { what, .. }) => {
            assert_eq!(what, format!("read {}", dir.join(&bundle).display()));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_dir_that_cannot_be_made_is_reported_before_the_download() {
    // Catches: a download into a directory that does not exist, which `gh` would report less
    // clearly, after the release was already read.
    let s = scratch("dir");
    let blocker = s.join("file");
    std::fs::write(&blocker, b"").expect("a file");
    let dir = blocker.join("d");
    let gh = good();
    match desired(&gh, &at(by_version(VERSION), &dir, None)) {
        Err(Error::Spawn { what, .. }) => assert_eq!(what, format!("create {}", dir.display())),
        other => panic!("{other:?}"),
    }
    assert!(gh.ran.borrow().is_empty());
}

#[test]
fn an_index_that_is_not_what_the_sums_list_is_refused() {
    // Catches: an index swapped after the release was made, and sums without a line for it.
    let s = scratch("sums");
    let request = at(by_version(VERSION), s.d(), None);
    let index = good().files[0].1.clone();
    let digest = Sha256::of(&index);
    let gh = with_file(good(), SUMS, sums_of(b"another index"));
    assert_eq!(
        failed(desired(&gh, &request)),
        format!(
            "index.json has sha256 {digest}; SHA256SUMS of release v0.4.0 lists sha256 {} for it",
            Sha256::of(b"another index")
        )
    );
    let gh = with_file(
        good(),
        SUMS,
        render(&[Line {
            sha256: digest,
            name: "other.json".to_owned(),
        }])
        .into_bytes(),
    );
    assert_eq!(
        failed(desired(&gh, &request)),
        format!(
            "index.json has sha256 {digest}; SHA256SUMS of release v0.4.0 lists no line for it"
        )
    );
}

/// `good()` with `index` as its index, listed by its sums.
fn with_index(index: &[u8]) -> FakeGh {
    let gh = with_file(good(), INDEX, index.to_vec());
    with_file(gh, SUMS, sums_of(index))
}

#[test]
fn an_index_that_is_not_utf8_or_of_another_service_or_version_is_refused() {
    // Catches: bytes that are not text read lossily, and an index of hello or of another commit's
    // packages set as tierstore at this version.
    let s = scratch("index");
    let request = at(by_version(VERSION), s.d(), None);
    assert_eq!(
        failed(desired(&with_index(b"\xff\xfe"), &request)),
        "index.json of release v0.4.0 is not UTF-8"
    );
    let packages = packages(&Platform::ALL);
    for (service, version) in [("hello", PACKAGES), ("tierstore", "0.4.0+0123456789aa")] {
        let index = index_of(service, version, &packages);
        let text = failed(desired(&with_index(index.as_bytes()), &request));
        assert!(
            text.contains("index_invalid"),
            "{service} {version}: {text}"
        );
    }
}

#[test]
fn an_index_that_is_not_canonical_or_too_long_for_the_client_is_refused() {
    // Catches: an index whose digest the coordinator would record for other bytes than a host
    // writes, and one longer than the 3 KiB the CI client passes (the call would fail after the
    // token exists); 3072 bytes pass the length rule, 3073 do not.
    let s = scratch("canonical");
    let request = at(by_version(VERSION), s.d(), None);
    let index = index_of("tierstore", PACKAGES, &packages(&Platform::ALL));
    let not_canonical =
        "index.json is not the canonical JSON of its index (keys sorted, no white space)";
    let at_most = format!("{index}{}", " ".repeat(MAX_INDEX_BYTES - index.len()));
    assert_eq!(
        refused(desired(&with_index(at_most.as_bytes()), &request)),
        vec![not_canonical.to_owned()]
    );
    let over = format!("{at_most} ");
    assert_eq!(
        refused(desired(&with_index(over.as_bytes()), &request)),
        vec![
            not_canonical.to_owned(),
            "index.json is 3073 bytes, over the 3072 hostkeeper's CI client passes".to_owned(),
        ]
    );
}

#[test]
fn an_output_that_cannot_be_appended_to_is_reported() {
    // Catches: a step that passes with nothing handed on, so the next job sets an empty version.
    for after_run in ["", RUN] {
        let s = scratch("output");
        let output = s.join("no-such-dir").join("output");
        let gh = after(good(), run_jobs("skipped"), json!([listed(TAG)]));
        let request = Request {
            version: if after_run.is_empty() { VERSION } else { "" },
            after_run,
            ..at(by_version(""), s.d(), Some(&output))
        };
        match desired(&gh, &request) {
            Err(Error::Spawn { what, .. }) => {
                assert_eq!(what, format!("append to {}", output.display()));
            }
            other => panic!("{after_run:?}: {other:?}"),
        }
    }
}

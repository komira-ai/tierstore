use std::cell::RefCell;

use hostkeeper_config::daemon::Sha256Hex;
use hostkeeper_config::index::PackageEntry;
use hostkeeper_config::names::{ServiceName, Version};
use serde_json::{Value, json};

use super::*;
use crate::sums::{Line, render};
use crate::testdir::TestDir;

const REPO: &str = "example/tierstore";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const VERSION: &str = "0.4.0";
const TAG: &str = "v0.4.0";
/// The version of the hostkeeper packages of release [`TAG`] at [`COMMIT`].
const PACKAGES: &str = "0.4.0+0123456789ab";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-desired-unit"), name)
}

/// The entry of `platform` in the index: a digest and a size of its own.
fn entry(platform: Platform) -> PackageEntry {
    let sha = hex::encode(Sha256::of(platform.name().as_bytes()).0);
    PackageEntry {
        platform: host_platform(platform),
        sha256: Sha256Hex::parse(&sha).expect("a digest"),
        bytes: 1000 + platform.name().len() as u64,
    }
}

/// The canonical index of `service` at `version` listing `platforms`.
fn index_of(service: &str, version: &str, platforms: &[Platform]) -> String {
    ReleaseIndex::new(
        ServiceName::new(service).expect("a service"),
        Version::new(version).expect("a version"),
        platforms.iter().map(|p| entry(*p)).collect(),
    )
    .expect("an index")
    .to_json()
}

/// The `SHA256SUMS` of a release whose index is `index`.
fn sums_of(index: &[u8]) -> Vec<u8> {
    render(&[Line {
        sha256: Sha256::of(index),
        name: INDEX.to_owned(),
    }])
    .into_bytes()
}

/// The release as GitHub's API answers for it, with `assets` (name, size).
fn release_of(assets: &[(String, u64)]) -> Value {
    let assets: Vec<Value> = assets
        .iter()
        .map(|(name, size)| json!({ "name": name, "size": size }))
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
    /// The answer for the release [`TAG`]; none is a 404.
    release: Option<Value>,
    /// The files `gh release download` writes: name and bytes.
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
        assert_eq!(path, format!("repos/{REPO}/releases/tags/{TAG}"));
        Ok(self.release.clone())
    }

    fn run(&self, args: &[String]) -> Result<String, Error> {
        self.ran.borrow_mut().push(args.to_vec());
        if let Some(why) = &self.fails {
            return Err(Error::Failed(why.clone()));
        }
        let dir = Path::new(&args[6]);
        for (name, bytes) in &self.files {
            std::fs::write(dir.join(name), bytes).expect("write a downloaded file");
        }
        Ok(String::new())
    }
}

/// A release of `index` (its bytes) with the `SHA256SUMS` that lists it, and the assets: the two
/// and a package of each platform the index of [`PACKAGES`] lists, at its size.
fn released(index: &[u8], platforms: &[Platform]) -> FakeGh {
    let sums = sums_of(index);
    let mut assets = vec![
        (INDEX.to_owned(), index.len() as u64),
        (SUMS.to_owned(), sums.len() as u64),
    ];
    for platform in platforms {
        assets.push((file_name(PACKAGES, *platform), entry(*platform).bytes));
    }
    FakeGh {
        release: Some(release_of(&assets)),
        files: vec![(INDEX.to_owned(), index.to_vec()), (SUMS.to_owned(), sums)],
        fails: None,
        asked: RefCell::new(Vec::new()),
        ran: RefCell::new(Vec::new()),
    }
}

/// A release whose index lists all three platforms.
fn good() -> FakeGh {
    released(
        index_of("tierstore", PACKAGES, &Platform::ALL).as_bytes(),
        &Platform::ALL,
    )
}

/// `gh`'s release, edited by `edit`.
fn with_release(mut gh: FakeGh, edit: impl Fn(&mut Value)) -> FakeGh {
    edit(gh.release.as_mut().expect("a release"));
    gh
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

fn no_package_line(platform: &str) -> String {
    format!(
        "index.json: the release index has no package for {platform} (no_package_for_platform): a host of {platform} would be refused the version, so it is not set"
    )
}

#[test]
fn a_release_of_three_platforms_hands_on_its_package_version_and_the_digest_of_its_index() {
    // Catches: a version handed to the coordinator that is not the one the packages carry (the
    // tag alone, or another commit's), a digest that is not index.json's, and the download asking
    // for other files or another release.
    let s = scratch("good");
    let dir = s.join("desired");
    let output = s.join("output");
    let gh = good();
    let index = index_of("tierstore", PACKAGES, &Platform::ALL);
    let digest = Sha256::of(index.as_bytes());
    let printed = desired(&gh, REPO, VERSION, &dir, Some(output.as_path())).expect("passes");
    assert_eq!(
        printed,
        vec![
            format!(
                "release v0.4.0 ({COMMIT}): index.json lists tierstore {PACKAGES} for each of 3 platforms, each an asset of the release"
            ),
            format!("package-version={PACKAGES}"),
            format!("package-digest={digest}"),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&output).expect("the output"),
        format!("package-version={PACKAGES}\npackage-digest={digest}\n")
    );
    let into = dir.display().to_string();
    let download: Vec<String> = [
        "release",
        "download",
        TAG,
        "--repo",
        REPO,
        "--dir",
        into.as_str(),
        "--pattern",
        "index.json",
        "--pattern",
        "SHA256SUMS",
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    assert_eq!(*gh.ran.borrow(), vec![download]);
    // Without --output the same lines are printed and nothing else is written.
    let s = scratch("good-no-output");
    let again = desired(&good(), REPO, VERSION, &s.join("desired"), None).expect("passes");
    assert_eq!(again, printed);
}

#[test]
fn an_index_that_lacks_a_platform_is_refused_and_nothing_is_handed_on() {
    // Catches: a version set whose index has no macOS package (every Mac would stall on
    // no_package_for_platform while the Linux hosts converge), or no package for another
    // platform; and an output written for a refused release, which the next job would set.
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
        let gh = released(index_of("tierstore", PACKAGES, listed).as_bytes(), listed);
        let lines = refused(desired(
            &gh,
            REPO,
            VERSION,
            &s.join("d"),
            Some(output.as_path()),
        ));
        let expected: Vec<String> = lacking.iter().copied().map(no_package_line).collect();
        assert_eq!(lines, expected, "{listed:?}");
        assert!(!output.exists(), "{listed:?}: nothing is handed on");
    }
}

#[test]
fn a_version_that_is_not_major_minor_patch_is_a_usage_error_and_github_is_not_asked() {
    // Catches: a tag (with its v), a package version, a short or padded version or an empty one
    // reaching GitHub, where it could name another release than the operator meant.
    for version in [
        "v0.4.0",
        "0.4",
        "0.4.0+0123456789ab",
        "0.04.0",
        "0.4.0.1",
        "",
    ] {
        let gh = good();
        let s = scratch("usage");
        match desired(&gh, REPO, version, &s.join("d"), None) {
            Err(Error::Usage(text)) => assert_eq!(
                text,
                format!(
                    "--version {version:?} is not <major>.<minor>.<patch>, a release tag without its v"
                )
            ),
            other => panic!("{version:?}: {other:?}"),
        }
        assert!(gh.asked.borrow().is_empty(), "{version:?}");
    }
}

#[test]
fn a_version_with_no_published_release_is_refused_before_anything_is_downloaded() {
    // Catches: a version that was never released, a draft or a pre-release set as desired.
    let s = scratch("none");
    let gh = FakeGh {
        release: None,
        ..good()
    };
    assert_eq!(
        failed(desired(&gh, REPO, VERSION, &s.join("d"), None)),
        "example/tierstore has no release v0.4.0: --version names a released version of tierstore"
    );
    assert!(gh.ran.borrow().is_empty());
    for key in ["draft", "prerelease"] {
        let gh = with_release(good(), |r| r[key] = json!(true));
        assert_eq!(
            failed(desired(&gh, REPO, VERSION, &s.join("d"), None)),
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
    let upper = COMMIT.to_ascii_uppercase();
    let upper_shown = format!("{upper:?}");
    for (commit, shown) in [
        (json!("main"), "\"main\""),
        (json!(upper), upper_shown.as_str()),
        (json!(null), "\"\""),
    ] {
        let gh = with_release(good(), |r| r["target_commitish"] = commit.clone());
        assert_eq!(
            failed(desired(&gh, REPO, VERSION, &s.join("d"), None)),
            format!(
                "release v0.4.0 of example/tierstore names {shown} as its commit, not a full commit SHA"
            )
        );
    }
}

#[test]
fn a_release_without_its_index_or_its_sums_is_refused_before_anything_is_downloaded() {
    // Catches: a release made before the hostkeeper packages (no index.json) set as desired,
    // which no host can take, and one without the sums the index is checked against.
    let s = scratch("no-index");
    let without = |names: &[&str]| {
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
    };
    let line = |name: &str| {
        format!("release v0.4.0 has no {name}: it has no hostkeeper packages for a host to take")
    };
    for (names, lines) in [
        (vec!["index.json"], vec![line("index.json")]),
        (vec!["SHA256SUMS"], vec![line("SHA256SUMS")]),
        (
            vec!["index.json", "SHA256SUMS"],
            vec![line("index.json"), line("SHA256SUMS")],
        ),
    ] {
        let gh = without(&names[..]);
        assert_eq!(
            refused(desired(&gh, REPO, VERSION, &s.join("d"), None)),
            lines
        );
        assert!(gh.ran.borrow().is_empty(), "{names:?}");
    }
    // A release with no list of assets at all has neither.
    let gh = with_release(good(), |r| r["assets"] = json!(null));
    assert_eq!(
        refused(desired(&gh, REPO, VERSION, &s.join("d"), None)),
        vec![line("index.json"), line("SHA256SUMS")]
    );
}

#[test]
fn a_failed_download_or_a_download_without_the_index_is_reported() {
    // Catches: a failed `gh release download` read as an empty release, and a check that goes
    // on without the file it checks.
    let s = scratch("download");
    let gh = FakeGh {
        fails: Some("gh release download: HTTP 502".to_owned()),
        ..good()
    };
    assert_eq!(
        failed(desired(&gh, REPO, VERSION, &s.join("d"), None)),
        "gh release download: HTTP 502"
    );
    let gh = FakeGh {
        files: Vec::new(),
        ..good()
    };
    let dir = s.join("empty");
    match desired(&gh, REPO, VERSION, &dir, None) {
        Err(Error::Spawn { what, .. }) => {
            assert_eq!(what, format!("read {}", dir.join(INDEX).display()));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_dir_that_cannot_be_made_is_reported_before_the_download() {
    // Catches: a download into a path that is a file, reported as gh's failure or not at all.
    let s = scratch("dir-file");
    let file = s.join("file");
    std::fs::write(&file, b"a file").expect("write");
    let gh = good();
    match desired(&gh, REPO, VERSION, &file.join("d"), None) {
        Err(Error::Spawn { what, .. }) => {
            assert_eq!(what, format!("create {}", file.join("d").display()));
        }
        other => panic!("{other:?}"),
    }
    assert!(gh.ran.borrow().is_empty());
}

#[test]
fn an_index_that_is_not_what_the_sums_list_is_refused() {
    // Catches: an index.json replaced after the release was made (another digest than the
    // signed sums list), and sums that do not list it, being set as desired.
    let s = scratch("sums");
    let index = index_of("tierstore", PACKAGES, &Platform::ALL);
    let digest = Sha256::of(index.as_bytes());
    let other = Sha256::of(b"another index");
    for (sums, said) in [
        (
            render(&[Line {
                sha256: other,
                name: INDEX.to_owned(),
            }]),
            format!("sha256 {other}"),
        ),
        (
            render(&[Line {
                sha256: digest,
                name: "other.json".to_owned(),
            }]),
            "no line".to_owned(),
        ),
    ] {
        let mut gh = good();
        gh.files[1].1 = sums.into_bytes();
        assert_eq!(
            failed(desired(&gh, REPO, VERSION, &s.join("d"), None)),
            format!(
                "index.json has sha256 {digest}; SHA256SUMS of release v0.4.0 lists {said} for it"
            )
        );
    }
    // Sums that are not sums are refused by their reader.
    let mut gh = good();
    gh.files[1].1 = b"not a sums line\n".to_vec();
    refused(desired(&gh, REPO, VERSION, &s.join("bad"), None));
}

#[test]
fn an_index_that_is_not_utf8_is_refused() {
    // Catches: bytes that hostkeeper's reader cannot be given read as an empty or lossy index.
    let s = scratch("utf8");
    let gh = released(b"{\"schema\":1,\xff}", &Platform::ALL);
    assert_eq!(
        failed(desired(&gh, REPO, VERSION, &s.join("d"), None)),
        "index.json of release v0.4.0 is not UTF-8"
    );
}

#[test]
fn an_index_of_another_service_or_version_is_refused_with_hostkeepers_reason() {
    // Catches: an index of another commit's packages (the version a host would be told is not
    // the one the index lists, so every host would refuse it), or of another service.
    let s = scratch("other");
    for (service, version, says) in [
        (
            "tierstore",
            "0.4.0+fedcba987654",
            "the release index is for the version \"0.4.0+fedcba987654\", not \"0.4.0+0123456789ab\" (index_invalid)",
        ),
        (
            "hello",
            PACKAGES,
            "the release index is for the service \"hello\", not \"tierstore\" (index_invalid)",
        ),
    ] {
        let gh = released(
            index_of(service, version, &Platform::ALL).as_bytes(),
            &Platform::ALL,
        );
        assert_eq!(
            failed(desired(&gh, REPO, VERSION, &s.join("d"), None)),
            says
        );
    }
}

#[test]
fn an_index_that_is_not_canonical_is_refused() {
    // Catches: an index that hostkeeper reads but that its writer would not have written, so not
    // the bytes the build made.
    let s = scratch("pretty");
    let index = index_of("tierstore", PACKAGES, &Platform::ALL);
    let value: Value = serde_json::from_str(&index).expect("json");
    let pretty = serde_json::to_string_pretty(&value).expect("pretty");
    let gh = released(pretty.as_bytes(), &Platform::ALL);
    assert_eq!(
        refused(desired(&gh, REPO, VERSION, &s.join("d"), None)),
        vec![
            "index.json is not the canonical JSON of its index (keys sorted, no white space)"
                .to_owned()
        ]
    );
}

#[test]
fn a_listed_package_that_is_not_an_asset_of_the_release_or_not_its_size_is_refused() {
    // Catches: an index whose package of a platform the release does not hold, or holds at
    // another size, so that hosts of that platform would fail to take the version.
    let s = scratch("assets");
    let mac = file_name(PACKAGES, Platform::MacosArm64);
    let arm = file_name(PACKAGES, Platform::LinuxArm64);
    let gh = with_release(good(), |r| {
        let assets = r["assets"].as_array_mut().expect("assets");
        assets.retain(|a| a["name"] != mac.as_str());
        for a in assets.iter_mut() {
            if a["name"] == arm.as_str() {
                a["size"] = json!(7);
            }
        }
    });
    let arm_bytes = entry(Platform::LinuxArm64).bytes;
    assert_eq!(
        refused(desired(&gh, REPO, VERSION, &s.join("d"), None)),
        vec![
            format!(
                "{arm}: the release's asset has Some(7) bytes, not the {arm_bytes} index.json lists"
            ),
            format!("index.json lists {mac}, which is not an asset of the release"),
        ]
    );
    // An asset without a size is not taken to have the index's.
    let gh = with_release(good(), |r| {
        for a in r["assets"].as_array_mut().expect("assets").iter_mut() {
            if a["name"] == arm.as_str() {
                a["size"] = json!(null);
            }
        }
    });
    assert_eq!(
        refused(desired(&gh, REPO, VERSION, &s.join("d"), None)),
        vec![format!(
            "{arm}: the release's asset has None bytes, not the {arm_bytes} index.json lists"
        )]
    );
}

#[test]
fn an_output_that_cannot_be_appended_to_is_reported() {
    // Catches: a version and digest that were not handed on reported as passed.
    let s = scratch("output");
    match desired(&good(), REPO, VERSION, &s.join("d"), Some(&*s)) {
        Err(Error::Spawn { what, .. }) => assert_eq!(what, format!("append to {}", s.display())),
        other => panic!("{other:?}"),
    }
}

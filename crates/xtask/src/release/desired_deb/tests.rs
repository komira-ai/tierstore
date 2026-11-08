use std::cell::RefCell;

use serde_json::{Value, json};

use super::*;
use crate::channels::apt::index::sha256_hex;
use crate::channels::aws::Answer;
use crate::sums::{Line, render};
use crate::testdir::TestDir;

const REPO: &str = "example/tierstore";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const URL: &str = "https://packages.komira.ai/apt";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside
            .parent()
            .expect("deps")
            .join("xtask-desired-deb-unit"),
        name,
    )
}

fn deb_bytes(name: &str) -> Vec<u8> {
    format!("bytes of {name}").into_bytes()
}

fn sums_text(names: &[String]) -> Vec<u8> {
    let lines: Vec<Line> = names
        .iter()
        .map(|n| Line {
            sha256: Sha256::of(&deb_bytes(n)),
            name: n.clone(),
        })
        .collect();
    render(&lines).into_bytes()
}

/// GitHub: the release v1.2.3, with assets, and the files `gh release download` writes.
struct FakeGh {
    release: Option<Value>,
    sums: Vec<u8>,
    ran: RefCell<Vec<Vec<String>>>,
}

impl Gh for FakeGh {
    fn api(&self, path: &str) -> Result<Option<Value>, Error> {
        assert_eq!(path, format!("repos/{REPO}/releases/tags/v1.2.3"));
        Ok(self.release.clone())
    }

    fn run(&self, args: &[String]) -> Result<String, Error> {
        self.ran.borrow_mut().push(args.to_vec());
        std::fs::write(Path::new(&args[6]).join(SUMS), &self.sums).expect("write");
        Ok(String::new())
    }
}

fn release(with_sums: bool) -> Option<Value> {
    let assets: Vec<Value> = if with_sums {
        vec![json!({"name": SUMS, "size": 10})]
    } else {
        Vec::new()
    };
    Some(json!({"draft": false, "prerelease": false, "target_commitish": COMMIT, "assets": assets}))
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

/// The `Packages` text listing `name` with `digest`.
fn stanza(name: &str, digest: &str) -> String {
    let arch = arch_of(name);
    format!(
        "Package: tierstore\nVersion: 1.2.3\nArchitecture: {arch}\nFilename: pool/tierstore/{name}\nSize: 1\nSHA256: {digest}\n\n"
    )
}

fn good_cdn() -> Cdn {
    let packages = package_names("1.2.3")
        .expect("names")
        .iter()
        .map(|n| {
            (
                packages_url(URL, arch_of(n)),
                Ok(stanza(n, &sha256_hex(&deb_bytes(n)))),
            )
        })
        .collect();
    Cdn {
        packages,
        asked: RefCell::new(Vec::new()),
    }
}

fn gh() -> FakeGh {
    FakeGh {
        release: release(true),
        sums: sums_text(&package_names("1.2.3").expect("names")),
        ran: RefCell::new(Vec::new()),
    }
}

fn run(gh: &FakeGh, cdn: &Cdn, dir: &Path, output: Option<&Path>) -> Result<Vec<String>, Error> {
    ask(gh, cdn, "1.2.3", dir, URL, output)
}

fn ask(
    gh: &FakeGh,
    cdn: &Cdn,
    version: &str,
    dir: &Path,
    apt_url: &str,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let request = Request {
        curl: Path::new("curl"),
        repo: REPO,
        version,
        dir,
        apt_url,
    };
    desired_deb(gh, cdn, &request, output)
}

fn refused(result: Result<Vec<String>, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(problems)) => problems,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn a_release_the_testing_suite_lists_with_the_digests_of_its_sums_gives_the_package_version_and_digest()
 {
    // Catches: a lab told to install a version testing does not hold, or a digest that pins
    // only one architecture or not the sums' bytes.
    let dir = scratch("ok");
    let out = dir.join("out");
    let cdn = good_cdn();
    let said = run(&gh(), &cdn, &dir, Some(&out)).expect("desired");
    assert_eq!(
        cdn.asked.borrow().as_slice(),
        [
            "https://packages.komira.ai/apt/dists/testing/main/binary-amd64/Packages",
            "https://packages.komira.ai/apt/dists/testing/main/binary-arm64/Packages"
        ]
    );
    let names = package_names("1.2.3").expect("names");
    let expected = Sha256::of(
        names
            .iter()
            .map(|n| format!("{}  {n}\n", Sha256::of(&deb_bytes(n))))
            .collect::<String>()
            .as_bytes(),
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("output"),
        format!("package-version=1.2.3\npackage-digest={expected}\n")
    );
    assert!(
        said[0].starts_with("release v1.2.3 (0123456789abcdef"),
        "{said:?}"
    );
    assert_eq!(said[1], "package-version=1.2.3");
}

#[test]
fn a_package_testing_does_not_list_or_lists_with_another_digest_is_named() {
    // Catches: the lab being sent to bytes other than the ones the release was checked against.
    let dir = scratch("problems");
    let mut cdn = good_cdn();
    cdn.packages[0].1 = Ok(String::new());
    cdn.packages[1].1 = Ok(stanza("tierstore_1.2.3_arm64.deb", &sha256_hex(b"other")));
    let problems = refused(run(&gh(), &cdn, &dir, None));
    assert_eq!(
        problems[0],
        "the testing suite lists no tierstore_1.2.3_amd64.deb: the release is not published there"
    );
    assert!(
        problems[1].starts_with("the testing suite lists tierstore_1.2.3_arm64.deb with sha256 "),
        "{problems:?}"
    );
    assert_eq!(problems.len(), 2);
}

#[test]
fn a_packages_file_that_cannot_be_read_or_is_not_one_and_sums_without_a_package_are_named() {
    // Catches: a CDN failure or a half-written index read as a published release.
    let dir = scratch("unread");
    let mut cdn = good_cdn();
    cdn.packages[0].1 = Err("curl: (22) The requested URL returned error: 404".to_owned());
    let problems = refused(run(&gh(), &cdn, &dir, None));
    assert_eq!(problems.len(), 1);
    assert!(problems[0].ends_with("binary-amd64/Packages could not be read: curl: (22) The requested URL returned error: 404"), "{problems:?}");
    let mut cdn = good_cdn();
    cdn.packages[0].1 = Ok("not a stanza".to_owned());
    let e = run(&gh(), &cdn, &dir, None).unwrap_err().to_string();
    assert!(e.contains("is not a Packages file"), "{e}");
    let mut short = gh();
    short.sums = sums_text(&["tierstore_1.2.3_amd64.deb".to_owned()]);
    let problems = refused(run(&short, &good_cdn(), &dir, None));
    assert_eq!(
        problems,
        ["SHA256SUMS of release v1.2.3 lists no tierstore_1.2.3_arm64.deb"]
    );
}

#[test]
fn a_release_that_is_missing_or_has_no_sums_and_flags_out_of_their_form_are_errors() {
    // Catches: a lab set for a release that does not exist, or from a non-https repository.
    let dir = scratch("errors");
    let mut no_release = gh();
    no_release.release = None;
    assert!(run(&no_release, &good_cdn(), &dir, None).is_err());
    let mut no_sums = gh();
    no_sums.release = release(false);
    let problems = refused(run(&no_sums, &good_cdn(), &dir, None));
    assert_eq!(
        problems,
        ["release v1.2.3 has no SHA256SUMS: there are no digests to hold the testing suite to"]
    );
    for url in [
        "http://packages.komira.ai/apt",
        "https://packages.komira.ai/apt/",
    ] {
        let e = ask(&gh(), &good_cdn(), "1.2.3", &dir, url, None);
        assert!(matches!(e, Err(Error::Usage(_))), "{url}");
    }
    let e = ask(&gh(), &good_cdn(), "v1.2.3", &dir, URL, None);
    assert!(matches!(e, Err(Error::Usage(_))));
}

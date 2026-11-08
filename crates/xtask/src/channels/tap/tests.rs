use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::json;

use super::*;
use crate::fetch::Sha256;
use crate::testdir::TestDir;

const REPO: &str = "komira-ai/tierstore";
const TAP: &str = "komira-ai/homebrew-tap";
const PR_URL: &str = "https://github.com/komira-ai/homebrew-tap/pull/7";

/// A directory of its own for one fixture: the name, the process and a number, so that no two
/// share one, whether the tests run as threads of one process or one process each.
fn scratch(name: &str) -> TestDir {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside.parent().expect("deps").join("xtask-tap-unit"),
        &format!("{name}-{}-{n}", std::process::id()),
    )
}

fn hashes() -> [String; 3] {
    [1_u8, 2, 3].map(|n| Sha256::of(&[n]).to_string())
}

fn formula_of(version: &str) -> String {
    formula::render(REPO, version, &hashes())
}

/// The answer of the contents API for a file: base64 wrapped at 60 digits, as GitHub does.
fn file_json(text: &str) -> Value {
    let encoded = STANDARD.encode(text);
    let wrapped: Vec<&str> = encoded
        .as_bytes()
        .chunks(60)
        .map(|c| std::str::from_utf8(c).expect("ascii"))
        .collect();
    json!({"content": format!("{}\n", wrapped.join("\n")), "sha": "blobsha"})
}

fn pull(number: u64, version: &str, auto_merge: bool) -> Value {
    json!({
        "number": number,
        "html_url": format!("https://github.com/{TAP}/pull/{number}"),
        "head": {"ref": format!("bump/tierstore-v{version}")},
        "auto_merge": if auto_merge { json!({"merge_method": "squash"}) } else { Value::Null },
    })
}

/// A stand-in `gh`: the answers of `gh api` by path, the commands it was run with, and the ones
/// that fail.
#[derive(Default)]
struct FakeGh {
    api: RefCell<BTreeMap<String, Value>>,
    runs: RefCell<Vec<Vec<String>>>,
    /// A run whose arguments, joined by spaces, contain the text fails with the message.
    failures: RefCell<Vec<(String, String)>>,
    /// What opening the pull request prints.
    opened: RefCell<Option<String>>,
}

impl FakeGh {
    /// The world before a release: the tag is there, the tap is at 1.2.2, nothing is open and
    /// the base is at `basesha`.
    fn new() -> FakeGh {
        let gh = FakeGh::default();
        gh.set(&format!("repos/{REPO}/git/ref/tags/v1.2.3"), json!({}));
        gh.set(
            &format!("repos/{TAP}/contents/{PATH}?ref=main"),
            file_json(&formula_of("1.2.2")),
        );
        gh.set(
            &format!("repos/{TAP}/pulls?state=open&per_page=100"),
            json!([]),
        );
        gh.set(
            &format!("repos/{TAP}/git/ref/heads/main"),
            json!({"object": {"sha": "basesha"}}),
        );
        gh
    }

    fn set(&self, path: &str, value: Value) {
        self.api.borrow_mut().insert(path.to_owned(), value);
    }

    fn unset(&self, path: &str) {
        self.api.borrow_mut().remove(path);
    }

    fn fail_when(&self, text: &str, message: &str) {
        self.failures
            .borrow_mut()
            .push((text.to_owned(), message.to_owned()));
    }

    fn open_pulls(&self, pulls: Vec<Value>) {
        self.set(
            &format!("repos/{TAP}/pulls?state=open&per_page=100"),
            Value::Array(pulls),
        );
    }

    fn ran(&self) -> Vec<String> {
        self.runs.borrow().iter().map(|r| r.join(" ")).collect()
    }
}

impl Gh for FakeGh {
    fn api(&self, path: &str) -> Result<Option<Value>, Error> {
        Ok(self.api.borrow().get(path).cloned())
    }

    fn run(&self, args: &[String]) -> Result<String, Error> {
        self.runs.borrow_mut().push(args.to_vec());
        let joined = args.join(" ");
        if let Some((_, message)) = self
            .failures
            .borrow()
            .iter()
            .find(|(text, _)| joined.contains(text))
        {
            return Err(Error::Failed(message.clone()));
        }
        if joined.contains(&format!("repos/{TAP}/pulls")) {
            let printed = self.opened.borrow().clone();
            return Ok(printed.unwrap_or_else(|| format!("{PR_URL}\n")));
        }
        Ok(String::new())
    }
}

struct Fixture {
    _scratch: TestDir,
    formula_file: PathBuf,
}

impl Fixture {
    fn new(name: &str, text: &str) -> Fixture {
        let scratch = scratch(name);
        let formula_file = scratch.join("tierstore.rb");
        std::fs::write(&formula_file, text).expect("formula");
        Fixture {
            _scratch: scratch,
            formula_file,
        }
    }

    fn request(&self) -> Request<'_> {
        Request {
            version: "1.2.3",
            formula_file: &self.formula_file,
            repo: REPO,
            tap_repo: TAP,
            base: "main",
        }
    }
}

fn release() -> Fixture {
    Fixture::new("release", &formula_of("1.2.3"))
}

#[test]
fn a_release_opens_the_pull_request_the_guard_accepts_and_asks_for_auto_merge() {
    // Catches: a branch, a path, a title or a base the guard refuses (`bump/tierstore-v1.2.3`,
    // the one file `Formula/tierstore.rb`), a formula changed on the way, a modification that
    // does not name the blob it replaces (GitHub refuses it), and an auto-merge never asked for.
    let f = release();
    let gh = FakeGh::new();
    let said = tap(&gh, &f.request()).expect("opened");
    let text = formula_of("1.2.3");
    let body = format!(
        "Formula/tierstore.rb for the release v1.2.3 of {REPO}, rendered by `channels formula` from the SHA256SUMS the release pipeline verified.\n\nRelease: https://github.com/{REPO}/releases/tag/v1.2.3\n\nOpened by the release pipeline of {REPO} after it published the release. The only change is Formula/tierstore.rb."
    );
    assert_eq!(
        gh.ran(),
        [
            format!(
                "api -X POST repos/{TAP}/git/refs -f ref=refs/heads/bump/tierstore-v1.2.3 -f sha=basesha"
            ),
            format!(
                "api -X PUT repos/{TAP}/contents/Formula/tierstore.rb -f message=tierstore 1.2.3 -f content={} -f branch=bump/tierstore-v1.2.3 -f sha=blobsha",
                STANDARD.encode(&text)
            ),
            format!(
                "api -X POST repos/{TAP}/pulls -f title=tierstore 1.2.3 -f head=bump/tierstore-v1.2.3 -f base=main -f body={body} --jq .html_url"
            ),
            format!("pr merge {PR_URL} -R {TAP} --auto --squash"),
        ]
    );
    assert_eq!(
        said,
        [
            format!("opened {PR_URL}: tierstore 1.2.3"),
            format!("auto-merge (squash) is on for {PR_URL}"),
        ]
    );
}

#[test]
fn the_first_formula_of_a_tap_is_added_without_a_blob_to_replace() {
    // Catches: a create that names a blob that does not exist.
    let f = release();
    let gh = FakeGh::new();
    gh.unset(&format!("repos/{TAP}/contents/{PATH}?ref=main"));
    tap(&gh, &f.request()).expect("opened");
    let put = &gh.ran()[1];
    assert!(put.starts_with("api -X PUT "), "{put}");
    assert!(!put.contains("-f sha="), "{put}");
}

#[test]
fn a_missing_tag_a_wrong_formula_or_a_tap_ahead_is_refused_before_anything_is_written() {
    // Catches: a pull request the guard would leave stuck (no tag, a formula of another
    // version), and a tap that moves down.
    let f = release();
    let gh = FakeGh::new();
    gh.unset(&format!("repos/{REPO}/git/ref/tags/v1.2.3"));
    assert_eq!(
        tap(&gh, &f.request()).unwrap_err(),
        Error::Failed(format!(
            "the tag v1.2.3 does not exist in {REPO}: the tap's guard would refuse the pull request"
        ))
    );

    let other = Fixture::new("wrong-version", &formula_of("1.2.4"));
    let e = tap(&FakeGh::new(), &other.request()).unwrap_err();
    assert_eq!(
        e,
        Error::Failed(format!(
            "{}: is not the formula of 1.2.3 of {REPO} that `channels formula` writes",
            other.formula_file.display()
        ))
    );

    let ahead = FakeGh::new();
    ahead.set(
        &format!("repos/{TAP}/contents/{PATH}?ref=main"),
        file_json(&formula_of("1.10.0")),
    );
    assert_eq!(
        tap(&ahead, &release().request()).unwrap_err(),
        Error::Failed(format!(
            "refusing to open a pull request for v1.2.3: {TAP} already has v1.10.0 (a tap never moves down)"
        ))
    );
    assert!(gh.ran().is_empty() && ahead.ran().is_empty());
}

#[test]
fn a_tap_already_at_the_version_finishes_quietly() {
    // Catches: a re-run after the pull request merged failing, or opening a second one.
    let f = release();
    let gh = FakeGh::new();
    gh.set(
        &format!("repos/{TAP}/contents/{PATH}?ref=main"),
        file_json(&formula_of("1.2.3")),
    );
    assert_eq!(
        tap(&gh, &f.request()).expect("quiet"),
        [format!(
            "{PATH} on main of {TAP} is already at v1.2.3: nothing to open"
        )]
    );
    assert!(gh.ran().is_empty());
}

#[test]
fn an_open_pull_request_for_the_version_is_not_opened_again() {
    // Catches: a re-run opening a second pull request, and one that leaves auto-merge off when
    // the first run's request for it was refused or lost.
    let f = release();
    let gh = FakeGh::new();
    gh.open_pulls(vec![pull(7, "1.2.3", false)]);
    let said = tap(&gh, &f.request()).expect("quiet");
    assert_eq!(
        gh.ran(),
        [format!("pr merge {PR_URL} -R {TAP} --auto --squash")]
    );
    assert_eq!(
        said,
        [
            format!("auto-merge (squash) is on for {PR_URL}"),
            format!(
                "a bump pull request for v1.2.3 or newer is open ({PR_URL}, 1.2.3): nothing to open"
            ),
        ]
    );

    let gh = FakeGh::new();
    gh.open_pulls(vec![pull(7, "1.2.3", true)]);
    let said = tap(&gh, &f.request()).expect("quiet");
    assert!(gh.ran().is_empty());
    assert_eq!(said.len(), 1);
}

#[test]
fn a_newer_open_pull_request_covers_the_release_and_older_ones_are_closed() {
    // Catches: an older release's pull request opened beside a newer one (the guard refuses it
    // as not higher), and older pull requests left open for ever.
    let f = release();
    let gh = FakeGh::new();
    gh.open_pulls(vec![pull(8, "1.3.0", false), pull(5, "1.2.1", false)]);
    let said = tap(&gh, &f.request()).expect("covered");
    assert_eq!(
        gh.ran(),
        [format!(
            "pr close 5 -R {TAP} --delete-branch --comment Superseded by the bump to v1.2.3."
        )]
    );
    assert_eq!(
        said,
        [
            format!(
                "a bump pull request for v1.2.3 or newer is open (https://github.com/{TAP}/pull/8, 1.3.0): nothing to open"
            ),
            format!("closed https://github.com/{TAP}/pull/5, superseded by v1.2.3"),
        ]
    );
}

#[test]
fn older_open_pull_requests_are_closed_when_this_one_is_opened_and_a_failed_close_is_a_notice() {
    // Catches: a stale pull request left beside the new one, and a release failing because a
    // maintainer closed the stale one first.
    let f = release();
    let gh = FakeGh::new();
    gh.open_pulls(vec![
        pull(5, "1.2.1", false),
        json!({"number": 6, "html_url": "x", "head": {"ref": "feature/other"}}),
        json!({"number": 9, "html_url": "x", "head": {"ref": "bump/tierstore-v1.2"}}),
    ]);
    gh.fail_when("pr close", "already closed");
    let said = tap(&gh, &f.request()).expect("opened");
    assert_eq!(gh.ran().len(), 5);
    assert!(gh.ran()[4].starts_with("pr close 5 "));
    assert_eq!(
        said[2],
        format!(
            "::notice title=An older tap pull request stays open::https://github.com/{TAP}/pull/5 could not be closed (already closed); close it by hand."
        )
    );
}

#[test]
fn a_tap_that_cannot_auto_merge_gets_a_notice_and_keeps_the_pull_request() {
    // Catches: a release red because the tap has no required checks yet.
    let f = release();
    let gh = FakeGh::new();
    gh.fail_when("pr merge", "auto merge is not allowed for this repository");
    let said = tap(&gh, &f.request()).expect("opened");
    assert_eq!(
        said[1],
        format!(
            "::notice title=The tap pull request is open without auto-merge::{PR_URL} stays open: GitHub refused auto-merge on it (auto merge is not allowed for this repository). A tap without required checks cannot auto-merge; squash-merge it by hand."
        )
    );
}

#[test]
fn a_branch_an_earlier_run_made_is_reused_when_it_holds_these_bytes_and_refused_otherwise() {
    // Catches: a re-run that fails at the branch it made itself, and one that opens a pull
    // request for bytes it did not write.
    let f = release();
    let contents = format!("repos/{TAP}/contents/{PATH}?ref=bump/tierstore-v1.2.3");
    let head = format!("repos/{TAP}/git/ref/heads/bump/tierstore-v1.2.3");
    let gh = FakeGh::new();
    gh.set(&head, json!({"object": {"sha": "committed"}}));
    gh.set(&contents, file_json(&formula_of("1.2.3")));
    tap(&gh, &f.request()).expect("reused");
    assert_eq!(gh.ran().len(), 2);
    assert!(gh.ran()[0].starts_with("api -X POST repos/komira-ai/homebrew-tap/pulls "));

    let gh = FakeGh::new();
    gh.set(&head, json!({"object": {"sha": "committed"}}));
    gh.set(&contents, file_json(&formula_of("1.2.4")));
    let refused = Error::Failed(format!(
        "the branch bump/tierstore-v1.2.3 exists in {TAP} with another {PATH} than this release's; delete the branch by hand"
    ));
    assert_eq!(tap(&gh, &f.request()).unwrap_err(), refused);
    assert!(gh.ran().is_empty());

    // A branch with a ref but no formula on it is not ours either.
    let gh = FakeGh::new();
    gh.set(&head, json!({"object": {"sha": "committed"}}));
    assert_eq!(tap(&gh, &f.request()).unwrap_err(), refused);
    assert!(gh.ran().is_empty());
}

#[test]
fn a_branch_left_at_the_base_tip_by_a_failed_commit_gets_the_formula_on_the_next_run() {
    // Catches: a run whose contents PUT failed leaving the branch at the base's tip with no
    // commit, and the next run refusing it as holding "another formula" (the base has one) or
    // failing the ref POST with a 422 (the first release, the base has none).
    let f = release();
    let head = format!("repos/{TAP}/git/ref/heads/bump/tierstore-v1.2.3");
    let ref_post = format!("api -X POST repos/{TAP}/git/refs");
    let put = format!("api -X PUT repos/{TAP}/contents/{PATH}");

    // The first run: the ref is made and the PUT fails.
    let gh = FakeGh::new();
    gh.fail_when("-X PUT", "502 Bad Gateway");
    assert_eq!(
        tap(&gh, &f.request()).unwrap_err(),
        Error::Failed("502 Bad Gateway".to_owned())
    );
    assert_eq!(gh.ran().len(), 2);

    // The next run, with the base holding a formula: the contents of the branch are the base's.
    let gh = FakeGh::new();
    gh.set(&head, json!({"object": {"sha": "basesha"}}));
    gh.set(
        &format!("repos/{TAP}/contents/{PATH}?ref=bump/tierstore-v1.2.3"),
        file_json(&formula_of("1.2.2")),
    );
    gh.fail_when("git/refs", "Reference already exists");
    let said = tap(&gh, &f.request()).expect("recovered");
    let ran = gh.ran();
    assert!(ran[0].starts_with(&put), "{ran:?}");
    assert!(
        ran[0].ends_with("-f branch=bump/tierstore-v1.2.3 -f sha=blobsha"),
        "{ran:?}"
    );
    assert!(ran[1].starts_with("api -X POST repos/komira-ai/homebrew-tap/pulls "));
    assert!(ran.iter().all(|r| !r.starts_with(&ref_post)), "{ran:?}");
    assert_eq!(said[0], format!("opened {PR_URL}: tierstore 1.2.3"));

    // The first release of a tap: the base has no formula, the branch has none, and no blob
    // is named.
    let gh = FakeGh::new();
    gh.unset(&format!("repos/{TAP}/contents/{PATH}?ref=main"));
    gh.set(&head, json!({"object": {"sha": "basesha"}}));
    gh.fail_when("git/refs", "Reference already exists");
    tap(&gh, &f.request()).expect("recovered");
    let ran = gh.ran();
    assert!(ran[0].starts_with(&put), "{ran:?}");
    assert!(!ran[0].contains("-f sha="), "{ran:?}");
    assert!(ran[1].starts_with("api -X POST repos/komira-ai/homebrew-tap/pulls "));

    // The PUT fails again: the failure is reported and no pull request is opened.
    let gh = FakeGh::new();
    gh.set(&head, json!({"object": {"sha": "basesha"}}));
    gh.fail_when("-X PUT", "502 Bad Gateway");
    assert_eq!(
        tap(&gh, &f.request()).unwrap_err(),
        Error::Failed("502 Bad Gateway".to_owned())
    );
    assert_eq!(gh.ran().len(), 1);
}

#[test]
fn what_github_answers_that_is_not_usable_is_a_failure_that_says_so() {
    // Catches: a pull request opened from a branch that was never made, a pull request whose
    // url was not printed, and a tap whose formula cannot be read.
    let f = release();
    let gh = FakeGh::new();
    gh.unset(&format!("repos/{TAP}/git/ref/heads/main"));
    assert_eq!(
        tap(&gh, &f.request()).unwrap_err(),
        Error::Failed(format!("{TAP} has no branch main"))
    );

    let gh = FakeGh::new();
    *gh.opened.borrow_mut() = Some("nothing\n".to_owned());
    assert_eq!(
        tap(&gh, &f.request()).unwrap_err(),
        Error::Failed("opening the pull request printed \"nothing\", not its url".to_owned())
    );

    let gh = FakeGh::new();
    gh.fail_when("git/refs", "Reference already exists");
    assert_eq!(
        tap(&gh, &f.request()).unwrap_err(),
        Error::Failed("Reference already exists".to_owned())
    );

    let live = format!("repos/{TAP}/contents/{PATH}?ref=main");
    for (value, why) in [
        (json!({"content": "***", "sha": "s"}), "is not base64"),
        (
            json!({"content": STANDARD.encode([0xff_u8, 0xfe]), "sha": "s"}),
            "is not text",
        ),
        (
            json!({"sha": "s"}),
            &format!("{PATH} on main of {TAP}: it has no url line"),
        ),
        (
            file_json(&formula_of("99999999999999999999.0.0")),
            &format!("{PATH} on main of {TAP}: a number too large"),
        ),
    ] {
        let gh = FakeGh::new();
        gh.set(&live, value);
        let Error::Failed(said) = tap(&gh, &f.request()).unwrap_err() else {
            panic!("failed")
        };
        assert!(said.contains(why), "{said} / {why}");
    }
}

#[test]
fn a_listed_pull_request_without_a_number_or_url_is_a_failure() {
    // Catches: a pull request we cannot close or ask auto-merge on being skipped as if absent.
    let f = release();
    let gh = FakeGh::new();
    gh.open_pulls(vec![json!({"head": {"ref": "bump/tierstore-v1.0.0"}})]);
    assert_eq!(
        tap(&gh, &f.request()).unwrap_err(),
        Error::Failed("the pull request of bump/tierstore-v1.0.0 has no number or url".to_owned())
    );
    // A tap that lists nothing at all (404) has nothing open.
    let gh = FakeGh::new();
    gh.unset(&format!("repos/{TAP}/pulls?state=open&per_page=100"));
    assert!(tap(&gh, &f.request()).is_ok());
}

#[test]
fn flags_out_of_their_form_and_a_formula_file_that_is_not_there_fail() {
    // Catches: a tap of another owner (the guard builds the urls from the tap's owner), a
    // version that is not a release's, and a missing file.
    let f = release();
    let gh = FakeGh::new();
    type Change = fn(&mut Request);
    let cases: [(Change, &str); 7] = [
        (
            |r| r.version = "1.2",
            "--version \"1.2\" is not <major>.<minor>.<patch>",
        ),
        (
            |r| r.repo = "komira-ai/other",
            "--repo \"komira-ai/other\" is not <owner>/tierstore",
        ),
        (
            |r| r.tap_repo = "other/homebrew-tap",
            "--tap-repo \"other/homebrew-tap\" is not <komira-ai>/<name>: the tap's guard builds the urls from the tap's owner",
        ),
        (
            |r| r.tap_repo = "komira-ai/",
            "--tap-repo \"komira-ai/\" is not <komira-ai>/<name>: the tap's guard builds the urls from the tap's owner",
        ),
        (
            |r| r.tap_repo = "komira-ai/a/b",
            "--tap-repo \"komira-ai/a/b\" is not <komira-ai>/<name>: the tap's guard builds the urls from the tap's owner",
        ),
        (|r| r.base = "", "--base \"\" is not a branch"),
        (|r| r.base = "a b", "--base \"a b\" is not a branch"),
    ];
    for (change, want) in cases {
        let mut request = f.request();
        change(&mut request);
        assert_eq!(
            tap(&gh, &request).unwrap_err(),
            Error::Usage(want.to_owned())
        );
    }
    let missing = scratch("missing").join("none.rb");
    let mut request = f.request();
    request.formula_file = &missing;
    assert!(matches!(
        tap(&gh, &request).unwrap_err(),
        Error::Spawn { .. }
    ));
    assert!(gh.ran().is_empty());
}

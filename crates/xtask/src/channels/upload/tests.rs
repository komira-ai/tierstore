use std::cell::RefCell;
use std::collections::BTreeMap;

use super::*;
use crate::testdir::TestDir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const BUCKET: &str = "site-bucket";
const PRECONDITION: &str = "An error occurred (PreconditionFailed) when calling the PutObject operation: At least one of the pre-conditions you specified did not hold";
const DENIED: &str =
    "An error occurred (AccessDenied) when calling the GetObject operation: Access Denied";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside.parent().expect("deps").join("xtask-upload-unit"),
        name,
    )
}

fn refused(stderr: &str) -> Answer {
    Answer {
        ok: false,
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

type Store = BTreeMap<String, Vec<u8>>;

/// What another release does to the store between a read and a write.
type Hook = Box<dyn FnOnce(&mut Store)>;

/// A change to a request, to put one flag out of its form.
type Change = fn(&mut Request);

/// A stand-in S3 and CloudFront: objects in memory under `bucket/key`, the conditions of
/// `put-object` judged as S3 judges them, every call recorded.
#[derive(Default)]
struct FakeAws {
    store: RefCell<Store>,
    calls: RefCell<Vec<Vec<String>>>,
    /// Run once each, in this order, just before a `put-object` of `latest.json` is judged.
    before_latest_put: RefCell<Vec<Hook>>,
    /// An answer for every call whose arguments, joined by spaces, contain the text.
    refusals: RefCell<Vec<(String, String)>>,
    /// What `get-object` of `latest.json` prints instead of its ETag.
    latest_stdout: RefCell<Option<String>>,
}

fn etag(bytes: &[u8]) -> String {
    format!("\"{}\"", Sha256::of(bytes))
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1).cloned()
}

impl FakeAws {
    fn put(&self, key: &str, bytes: &[u8]) {
        self.store
            .borrow_mut()
            .insert(format!("{BUCKET}/{key}"), bytes.to_vec());
    }

    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.store.borrow().get(&format!("{BUCKET}/{key}")).cloned()
    }

    fn refuse_when(&self, text: &str, stderr: &str) {
        self.refusals
            .borrow_mut()
            .push((text.to_owned(), stderr.to_owned()));
    }

    /// Before the next put of `latest.json` is judged, another release writes `version`.
    fn another_release_writes(&self, version: &str) {
        let text = format!("{{\"version\": \"{version}\"}}");
        self.before_latest_put
            .borrow_mut()
            .push(Box::new(move |store| {
                store.insert(format!("{BUCKET}/{LATEST_KEY}"), text.into_bytes());
            }));
    }

    fn puts(&self) -> Vec<Vec<String>> {
        self.calls_of("put-object")
    }

    fn keys_put(&self) -> Vec<String> {
        self.puts()
            .iter()
            .map(|c| flag(c, "--key").expect("a key"))
            .collect()
    }

    fn calls_of(&self, verb: &str) -> Vec<Vec<String>> {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c[1] == verb)
            .cloned()
            .collect()
    }
}

impl Aws for FakeAws {
    fn run(&self, args: &[String]) -> Result<Answer, Error> {
        self.calls.borrow_mut().push(args.to_vec());
        let joined = args.join(" ");
        if let Some((_, stderr)) = self
            .refusals
            .borrow()
            .iter()
            .find(|(text, _)| joined.contains(text))
        {
            return Ok(refused(stderr));
        }
        let ok = |stdout: &str| {
            Ok(Answer {
                ok: true,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            })
        };
        match (args[0].as_str(), args[1].as_str()) {
            ("s3api", "put-object") => {
                let key = flag(args, "--key").expect("--key");
                let full = format!("{BUCKET}/{key}");
                assert_eq!(flag(args, "--bucket").as_deref(), Some(BUCKET));
                if key == LATEST_KEY {
                    let hook = {
                        let mut hooks = self.before_latest_put.borrow_mut();
                        (!hooks.is_empty()).then(|| hooks.remove(0))
                    };
                    if let Some(hook) = hook {
                        hook(&mut self.store.borrow_mut());
                    }
                }
                let stored = self.store.borrow().get(&full).cloned();
                if flag(args, "--if-none-match").as_deref() == Some("*") && stored.is_some() {
                    return Ok(refused(PRECONDITION));
                }
                if let Some(wanted) = flag(args, "--if-match")
                    && stored.as_deref().map(etag) != Some(wanted)
                {
                    return Ok(refused(PRECONDITION));
                }
                let body = std::fs::read(flag(args, "--body").expect("--body")).expect("body");
                self.store.borrow_mut().insert(full, body);
                ok("{}")
            }
            ("s3api", "get-object") => {
                let key = flag(args, "--key").expect("--key");
                let stored = self.store.borrow().get(&format!("{BUCKET}/{key}")).cloned();
                let Some(bytes) = stored else {
                    return Ok(refused(
                        "An error occurred (NoSuchKey) when calling the GetObject operation: The specified key does not exist.",
                    ));
                };
                std::fs::write(args.last().expect("outfile"), &bytes).expect("write");
                let printed = if key == LATEST_KEY {
                    self.latest_stdout.borrow().clone()
                } else {
                    None
                };
                ok(&printed.unwrap_or_else(|| format!("{{\"ETag\": {:?}}}", etag(&bytes))))
            }
            ("cloudfront", "create-invalidation") => ok("{}"),
            other => panic!("unexpected aws call {other:?}"),
        }
    }
}

/// `dir` as the `release-assets` artifact of the release `version`: the tap-named tarballs and
/// Debian packages, the bundle of each, and a `SHA256SUMS` that lists the files but not the
/// bundles (and one commit-named file the upload has no business with).
fn release_dir(dir: &Path, version: &str) {
    let mut lines = vec![sums::Line {
        sha256: Sha256::of(b"commit-named"),
        name: format!("tierstore-{COMMIT}-linux-x86_64.tar.gz"),
    }];
    for name in distribution_assets(version) {
        let bytes = format!("bytes of {name}");
        std::fs::write(dir.join(&name), &bytes).expect("file");
        std::fs::write(
            dir.join(format!("{name}{SUFFIX}")),
            format!("bundle of {name}"),
        )
        .expect("bundle");
        lines.push(sums::Line {
            sha256: Sha256::of(bytes.as_bytes()),
            name,
        });
    }
    std::fs::write(dir.join(SUMS), sums::render(&lines)).expect("sums");
}

/// The release of 1.2.3 in a directory of its own, and the work directory beside it.
struct Fixture {
    _scratch: TestDir,
    release: PathBuf,
    work: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let scratch = scratch(name);
        let release = scratch.join("release");
        std::fs::create_dir(&release).expect("release");
        release_dir(&release, "1.2.3");
        let work = scratch.join("work");
        Fixture {
            _scratch: scratch,
            release,
            work,
        }
    }

    fn request<'a>(&'a self, distribution_id: &'a str) -> Request<'a> {
        Request {
            dir: &self.release,
            tag: "v1.2.3",
            commit: COMMIT,
            bucket: BUCKET,
            distribution_id,
            work: &self.work,
            attempts: 3,
            latest: true,
        }
    }
}

/// The names of the files of 1.2.3, sorted.
fn names() -> Vec<String> {
    let mut names = distribution_assets("1.2.3");
    names.extend(names.clone().iter().map(|n| format!("{n}{SUFFIX}")));
    names.sort();
    names
}

fn json(bytes: Vec<u8>) -> Value {
    serde_json::from_slice(&bytes).expect("json")
}

#[test]
fn the_first_release_writes_each_file_once_and_latest_json_last() {
    // Catches: a file put without If-None-Match (the role refuses it), a missing file of the
    // version, and latest.json written before the files it points at.
    let f = Fixture::new("first");
    let aws = FakeAws::default();
    let said = upload(&aws, &f.request("EDIST123")).expect("uploaded");
    let mut want: Vec<String> = names()
        .iter()
        .map(|n| format!("releases/1.2.3/{n}"))
        .collect();
    want.push("releases/1.2.3/SHA256SUMS".to_owned());
    want.push(LATEST_KEY.to_owned());
    assert_eq!(aws.keys_put(), want);
    assert_eq!(said.len(), want.len() + 1);
    assert_eq!(said[0], format!("{}: written", want[0]));
    assert_eq!(said[said.len() - 2], format!("{LATEST_KEY}: now 1.2.3"));
    assert_eq!(said[said.len() - 1], "/releases/latest.json: invalidated");
    for call in aws.puts() {
        let key = flag(&call, "--key").expect("key");
        assert_eq!(call[0], "s3api");
        assert_eq!(
            flag(&call, "--if-none-match").as_deref(),
            Some("*"),
            "{key}: every first write is conditional"
        );
        let cache = if key == LATEST_KEY {
            LATEST_CACHE
        } else {
            IMMUTABLE
        };
        assert_eq!(
            flag(&call, "--cache-control").as_deref(),
            Some(cache),
            "{key}"
        );
    }
    assert_eq!(
        aws.calls_of("create-invalidation"),
        [[
            "cloudfront",
            "create-invalidation",
            "--distribution-id",
            "EDIST123",
            "--paths",
            "/releases/latest.json"
        ]]
    );
}

#[test]
fn the_sums_of_the_version_list_exactly_its_files_and_latest_json_lists_them_with_sizes() {
    // Catches: a SHA256SUMS that lists a file the directory lacks or the commit-named file of the
    // release, a latest.json that omits a file, lists SHA256SUMS or has a wrong size or digest,
    // and one that is not the release's version, tag and commit.
    let f = Fixture::new("contents");
    let aws = FakeAws::default();
    upload(&aws, &f.request("")).expect("uploaded");
    let sums_text =
        String::from_utf8(aws.get("releases/1.2.3/SHA256SUMS").expect("sums")).expect("text");
    let listed: Vec<&str> = sums_text
        .lines()
        .map(|l| l.split_once("  ").expect("line").1)
        .collect();
    assert_eq!(listed, names());
    assert!(sums_text.contains(&format!(
        "{}  tierstore-1.2.3-macos-arm64.tar.gz\n",
        Sha256::of(b"bytes of tierstore-1.2.3-macos-arm64.tar.gz")
    )));
    let latest = json(aws.get(LATEST_KEY).expect("latest"));
    assert_eq!(latest["version"], "1.2.3");
    assert_eq!(latest["tag"], "v1.2.3");
    assert_eq!(latest["commit"], COMMIT);
    let files = latest["files"].as_object().expect("files");
    assert_eq!(files.keys().cloned().collect::<Vec<_>>(), names());
    let one = &files["tierstore-1.2.3-linux-x86_64.tar.gz"];
    assert_eq!(
        one["size"],
        "bytes of tierstore-1.2.3-linux-x86_64.tar.gz".len()
    );
    assert_eq!(
        one["sha256"],
        Sha256::of(b"bytes of tierstore-1.2.3-linux-x86_64.tar.gz").to_string()
    );
    // No distribution given: nothing is invalidated.
    assert!(aws.calls_of("create-invalidation").is_empty());
}

#[test]
fn a_rerun_with_the_same_bytes_passes_and_writes_nothing_new() {
    // Catches: a re-run of a half-finished job failing on the files it already wrote, and one
    // that writes latest.json again when it is at this version.
    let f = Fixture::new("rerun");
    let aws = FakeAws::default();
    upload(&aws, &f.request("EDIST123")).expect("first");
    let before = aws.store.borrow().clone();
    let said = upload(&aws, &f.request("EDIST123")).expect("second");
    assert_eq!(*aws.store.borrow(), before);
    assert!(
        said[..said.len() - 2]
            .iter()
            .all(|l| l.ends_with(": already there with the same bytes")),
        "{said:?}"
    );
    assert_eq!(
        said[said.len() - 2],
        format!("{LATEST_KEY}: already at 1.2.3")
    );
    // The invalidation is asked for again: the first run may have stopped before it.
    assert_eq!(said[said.len() - 1], "/releases/latest.json: invalidated");
    // The only put of latest.json is the first run's.
    assert_eq!(
        aws.keys_put().iter().filter(|k| *k == LATEST_KEY).count(),
        1
    );
}

#[test]
fn a_different_file_at_a_versions_key_fails_and_is_never_written_over() {
    // Catches: a 412 taken for success whatever the stored bytes are, and a file of a version
    // replaced.
    let f = Fixture::new("different");
    let aws = FakeAws::default();
    let key = "releases/1.2.3/tierstore-1.2.3-linux-x86_64.tar.gz";
    aws.put(key, b"other bytes");
    let Error::Failed(why) = upload(&aws, &f.request("EDIST123")).unwrap_err() else {
        panic!("failed")
    };
    assert!(
        why.starts_with(&format!("{key} exists with other bytes (sha256 ")),
        "{why}"
    );
    assert!(
        why.ends_with("a file of a version is never replaced"),
        "{why}"
    );
    assert_eq!(aws.get(key).expect("kept"), b"other bytes");
    // latest.json is not written: no pointer to a version that does not hold.
    assert_eq!(aws.get(LATEST_KEY), None);
    assert!(aws.calls_of("create-invalidation").is_empty());
}

#[test]
fn a_412_whose_file_cannot_be_read_back_fails() {
    // Catches: a re-run check that passes when the role may not read the file back.
    let f = Fixture::new("unreadable");
    let aws = FakeAws::default();
    let key = "releases/1.2.3/tierstore-1.2.3-linux-x86_64.tar.gz";
    aws.put(key, b"x");
    aws.refuse_when(&format!("get-object --bucket {BUCKET} --key {key}"), DENIED);
    let e = upload(&aws, &f.request("")).unwrap_err();
    assert_eq!(
        e,
        Error::Failed(format!(
            "{key} exists, and reading it back failed: {DENIED}"
        ))
    );
}

#[test]
fn a_refusal_that_is_not_a_412_fails_with_the_answer_and_stops() {
    // Catches: a role condition AWS reads differently from the policy being taken for a re-run,
    // and the files after it still being put.
    let f = Fixture::new("denied");
    let aws = FakeAws::default();
    aws.refuse_when(
        "tierstore-1.2.3-linux-aarch64.tar.gz --body",
        "Access Denied\n",
    );
    let e = upload(&aws, &f.request("")).unwrap_err();
    assert_eq!(
        e,
        Error::Failed(
            "put releases/1.2.3/tierstore-1.2.3-linux-aarch64.tar.gz: Access Denied".to_owned()
        )
    );
    assert_eq!(
        aws.keys_put().last().map(String::as_str),
        Some("releases/1.2.3/tierstore-1.2.3-linux-aarch64.tar.gz")
    );
}

#[test]
fn a_file_that_is_not_what_the_release_sums_list_is_refused_before_anything_is_put() {
    // Catches: bytes that crossed the artifact store changed, a file missing, and a tarball the
    // sums do not list, each uploaded anyway.
    let f = Fixture::new("sums");
    let tarball = f.release.join("tierstore-1.2.3-macos-arm64.tar.gz");
    std::fs::write(&tarball, "changed").expect("change");
    std::fs::remove_file(f.release.join("tierstore_1.2.3_arm64.deb")).expect("remove");
    let sums_path = f.release.join(SUMS);
    let kept: String = std::fs::read_to_string(&sums_path)
        .expect("sums")
        .lines()
        .filter(|l| !l.ends_with("tierstore-1.2.3-linux-aarch64.tar.gz"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&sums_path, kept).expect("sums");
    let aws = FakeAws::default();
    let Error::Refused(problems) = upload(&aws, &f.request("")).unwrap_err() else {
        panic!("refused")
    };
    assert_eq!(problems.len(), 3, "{problems:?}");
    let missing = f.release.join("tierstore_1.2.3_arm64.deb");
    assert!(problems.contains(&format!("{} is not a file", missing.display())));
    assert!(
        problems
            .contains(&"SHA256SUMS does not list tierstore-1.2.3-linux-aarch64.tar.gz".to_owned())
    );
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("tierstore-1.2.3-macos-arm64.tar.gz is ")
                && p.contains(", but SHA256SUMS lists ")),
        "{problems:?}"
    );
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn a_newer_latest_json_is_left_alone() {
    // Catches: an older release finishing late, or a re-run of an older release, moving
    // latest.json back.
    let f = Fixture::new("newer");
    let aws = FakeAws::default();
    aws.put(LATEST_KEY, br#"{"version": "1.10.0"}"#);
    let said = upload(&aws, &f.request("EDIST123")).expect("uploaded");
    assert_eq!(
        said.last().map(String::as_str),
        Some("releases/latest.json: at 1.10.0, newer than 1.2.3: left as it is")
    );
    assert_eq!(
        aws.get(LATEST_KEY).expect("kept"),
        br#"{"version": "1.10.0"}"#
    );
    assert!(aws.calls_of("create-invalidation").is_empty());
}

#[test]
fn an_older_latest_json_is_replaced_under_if_match_on_the_etag_that_was_read() {
    // Catches: a replacement that is not conditional on the pointer that was read (a lost
    // update), and a version compared as text (1.2.3 is above 1.1.9 and 1.10.0 above 1.9.0).
    for (n, stored) in ["1.2.2", "1.1.9"].into_iter().enumerate() {
        let f = Fixture::new(&format!("older-{n}"));
        let aws = FakeAws::default();
        let text = format!("{{\"version\": \"{stored}\"}}");
        aws.put(LATEST_KEY, text.as_bytes());
        upload(&aws, &f.request("")).expect("uploaded");
        let put = aws.puts().pop().expect("a put");
        assert_eq!(flag(&put, "--key").as_deref(), Some(LATEST_KEY));
        assert_eq!(flag(&put, "--if-match"), Some(etag(text.as_bytes())));
        assert_eq!(flag(&put, "--if-none-match"), None);
        assert_eq!(
            json(aws.get(LATEST_KEY).expect("latest"))["version"],
            "1.2.3"
        );
    }
}

#[test]
fn a_concurrent_write_of_latest_json_makes_the_verb_read_and_decide_again() {
    // Catches: a 412 on latest.json failing the job when the other release is older (the retry
    // writes ours), and a retry that writes over a newer release that won the race.
    let f = Fixture::new("race-older");
    let aws = FakeAws::default();
    aws.put(LATEST_KEY, br#"{"version": "1.0.0"}"#);
    aws.another_release_writes("1.2.0");
    upload(&aws, &f.request("")).expect("uploaded");
    let tries = aws
        .puts()
        .iter()
        .filter(|c| flag(c, "--key").as_deref() == Some(LATEST_KEY))
        .count();
    assert_eq!(tries, 2);
    assert_eq!(
        json(aws.get(LATEST_KEY).expect("latest"))["version"],
        "1.2.3"
    );

    let g = Fixture::new("race-newer");
    let aws = FakeAws::default();
    aws.put(LATEST_KEY, br#"{"version": "1.0.0"}"#);
    aws.another_release_writes("1.3.0");
    let said = upload(&aws, &g.request("")).expect("uploaded");
    assert_eq!(
        said.last().map(String::as_str),
        Some("releases/latest.json: at 1.3.0, newer than 1.2.3: left as it is")
    );
    assert_eq!(
        aws.get(LATEST_KEY).expect("kept"),
        br#"{"version": "1.3.0"}"#
    );
}

#[test]
fn a_latest_json_that_keeps_changing_fails_after_the_attempts() {
    // Catches: a retry loop with no end, and one that gives up silently.
    let f = Fixture::new("race-always");
    let aws = FakeAws::default();
    aws.put(LATEST_KEY, br#"{"version": "1.0.0"}"#);
    aws.another_release_writes("1.0.1");
    aws.another_release_writes("1.0.2");
    let mut request = f.request("");
    request.attempts = 2;
    let e = upload(&aws, &request).unwrap_err();
    assert_eq!(
        e,
        Error::Failed(
            "releases/latest.json was written by another release 2 times in a row; run the job again"
                .to_owned()
        )
    );
}

#[test]
fn latest_json_that_cannot_be_read_or_understood_fails() {
    // Catches: an unreadable pointer taken for a missing one (and so written over), and a
    // pointer without a version taken for older than every release.
    let no_version = "releases/latest.json in the bucket has no <major>.<minor>.<patch> version";
    // (what the bucket holds, what `get-object` prints, the failure)
    let cases: [(&str, Option<&str>, &str); 5] = [
        (
            "not json",
            None,
            "releases/latest.json in the bucket is not JSON",
        ),
        (r#"{"tag": "v1.0.0"}"#, None, no_version),
        (r#"{"version": "1.0"}"#, None, no_version),
        (
            r#"{"version": "1.0.0"}"#,
            Some("not json at all"),
            "read releases/latest.json: aws did not print JSON",
        ),
        (
            r#"{"version": "1.0.0"}"#,
            Some("{\"ContentLength\": 3}"),
            "read releases/latest.json: aws printed no ETag",
        ),
    ];
    for (n, (stored, printed, want)) in cases.into_iter().enumerate() {
        let f = Fixture::new(&format!("unreadable-latest-{n}"));
        let aws = FakeAws::default();
        aws.put(LATEST_KEY, stored.as_bytes());
        *aws.latest_stdout.borrow_mut() = printed.map(str::to_owned);
        let Error::Failed(why) = upload(&aws, &f.request("")).unwrap_err() else {
            panic!("{stored}")
        };
        assert!(why.starts_with(want), "{stored}: {why}");
    }
    let f = Fixture::new("unreadable-latest-denied");
    let aws = FakeAws::default();
    aws.refuse_when(
        &format!("get-object --bucket {BUCKET} --key {LATEST_KEY}"),
        DENIED,
    );
    assert_eq!(
        upload(&aws, &f.request("")).unwrap_err(),
        Error::Failed(format!("read releases/latest.json: {DENIED}"))
    );
}

#[test]
fn a_stored_version_too_large_to_compare_fails_and_so_does_a_tag_with_one() {
    // Catches: a number that overflows read as zero, which would make every release newer.
    let f = Fixture::new("huge-stored");
    let aws = FakeAws::default();
    aws.put(LATEST_KEY, br#"{"version": "99999999999999999999.0.0"}"#);
    assert_eq!(
        upload(&aws, &f.request("")).unwrap_err(),
        Error::Failed(
            "releases/latest.json in the bucket has no <major>.<minor>.<patch> version".to_owned()
        )
    );

    let g = Fixture::new("huge-tag");
    release_dir(&g.release, "99999999999999999999.0.0");
    let mut request = g.request("");
    request.tag = "v99999999999999999999.0.0";
    assert_eq!(
        upload(&FakeAws::default(), &request).unwrap_err(),
        Error::Usage("--tag \"v99999999999999999999.0.0\" holds a number too large".to_owned())
    );
}

#[test]
fn a_refused_write_of_latest_json_or_a_refused_invalidation_fails() {
    // Catches: a pointer that was not written, or a cache that was not told, ending green.
    let f = Fixture::new("latest-denied");
    let aws = FakeAws::default();
    aws.refuse_when("--key releases/latest.json --body", "Access Denied");
    assert_eq!(
        upload(&aws, &f.request("")).unwrap_err(),
        Error::Failed("put releases/latest.json: Access Denied".to_owned())
    );

    let g = Fixture::new("invalidation-denied");
    let aws = FakeAws::default();
    aws.refuse_when("create-invalidation", "no rights");
    assert_eq!(
        upload(&aws, &g.request("EDIST123")).unwrap_err(),
        Error::Failed("invalidate /releases/latest.json: no rights".to_owned())
    );

    // A re-run at the version already stored invalidates too, and fails the same way.
    let h = Fixture::new("invalidation-denied-again");
    let aws = FakeAws::default();
    upload(&aws, &h.request("")).expect("first");
    aws.refuse_when("create-invalidation", "no rights");
    assert_eq!(
        upload(&aws, &h.request("EDIST123")).unwrap_err(),
        Error::Failed("invalidate /releases/latest.json: no rights".to_owned())
    );
}

#[test]
fn flags_out_of_their_form_are_usage_errors_before_anything_is_read() {
    // Catches: a tag, commit, bucket, distribution or attempt count that is passed on to aws as
    // it came (an unset variable is an empty flag).
    let f = Fixture::new("usage");
    let aws = FakeAws::default();
    let cases: [(Change, &str); 8] = [
        (
            |r| r.tag = "1.2.3",
            "--tag \"1.2.3\" is not v<major>.<minor>.<patch>",
        ),
        (
            |r| r.tag = "v1.2",
            "--tag \"v1.2\" is not v<major>.<minor>.<patch>",
        ),
        (
            |r| r.commit = "abc",
            "--commit \"abc\" is not a full commit SHA (40 lowercase hex digits)",
        ),
        (|r| r.bucket = "", "--bucket \"\" is not a bucket name"),
        (
            |r| r.bucket = "Bucket/x",
            "--bucket \"Bucket/x\" is not a bucket name",
        ),
        (
            |r| r.distribution_id = "e1",
            "--distribution-id \"e1\" is not a distribution id",
        ),
        (|r| r.attempts = 0, "--attempts is 0; it must be 1 or more"),
        (
            |r| r.tag = "v01.2.3",
            "--tag \"v01.2.3\" is not v<major>.<minor>.<patch>",
        ),
    ];
    for (change, want) in cases {
        let mut request = f.request("");
        change(&mut request);
        assert_eq!(
            upload(&aws, &request).unwrap_err(),
            Error::Usage(want.to_owned())
        );
    }
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn a_directory_without_sums_or_a_work_directory_that_cannot_be_made_fails() {
    // Catches: an upload that goes on without the sums to hold the files to, and one that loses
    // the staged files' place.
    let f = Fixture::new("environment");
    std::fs::remove_file(f.release.join(SUMS)).expect("remove");
    let e = upload(&FakeAws::default(), &f.request("")).unwrap_err();
    assert!(matches!(e, Error::Spawn { .. }), "{e:?}");

    let g = Fixture::new("environment-work");
    std::fs::write(&g.work, "a file where the directory goes").expect("file");
    let e = upload(&FakeAws::default(), &g.request("")).unwrap_err();
    assert!(matches!(e, Error::Spawn { .. }), "{e:?}");
}

#[test]
fn each_file_is_served_as_what_it_is() {
    // Catches: a tarball served as text, or the sums served as a download.
    assert_eq!(content_type("SHA256SUMS"), "text/plain; charset=utf-8");
    assert_eq!(
        content_type("tierstore-1.2.3-macos-arm64.tar.gz.sigstore.json"),
        "application/json"
    );
    assert_eq!(
        content_type("tierstore_1.2.3_amd64.deb"),
        "application/vnd.debian.binary-package"
    );
    assert_eq!(
        content_type("tierstore-1.2.3-macos-arm64.tar.gz"),
        "application/gzip"
    );
}

#[test]
fn a_file_that_cannot_be_read_is_an_error_naming_it() {
    // Catches: a digest of nothing for a file that could not be opened or read.
    let dir = scratch("digest");
    let e = digest_file(&dir.join("missing")).unwrap_err();
    assert!(
        matches!(&e, Error::Spawn { what, .. } if what.starts_with("read ")),
        "{e:?}"
    );
    let e = digest_file(&dir).unwrap_err();
    assert!(matches!(e, Error::Spawn { .. }), "{e:?}");
    std::fs::write(dir.join("f"), "abc").expect("file");
    assert_eq!(
        digest_file(&dir.join("f")).expect("digest"),
        (Sha256::of(b"abc"), 3)
    );
}

#[test]
fn bucket_names_may_hold_digits_dots_and_dashes() {
    // Catches: a real bucket name (`tierstore-prod-site-123456789012`, or a dotted one) refused.
    let f = Fixture::new("bucket-names");
    for bucket in ["tierstore-prod-site-123456789012", "site.example.com"] {
        let mut request = f.request("");
        request.bucket = bucket;
        assert_eq!(check_request(&request), Ok("1.2.3".to_owned()), "{bucket}");
    }
}

/// A request of the fixture that does not replace `latest.json`.
fn without_latest(f: &Fixture) -> Request<'_> {
    Request {
        latest: false,
        ..f.request("")
    }
}

#[test]
fn a_release_published_with_no_latest_puts_its_files_and_never_names_it_the_newest() {
    // Catches: a publish to testing that moves releases/latest.json before the install checks.
    let f = Fixture::new("no-latest");
    let aws = FakeAws::default();
    let said = upload(&aws, &without_latest(&f)).expect("uploaded");
    let keys = aws.keys_put();
    assert_eq!(keys.len(), names().len() + 1);
    assert!(
        keys.iter().all(|k| k.starts_with("releases/1.2.3/")),
        "{keys:?}"
    );
    assert!(aws.get(LATEST_KEY).is_none());
    assert!(said.iter().all(|l| !l.contains("latest.json")), "{said:?}");
    assert!(aws.calls_of("create-invalidation").is_empty());
}

#[test]
fn latest_reads_the_published_files_back_and_then_replaces_the_pointer_and_puts_nothing_else() {
    // Catches: a promotion that uploads the files again (so not the bytes testing holds), or
    // that names the release before the files were read back.
    let f = Fixture::new("latest");
    let aws = FakeAws::default();
    upload(&aws, &without_latest(&f)).expect("published to testing");
    let before = aws.calls_of("put-object").len();
    let said = latest(&aws, &f.request("EDIST123")).expect("promoted");
    let puts = aws.puts().split_off(before);
    assert_eq!(puts.len(), 1);
    assert_eq!(flag(&puts[0], "--key").as_deref(), Some(LATEST_KEY));
    assert_eq!(said.len(), names().len() + 1 + 2);
    assert!(
        said[0].starts_with("releases/1.2.3/") && said[0].contains(": sha256 "),
        "{said:?}"
    );
    assert_eq!(said[said.len() - 2], format!("{LATEST_KEY}: now 1.2.3"));
    let reads: Vec<String> = aws
        .calls_of("get-object")
        .iter()
        .map(|c| flag(c, "--key").expect("key"))
        .filter(|k| k.starts_with("releases/1.2.3/"))
        .collect();
    assert!(
        reads.contains(&"releases/1.2.3/SHA256SUMS".to_owned()),
        "{reads:?}"
    );
    assert_eq!(
        json(aws.get(LATEST_KEY).expect("latest"))["version"],
        "1.2.3"
    );
}

#[test]
fn latest_refuses_a_file_that_is_not_in_the_bucket_or_holds_other_bytes_and_writes_no_pointer() {
    // Catches: a pointer to a version whose files are missing or are not the ones that were
    // checked against SHA256SUMS.
    let f = Fixture::new("latest-refused");
    let aws = FakeAws::default();
    let e = latest(&aws, &f.request("")).unwrap_err().to_string();
    assert!(e.contains("could not be read back"), "{e}");
    upload(&aws, &without_latest(&f)).expect("published to testing");
    let name = distribution_assets("1.2.3").remove(0);
    aws.put(&format!("releases/1.2.3/{name}"), b"other bytes");
    let e = latest(&aws, &f.request("")).unwrap_err().to_string();
    assert!(
        e.contains(&format!("releases/1.2.3/{name} holds sha256")),
        "{e}"
    );
    assert!(e.contains("but this release's"), "{e}");
    assert!(aws.get(LATEST_KEY).is_none());
    // The sums of the version are held to the same.
    let aws = FakeAws::default();
    upload(&aws, &without_latest(&f)).expect("published to testing");
    aws.put("releases/1.2.3/SHA256SUMS", b"other sums");
    let e = latest(&aws, &f.request("")).unwrap_err().to_string();
    assert!(e.contains("releases/1.2.3/SHA256SUMS holds sha256"), "{e}");
    assert!(aws.get(LATEST_KEY).is_none());
}

#[test]
fn latest_leaves_a_newer_pointer_alone_and_holds_its_flags_and_files_to_the_same_forms() {
    // Catches: the promotion of an older release moving the channel down, and a directory that
    // is not a release read as one.
    let f = Fixture::new("latest-newer");
    let aws = FakeAws::default();
    upload(&aws, &without_latest(&f)).expect("published to testing");
    aws.put(LATEST_KEY, br#"{"version": "1.9.0"}"#);
    let said = latest(&aws, &f.request("")).expect("left alone");
    assert!(
        said.last()
            .expect("a line")
            .contains("newer than 1.2.3: left as it is"),
        "{said:?}"
    );
    let mut r = f.request("");
    r.tag = "1.2.3";
    assert!(matches!(latest(&aws, &r), Err(Error::Usage(_))));
    std::fs::remove_file(f.release.join(SUMS)).expect("remove");
    assert!(latest(&aws, &f.request("")).is_err());
}

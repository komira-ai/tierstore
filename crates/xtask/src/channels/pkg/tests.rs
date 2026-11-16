use serde_json::{Value, json};

use super::*;
use crate::channels::apt::testing::{
    BUCKET, DENIED, FakeAws, NO_SUCH_KEY, PRECONDITION, etag, flag,
};
use crate::channels::upload::IMMUTABLE;
use crate::fetch::Sha256;
use crate::testdir::TestDir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const PKG: &str = "tierstore-1.2.3-macos-arm64.pkg";
const PKG_KEY: &str = "pkg/tierstore/1.2.3/tierstore-1.2.3-macos-arm64.pkg";
const BUNDLE_KEY: &str = "pkg/tierstore/1.2.3/tierstore-1.2.3-macos-arm64.pkg.sigstore.json";
const SUMS_KEY: &str = "pkg/tierstore/1.2.3/SHA256SUMS";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(beside.parent().expect("deps").join("xtask-pkg-unit"), name)
}

fn pkg_bytes() -> Vec<u8> {
    b"bytes of the signed pkg".to_vec()
}

fn bundle_bytes() -> Vec<u8> {
    b"bundle of the signed pkg".to_vec()
}

/// A release directory of version 1.2.3: another file, the installer and its bundle, and the
/// `SHA256SUMS` that lists the first two (the bundle is made after the sums).
struct Fixture {
    _scratch: TestDir,
    dir: PathBuf,
    work: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let scratch = scratch(name);
        let dir = scratch.join("release");
        std::fs::create_dir(&dir).expect("release");
        std::fs::write(dir.join(PKG), pkg_bytes()).expect("pkg");
        std::fs::write(dir.join(format!("{PKG}{SUFFIX}")), bundle_bytes()).expect("bundle");
        let lines = [
            sums::Line {
                sha256: Sha256::of(b"another file"),
                name: "tierstore-1.2.3-linux-x86_64.tar.gz".to_owned(),
            },
            sums::Line {
                sha256: Sha256::of(&pkg_bytes()),
                name: PKG.to_owned(),
            },
        ];
        std::fs::write(dir.join(SUMS), sums::render(&lines)).expect("sums");
        let work = scratch.join("work");
        Fixture {
            _scratch: scratch,
            dir,
            work,
        }
    }

    fn request<'a>(&'a self, distribution_id: &'a str) -> Request<'a> {
        Request {
            dir: &self.dir,
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

fn refusals<T: std::fmt::Debug>(result: Result<T, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(lines)) => lines,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn failed<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    match result {
        Err(Error::Failed(text)) => text,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn usage<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    match result {
        Err(Error::Usage(text)) => text,
        other => panic!("expected a usage error, got {other:?}"),
    }
}

/// The bucket as the upload leaves it for the fixture.
fn published(aws: &FakeAws) {
    aws.put(PKG_KEY, &pkg_bytes());
    aws.put(BUNDLE_KEY, &bundle_bytes());
    let lines = [
        sums::Line {
            sha256: Sha256::of(&bundle_bytes()),
            name: format!("{PKG}{SUFFIX}"),
        },
        sums::Line {
            sha256: Sha256::of(&pkg_bytes()),
            name: PKG.to_owned(),
        },
    ];
    aws.put(SUMS_KEY, sums::render(&lines).as_bytes());
}

fn pointer(version: &str) -> Vec<u8> {
    format!("{{\"version\": \"{version}\"}}").into_bytes()
}

#[test]
fn upload_puts_the_installer_its_bundle_and_their_sums_once_each_under_the_versions_directory() {
    // Asserts: the three keys, the bytes put at each (the sums list the two files, bundle
    // first), the write-once condition and the content type of each put, and no pointer.
    let f = Fixture::new("upload");
    let aws = FakeAws::new();
    let said = upload(&aws, &f.request("")).expect("upload");
    assert_eq!(
        said,
        [
            format!("{BUNDLE_KEY}: written"),
            format!("{PKG_KEY}: written"),
            format!("{SUMS_KEY}: written"),
        ]
    );
    assert_eq!(aws.keys_put(), [BUNDLE_KEY, PKG_KEY, SUMS_KEY]);
    assert_eq!(aws.get(PKG_KEY), Some(pkg_bytes()));
    assert_eq!(aws.get(BUNDLE_KEY), Some(bundle_bytes()));
    let sums_text = format!(
        "{}  {PKG}{SUFFIX}\n{}  {PKG}\n",
        Sha256::of(&bundle_bytes()),
        Sha256::of(&pkg_bytes())
    );
    assert_eq!(aws.get(SUMS_KEY), Some(sums_text.into_bytes()));
    let types: Vec<String> = aws
        .calls_of("put-object")
        .iter()
        .map(|c| flag(c, "--content-type").expect("a type"))
        .collect();
    assert_eq!(
        types,
        [
            "application/json",
            "application/octet-stream",
            "text/plain; charset=utf-8"
        ]
    );
    for call in aws.calls_of("put-object") {
        assert_eq!(flag(&call, "--if-none-match").as_deref(), Some("*"));
        assert_eq!(flag(&call, "--cache-control").as_deref(), Some(IMMUTABLE));
    }
    assert!(aws.get(STABLE_KEY).is_none());
}

#[test]
fn a_second_upload_of_the_same_bytes_passes_and_other_bytes_at_a_key_fail() {
    // Asserts: a re-run of the job is accepted ("already there"), and a file of a version is
    // never replaced: other bytes at the key are a failure naming the key and both digests.
    let f = Fixture::new("upload-twice");
    let aws = FakeAws::new();
    upload(&aws, &f.request("")).expect("first");
    let said = upload(&aws, &f.request("")).expect("second");
    assert_eq!(
        said[1],
        format!("{PKG_KEY}: already there with the same bytes")
    );
    aws.put(PKG_KEY, b"another installer");
    let text = failed(upload(&aws, &f.request("")));
    assert!(
        text.contains(&format!(
            "{PKG_KEY} exists with other bytes (sha256 {}",
            Sha256::of(b"another installer")
        )) && text.contains("a file of a version is never replaced"),
        "{text}"
    );
}

#[test]
fn an_upload_that_cannot_read_back_or_is_refused_by_aws_fails_naming_the_key() {
    // Asserts: a 412 whose read-back fails, and a refusal that is not a precondition, are
    // failures that name the key and AWS's words.
    let f = Fixture::new("upload-aws");
    let aws = FakeAws::new();
    aws.put(PKG_KEY, &pkg_bytes());
    aws.refuse_when("get-object", DENIED);
    let text = failed(upload(&aws, &f.request("")));
    assert_eq!(
        text,
        format!("{PKG_KEY} exists, and reading it back failed: {DENIED}")
    );
    let aws = FakeAws::new();
    aws.refuse_when("put-object", DENIED);
    assert_eq!(
        failed(upload(&aws, &f.request(""))),
        format!("put {BUNDLE_KEY}: {DENIED}")
    );
}

#[test]
fn files_not_held_to_the_release_are_refused_before_anything_is_put() {
    // Asserts: a missing installer or bundle, an installer the sums do not list, and an installer
    // whose digest is not the sums' each give one line naming the file, and AWS is never called.
    let f = Fixture::new("refusals");
    std::fs::remove_file(f.dir.join(PKG)).expect("remove");
    let aws = FakeAws::new();
    let lines = refusals(upload(&aws, &f.request("")));
    assert_eq!(
        lines,
        [format!("{} is not a file", f.dir.join(PKG).display())]
    );
    assert!(aws.calls.borrow().is_empty());

    let f = Fixture::new("refusals-bundle");
    std::fs::remove_file(f.dir.join(format!("{PKG}{SUFFIX}"))).expect("remove");
    let lines = refusals(upload(&aws, &f.request("")));
    assert_eq!(
        lines,
        [format!(
            "{} is not a file",
            f.dir.join(format!("{PKG}{SUFFIX}")).display()
        )]
    );

    let f = Fixture::new("refusals-unlisted");
    let other = sums::Line {
        sha256: Sha256::of(b"x"),
        name: "x".to_owned(),
    };
    std::fs::write(f.dir.join(SUMS), sums::render(&[other])).expect("sums");
    assert_eq!(
        refusals(upload(&aws, &f.request(""))),
        [format!("{SUMS} does not list {PKG}")]
    );

    let f = Fixture::new("refusals-digest");
    std::fs::write(f.dir.join(PKG), b"changed after the sums").expect("pkg");
    assert_eq!(
        refusals(upload(&aws, &f.request(""))),
        [format!(
            "{PKG} is {}, but {SUMS} lists {}",
            Sha256::of(b"changed after the sums"),
            Sha256::of(&pkg_bytes())
        )]
    );
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn flags_out_of_their_form_are_usage_errors() {
    // Asserts: a tag without the v is refused by upload and by promote before any file is read.
    let f = Fixture::new("usage");
    let aws = FakeAws::new();
    let mut r = f.request("");
    r.tag = "1.2.3";
    assert_eq!(
        usage(upload(&aws, &r)),
        "--tag \"1.2.3\" is not v<major>.<minor>.<patch>"
    );
    assert_eq!(
        usage(promote(&aws, &r)),
        "--tag \"1.2.3\" is not v<major>.<minor>.<patch>"
    );
    let mut r = f.request("");
    r.tag = "v99999999999999999999.0.0";
    assert_eq!(
        usage(promote(&aws, &r)),
        "--tag \"v99999999999999999999.0.0\" holds a number too large"
    );
}

#[test]
fn promote_reads_the_files_back_and_writes_the_pointer_with_the_digests_and_sizes() {
    // Asserts: the pointer's exact JSON (version, tag, commit, each file's sha256 and size), its
    // content type, cache header and write-once condition when there is none, the read-back of
    // each file and the sums, and the invalidation of the pointer's path.
    let f = Fixture::new("promote");
    let aws = FakeAws::new();
    published(&aws);
    let said = promote(&aws, &f.request("E123ABC")).expect("promote");
    assert_eq!(
        said,
        [
            format!("{BUNDLE_KEY}: sha256 {}", Sha256::of(&bundle_bytes())),
            format!("{PKG_KEY}: sha256 {}", Sha256::of(&pkg_bytes())),
            format!(
                "{SUMS_KEY}: sha256 {}",
                Sha256::of(&std::fs::read(f.work.join(SUMS)).expect("sums"))
            ),
            format!("{STABLE_KEY}: now 1.2.3"),
            format!("/{STABLE_KEY}: invalidated"),
        ]
    );
    let stored: Value =
        serde_json::from_slice(&aws.get(STABLE_KEY).expect("pointer")).expect("JSON");
    assert_eq!(
        stored,
        json!({
            "version": "1.2.3",
            "tag": "v1.2.3",
            "commit": COMMIT,
            "files": {
                PKG: {"sha256": Sha256::of(&pkg_bytes()).to_string(), "size": pkg_bytes().len()},
                format!("{PKG}{SUFFIX}"): {"sha256": Sha256::of(&bundle_bytes()).to_string(), "size": bundle_bytes().len()},
            },
        })
    );
    let put = &aws.calls_of("put-object")[0];
    assert_eq!(
        flag(put, "--content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(flag(put, "--cache-control").as_deref(), Some(POINTER_CACHE));
    assert_eq!(flag(put, "--if-none-match").as_deref(), Some("*"));
    let invalidation = &aws.calls_of("create-invalidation")[0];
    assert_eq!(
        flag(invalidation, "--distribution-id").as_deref(),
        Some("E123ABC")
    );
    assert_eq!(
        flag(invalidation, "--paths").as_deref(),
        Some("/pkg/tierstore/stable.json")
    );
}

#[test]
fn promote_replaces_an_older_pointer_with_if_match_and_leaves_a_newer_or_equal_one() {
    // Asserts: an older pointer is replaced conditional on its ETag; a newer one is left with
    // nothing written; an equal one is left and only invalidated (when a distribution is given).
    let f = Fixture::new("promote-order");
    let aws = FakeAws::new();
    published(&aws);
    aws.put(STABLE_KEY, &pointer("1.2.2"));
    promote(&aws, &f.request("")).expect("older");
    let put = &aws.calls_of("put-object")[0];
    assert_eq!(flag(put, "--if-match"), Some(etag(&pointer("1.2.2"))));
    assert!(aws.calls_of("create-invalidation").is_empty());

    let aws = FakeAws::new();
    published(&aws);
    aws.put(STABLE_KEY, &pointer("1.10.0"));
    let said = promote(&aws, &f.request("E1")).expect("newer");
    assert_eq!(
        said.last().map(String::as_str),
        Some("pkg/tierstore/stable.json: at 1.10.0, newer than 1.2.3: left as it is")
    );
    assert!(aws.calls_of("put-object").is_empty());

    let aws = FakeAws::new();
    published(&aws);
    aws.put(STABLE_KEY, &pointer("1.2.3"));
    let said = promote(&aws, &f.request("E1")).expect("equal");
    assert_eq!(
        said[said.len() - 2..],
        [
            "pkg/tierstore/stable.json: already at 1.2.3".to_owned(),
            "/pkg/tierstore/stable.json: invalidated".to_owned()
        ]
    );
    assert!(aws.calls_of("put-object").is_empty());
}

#[test]
fn promote_decides_again_after_a_lost_race_and_gives_up_after_its_attempts() {
    // Asserts: a pointer written by another release between the read and the put is read again
    // (the second put succeeds, conditional on the new ETag); one written under every attempt
    // fails naming the number of attempts.
    let f = Fixture::new("promote-race");
    let aws = FakeAws::new();
    published(&aws);
    aws.put(STABLE_KEY, &pointer("1.2.0"));
    aws.before_put_of(
        "stable.json",
        Box::new(|store| {
            store.insert(STABLE_KEY.to_owned(), pointer("1.2.1"));
        }),
    );
    let said = promote(&aws, &f.request("")).expect("second attempt");
    assert_eq!(
        said.last().map(String::as_str),
        Some("pkg/tierstore/stable.json: now 1.2.3")
    );
    assert_eq!(aws.calls_of("put-object").len(), 2);

    let aws = FakeAws::new();
    published(&aws);
    aws.put(STABLE_KEY, &pointer("1.2.0"));
    // Every put of the pointer loses its condition.
    aws.refuse_when("--key pkg/tierstore/stable.json --body", PRECONDITION);
    let text = failed(promote(&aws, &f.request("")));
    assert_eq!(
        text,
        "pkg/tierstore/stable.json was written by another release 3 times in a row; run the job again"
    );
}

#[test]
fn promote_refuses_a_bucket_that_does_not_hold_the_release_files() {
    // Asserts: a file or the sums missing from the bucket, or holding other bytes, is a failure
    // naming the key, and the pointer is never written.
    let f = Fixture::new("promote-bucket");
    let aws = FakeAws::new();
    let text = failed(promote(&aws, &f.request("")));
    assert!(
        text.starts_with(&format!("{BUNDLE_KEY} could not be read back")),
        "{text}"
    );
    published(&aws);
    aws.put(PKG_KEY, b"other bytes");
    let text = failed(promote(&aws, &f.request("")));
    assert_eq!(
        text,
        format!(
            "{PKG_KEY} holds sha256 {}, but this release's file is {}",
            Sha256::of(b"other bytes"),
            Sha256::of(&pkg_bytes())
        )
    );
    assert!(aws.calls_of("put-object").is_empty());
}

#[test]
fn promote_fails_on_an_unreadable_pointer_a_refused_put_or_a_refused_invalidation() {
    // Asserts: the words of each failure. A pointer that is not JSON, has no version, or whose
    // read printed no ETag; a read refused for another reason than a missing key; a put refused
    // for another reason than a lost condition; an invalidation AWS refuses. A missing key and a
    // denied read both mean there is no pointer yet.
    let f = Fixture::new("promote-failures");
    let aws = FakeAws::new();
    published(&aws);
    aws.put(STABLE_KEY, b"not json");
    let text = failed(promote(&aws, &f.request("")));
    assert!(
        text.starts_with(&format!("{STABLE_KEY} in the bucket is not JSON: ")),
        "{text}"
    );
    aws.put(STABLE_KEY, b"{\"version\": \"latest\"}");
    assert_eq!(
        failed(promote(&aws, &f.request(""))),
        format!("{STABLE_KEY} in the bucket has no <major>.<minor>.<patch> version")
    );
    *aws.stdout.borrow_mut() = Some("{}".to_owned());
    aws.put(STABLE_KEY, &pointer("1.2.0"));
    assert_eq!(
        failed(promote(&aws, &f.request(""))),
        format!("read {STABLE_KEY}: aws printed no ETag")
    );
    let aws = FakeAws::new();
    published(&aws);
    aws.refuse_when(
        "pkg/tierstore/stable.json",
        "An error occurred (InternalError): try again",
    );
    assert_eq!(
        failed(promote(&aws, &f.request(""))),
        format!("read {STABLE_KEY}: An error occurred (InternalError): try again")
    );
    for missing in [NO_SUCH_KEY, DENIED] {
        let aws = FakeAws::new();
        published(&aws);
        aws.refuse_when("stable.json", missing);
        // The pointer cannot be read: it is written (the put is refused too, so the test ends
        // at the put and shows the read was taken as "no pointer yet").
        let text = failed(promote(&aws, &f.request("")));
        assert!(text.starts_with(&format!("put {STABLE_KEY}: ")), "{text}");
    }
    let aws = FakeAws::new();
    published(&aws);
    aws.refuse_when(
        "create-invalidation",
        "An error occurred (AccessDenied) when calling CreateInvalidation",
    );
    assert_eq!(
        failed(promote(&aws, &f.request("E1"))),
        "invalidate /pkg/tierstore/stable.json: An error occurred (AccessDenied) when calling CreateInvalidation"
    );
}

#[test]
fn a_put_that_loses_for_another_reason_than_its_condition_is_not_retried() {
    // Asserts: only a precondition failure is decided again; any other refusal ends the verb.
    let f = Fixture::new("promote-refused-put");
    let aws = FakeAws::new();
    published(&aws);
    aws.refuse_when(
        "put-object",
        "An error occurred (SlowDown) when calling the PutObject operation",
    );
    let text = failed(promote(&aws, &f.request("")));
    assert!(text.starts_with(&format!("put {STABLE_KEY}: ")), "{text}");
    assert_eq!(aws.calls_of("put-object").len(), 1);
}

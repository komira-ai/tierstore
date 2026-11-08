use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::sync::mpsc;

use super::*;
use crate::channels::aws::Answer;
use crate::testdir::TestDir;

const ROLE: &str = "arn:aws:iam::123456789012:role/release";
const BEARER: &str = "runner-bearer-secret";
const JWT: &str = "eyJ.header.signature";
const STS: &str = r#"{"AccessKeyId":"AKIAFAKE","SecretAccessKey":"fake-secret","SessionToken":"fake-session","Expiration":"2026-11-08T12:00:00+00:00"}"#;

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside.parent().expect("deps").join("xtask-assume-unit"),
        name,
    )
}

/// A token endpoint on the loopback address that answers its one request with `status` and
/// `body`: the URL (with a query already, as the runner's is) and what it was asked.
fn token_endpoint(status: &'static str, body: String) -> (String, mpsc::Receiver<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!(
        "http://{}/token?api-version=2.0",
        listener.local_addr().expect("addr")
    );
    let (sent, asked) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("request");
            if line.trim_end().is_empty() {
                break;
            }
            lines.push(line.trim_end().to_owned());
        }
        // Nobody may be listening: a test that does not read what was asked drops the receiver.
        let _ = sent.send(lines);
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).expect("head");
        stream.write_all(body.as_bytes()).expect("body");
    });
    (url, asked)
}

/// A stand-in `aws` that answers every call with one answer and records the calls.
struct FakeAws {
    answer: Answer,
    calls: RefCell<Vec<Vec<String>>>,
}

impl FakeAws {
    fn says(ok: bool, stdout: &str, stderr: &str) -> FakeAws {
        FakeAws {
            answer: Answer {
                ok,
                stdout: stdout.to_owned(),
                stderr: stderr.to_owned(),
            },
            calls: RefCell::default(),
        }
    }
}

impl Aws for FakeAws {
    fn run(&self, args: &[String]) -> Result<Answer, Error> {
        self.calls.borrow_mut().push(args.to_vec());
        Ok(Answer {
            ok: self.answer.ok,
            stdout: self.answer.stdout.clone(),
            stderr: self.answer.stderr.clone(),
        })
    }
}

/// A log that records each write and whether the env file existed when it was made.
struct Log {
    env_file: PathBuf,
    writes: Vec<(String, bool)>,
}

impl std::io::Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writes.push((
            String::from_utf8_lossy(buf).into_owned(),
            self.env_file.exists(),
        ));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A log that cannot be written.
struct BrokenLog;

impl std::io::Write for BrokenLog {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("closed"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Run {
    /// Kept so the scratch directory outlives the run.
    _dir: TestDir,
    result: Result<Vec<String>, Error>,
    log: Log,
    env_file: PathBuf,
    aws: FakeAws,
}

fn run_with(
    name: &str,
    url: &str,
    https_only: bool,
    aws: FakeAws,
    change: impl FnOnce(&mut Request),
) -> Run {
    let dir = scratch(name);
    let env_file = dir.join("github-env");
    let oidc = || {
        Ok(Oidc {
            url: url.to_owned(),
            bearer: BEARER.to_owned(),
        })
    };
    let mut log = Log {
        env_file: env_file.clone(),
        writes: Vec::new(),
    };
    let mut request = Request {
        role_arn: ROLE,
        session_name: "tierstore-release-42",
        region: "us-east-1",
        env_file: &env_file,
        https_only,
    };
    change(&mut request);
    let result = assume(&aws, &mut log, &oidc, &request);
    Run {
        _dir: dir,
        result,
        log,
        env_file,
        aws,
    }
}

fn masked(run: &Run) -> Vec<&str> {
    run.log
        .writes
        .iter()
        .filter_map(|(w, _)| w.strip_prefix("::add-mask::"))
        .map(|m| m.trim_end())
        .collect()
}

#[test]
fn the_runner_token_is_exchanged_and_the_credentials_reach_the_env_file_exactly() {
    // Catches: credentials that are not written as the steps after read them, a token asked for
    // without the audience STS trusts or without the runner's bearer, and an exchange that does
    // not pass the role, session and token to STS.
    let (url, asked) = token_endpoint("200 OK", format!(r#"{{"count":1,"value":"{JWT}"}}"#));
    let run = run_with("success", &url, false, FakeAws::says(true, STS, ""), |_| {});
    assert_eq!(
        run.result,
        Ok(vec![
            "assumed the role; its credentials are masked and reach the steps after this one"
                .to_owned()
        ])
    );
    assert_eq!(
        std::fs::read_to_string(&run.env_file).expect("env file"),
        "AWS_ACCESS_KEY_ID=AKIAFAKE\nAWS_SECRET_ACCESS_KEY=fake-secret\nAWS_SESSION_TOKEN=fake-session\nAWS_REGION=us-east-1\nAWS_DEFAULT_REGION=us-east-1\n"
    );
    let request = asked.recv().expect("the token was asked for");
    assert_eq!(
        request[0],
        "GET /token?api-version=2.0&audience=sts.amazonaws.com HTTP/1.1"
    );
    assert!(
        request
            .iter()
            .any(|l| l.eq_ignore_ascii_case(&format!("authorization: Bearer {BEARER}"))),
        "{request:?}"
    );
    let calls = run.aws.calls.borrow();
    assert_eq!(
        *calls,
        [[
            "sts",
            "assume-role-with-web-identity",
            "--role-arn",
            ROLE,
            "--role-session-name",
            "tierstore-release-42",
            "--web-identity-token",
            JWT,
            "--duration-seconds",
            "3600",
            "--query",
            "Credentials",
            "--output",
            "json"
        ]
        .map(str::to_owned)]
    );
}

#[test]
fn every_secret_is_masked_before_it_is_used_and_none_is_printed() {
    // Catches: a credential that reaches the env file, the log or the returned lines before the
    // runner is told to hide it.
    let (url, _) = token_endpoint("200 OK", format!(r#"{{"value":"{JWT}"}}"#));
    let run = run_with("masks", &url, false, FakeAws::says(true, STS, ""), |_| {});
    let said = run.result.as_ref().expect("assumed");
    assert_eq!(
        masked(&run),
        [ROLE, BEARER, JWT, "AKIAFAKE", "fake-secret", "fake-session"]
    );
    // Each mask line is one write, and the env file did not exist at any of them.
    assert_eq!(run.log.writes.len(), 6, "{:?}", run.log.writes);
    assert!(
        run.log
            .writes
            .iter()
            .all(|(w, exists)| !exists && w.starts_with("::add-mask::") && w.ends_with('\n')),
        "{:?}",
        run.log.writes
    );
    for secret in ["AKIAFAKE", "fake-secret", "fake-session", JWT, BEARER] {
        assert!(said.iter().all(|l| !l.contains(secret)), "{said:?}");
    }
}

#[test]
fn a_failed_token_request_stops_before_aws_and_writes_nothing() {
    // Catches: an exchange attempted without a token, and a failure that quotes the bearer.
    let (url, _) = token_endpoint("403 Forbidden", "no".to_owned());
    let run = run_with("denied", &url, false, FakeAws::says(true, STS, ""), |_| {});
    let Err(Error::Failed(why)) = &run.result else {
        panic!("{:?}", run.result);
    };
    assert!(why.starts_with("the OIDC token request failed: "), "{why}");
    assert!(!why.contains(BEARER), "{why}");
    assert!(run.aws.calls.borrow().is_empty());
    assert!(!run.env_file.exists());
    assert_eq!(masked(&run), [ROLE, BEARER]);
}

#[test]
fn a_token_answer_without_a_value_is_refused() {
    // Catches: an empty or missing token passed on to STS.
    for (name, body) in [
        ("not-json", "<html>".to_owned()),
        ("no-value", r#"{"count":0}"#.to_owned()),
        ("empty-value", r#"{"value":""}"#.to_owned()),
        ("number-value", r#"{"value":7}"#.to_owned()),
    ] {
        let (url, _) = token_endpoint("200 OK", body);
        let run = run_with(name, &url, false, FakeAws::says(true, STS, ""), |_| {});
        let Err(Error::Failed(why)) = &run.result else {
            panic!("{name}: {:?}", run.result);
        };
        assert!(
            why.starts_with("the OIDC token request failed: the answer"),
            "{name}: {why}"
        );
        assert!(run.aws.calls.borrow().is_empty(), "{name}");
    }
}

#[test]
fn the_token_is_only_asked_for_over_https() {
    // Catches: a bearer sent in the clear to a plain http address.
    let run = run_with(
        "plain",
        "http://127.0.0.1:9/token",
        true,
        FakeAws::says(true, STS, ""),
        |_| {},
    );
    let Err(Error::Failed(why)) = &run.result else {
        panic!("{:?}", run.result);
    };
    assert!(why.starts_with("the OIDC token request failed: "), "{why}");
    assert!(run.aws.calls.borrow().is_empty());
}

#[test]
fn a_token_url_without_a_query_gets_one_for_the_audience() {
    // Catches: an audience appended with `&` to a url that has no `?`.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/token", listener.local_addr().expect("addr"));
    let (sent, asked) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut first = String::new();
        BufReader::new(stream).read_line(&mut first).expect("line");
        sent.send(first).expect("send");
    });
    let run = run_with(
        "no-query",
        &url,
        false,
        FakeAws::says(true, STS, ""),
        |_| {},
    );
    assert!(run.result.is_err());
    assert_eq!(
        asked.recv().expect("asked"),
        "GET /token?audience=sts.amazonaws.com HTTP/1.1\r\n"
    );
}

#[test]
fn a_refusal_by_sts_fails_with_its_words_and_writes_nothing() {
    // Catches: a role that was not assumed taken for one that was.
    let (url, _) = token_endpoint("200 OK", format!(r#"{{"value":"{JWT}"}}"#));
    let denied = "An error occurred (AccessDenied) when calling the AssumeRoleWithWebIdentity operation: Not authorized\n";
    let run = run_with("sts", &url, false, FakeAws::says(false, "", denied), |_| {});
    assert_eq!(
        run.result,
        Err(Error::Failed(
            "assuming the role failed: An error occurred (AccessDenied) when calling the AssumeRoleWithWebIdentity operation: Not authorized"
                .to_owned()
        ))
    );
    assert!(!run.env_file.exists());
    assert_eq!(masked(&run), [ROLE, BEARER, JWT]);
}

#[test]
fn credentials_that_are_not_all_there_are_refused_and_not_written() {
    // Catches: a half answer written as if it were credentials, and a value that would add a
    // line of its own to the env file.
    for (name, stdout) in [
        ("not-json", "not json".to_owned()),
        ("null", "null".to_owned()),
        (
            "no-key",
            r#"{"SecretAccessKey":"s","SessionToken":"t"}"#.to_owned(),
        ),
        (
            "no-secret",
            r#"{"AccessKeyId":"a","SessionToken":"t"}"#.to_owned(),
        ),
        (
            "no-session",
            r#"{"AccessKeyId":"a","SecretAccessKey":"s"}"#.to_owned(),
        ),
        (
            "empty",
            r#"{"AccessKeyId":"","SecretAccessKey":"s","SessionToken":"t"}"#.to_owned(),
        ),
        (
            "newline",
            r#"{"AccessKeyId":"a\nAWS_REGION=x","SecretAccessKey":"s","SessionToken":"t"}"#
                .to_owned(),
        ),
    ] {
        let (url, _) = token_endpoint("200 OK", format!(r#"{{"value":"{JWT}"}}"#));
        let run = run_with(name, &url, false, FakeAws::says(true, &stdout, ""), |_| {});
        assert_eq!(
            run.result,
            Err(Error::Failed(
                "the credentials answer of STS is unusable".to_owned()
            )),
            "{name}"
        );
        assert!(!run.env_file.exists(), "{name}");
        assert_eq!(masked(&run), [ROLE, BEARER, JWT], "{name}");
    }
}

#[test]
fn an_empty_role_and_values_that_are_not_plain_are_refused_before_anything_is_asked() {
    // Catches: an assume with no role, a session name AWS would refuse, a value that is more than
    // one line of the env file.
    let changes: [Refused; 7] = [
        ("empty-role", |r| r.role_arn = "", "--role-arn is empty"),
        (
            "role-newline",
            |r| r.role_arn = "arn\nx",
            "--role-arn is empty or has a control character",
        ),
        (
            "empty-region",
            |r| r.region = "",
            "--region is empty or has a control character",
        ),
        (
            "region-newline",
            |r| r.region = "us\r-east-1",
            "--region is empty or has a control character",
        ),
        (
            "empty-session",
            |r| r.session_name = "",
            "--session-name is not 2 to 64 letters, digits and `_+=,.@-`",
        ),
        (
            "session-space",
            |r| r.session_name = "a b",
            "--session-name is not 2 to 64 letters, digits and `_+=,.@-`",
        ),
        (
            "session-long",
            |r| r.session_name = &LONG,
            "--session-name is not 2 to 64 letters, digits and `_+=,.@-`",
        ),
    ];
    for (name, change, why) in changes {
        let run = run_with(
            name,
            "http://127.0.0.1:9/",
            false,
            FakeAws::says(true, STS, ""),
            change,
        );
        assert_eq!(run.result, Err(Error::Usage(why.to_owned())), "{name}");
        assert!(run.log.writes.is_empty(), "{name}");
        assert!(run.aws.calls.borrow().is_empty(), "{name}");
    }
}

/// `(name, the change that puts a value out of its form, what the usage error says)`.
type Refused = (&'static str, fn(&mut Request), &'static str);

static LONG: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| "a".repeat(65));

#[test]
fn a_session_name_of_the_limits_is_taken() {
    // Catches: a boundary off by one in either direction.
    assert!(session_ok("ab"));
    assert!(session_ok(&"a".repeat(64)));
    assert!(session_ok("A_+=,.@-9"));
    assert!(!session_ok("a"));
    assert!(!session_ok(&"a".repeat(65)));
    assert!(!session_ok("a/b"));
}

#[test]
fn a_missing_oidc_offer_fails_before_anything_is_masked() {
    // Catches: a job without `id-token: write` going on with an empty token.
    let dir = scratch("no-offer");
    let env_file = dir.join("github-env");
    let mut log = Log {
        env_file: env_file.clone(),
        writes: Vec::new(),
    };
    let aws = FakeAws::says(true, STS, "");
    let offer = || oidc_from_env_with(|_| None);
    let result = assume(
        &aws,
        &mut log,
        &offer,
        &Request {
            role_arn: ROLE,
            session_name: "tierstore-release-42",
            region: "us-east-1",
            env_file: &env_file,
            https_only: true,
        },
    );
    assert_eq!(
        result,
        Err(Error::Failed(
            "the runner offers no OIDC token (the job needs `permissions: id-token: write`)"
                .to_owned()
        ))
    );
    assert!(log.writes.is_empty());
}

#[test]
fn the_offer_is_the_url_and_the_bearer_the_platform_sets() {
    // Catches: the two names swapped, and one missing taken for the pair.
    let only = |set: &'static [(&'static str, &'static str)]| {
        oidc_from_env_with(move |name| {
            set.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_owned())
        })
    };
    let both = only(&[
        ("ACTIONS_ID_TOKEN_REQUEST_URL", "https://u"),
        ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "b"),
    ])
    .expect("both");
    assert_eq!(
        (both.url.as_str(), both.bearer.as_str()),
        ("https://u", "b")
    );
    assert!(only(&[("ACTIONS_ID_TOKEN_REQUEST_URL", "https://u")]).is_err());
    assert!(only(&[("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "b")]).is_err());
}

#[test]
fn a_platform_value_is_read_by_name_and_empty_is_unset() {
    // Catches: the real lookup returning an empty value or reading the wrong variable. PATH is
    // set in every process that runs this test.
    assert!(platform_value("PATH").is_some_and(|p| !p.is_empty()));
    assert_eq!(platform_value("XTASK_ASSUME_TEST_NAME_NOBODY_SETS"), None);
}

#[test]
fn a_log_that_cannot_be_written_fails_before_the_exchange() {
    // Catches: a mask that never reached the runner, with the exchange going on regardless.
    let dir = scratch("broken-log");
    let env_file = dir.join("github-env");
    let aws = FakeAws::says(true, STS, "");
    let offer = || {
        Ok(Oidc {
            url: "http://127.0.0.1:9/".to_owned(),
            bearer: BEARER.to_owned(),
        })
    };
    let result = assume(
        &aws,
        &mut BrokenLog,
        &offer,
        &Request {
            role_arn: ROLE,
            session_name: "tierstore-release-42",
            region: "us-east-1",
            env_file: &env_file,
            https_only: false,
        },
    );
    let Err(Error::Spawn { what, why }) = result else {
        panic!("{result:?}");
    };
    assert_eq!((what.as_str(), why.as_str()), ("write the log", "closed"));
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn an_env_file_that_cannot_be_appended_to_fails_after_the_masks() {
    // Catches: credentials lost without a failure, leaving the next step to run with none.
    let (url, _) = token_endpoint("200 OK", format!(r#"{{"value":"{JWT}"}}"#));
    let dir = scratch("env-is-dir");
    // Leaked so that it outlives the request the change is made to: a test process is short.
    let missing: &'static std::path::Path =
        Box::leak(dir.join("no-such-dir").join("github-env").into_boxed_path());
    let run = run_with("env-fail", &url, false, FakeAws::says(true, STS, ""), |r| {
        r.env_file = missing;
    });
    assert!(
        matches!(run.result, Err(Error::Spawn { .. })),
        "{:?}",
        run.result
    );
    assert_eq!(masked(&run).len(), 6);
}

#[test]
fn an_answer_cut_short_is_a_failed_token_request() {
    // Catches: half a token taken for the token when the connection drops mid-answer.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/token", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut line = String::new();
        while reader.read_line(&mut line).expect("request") > 2 {
            line.clear();
        }
        let cut = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"value\":";
        stream.write_all(cut).expect("cut answer");
    });
    let run = run_with("cut", &url, false, FakeAws::says(true, STS, ""), |_| {});
    let Err(Error::Failed(why)) = &run.result else {
        panic!("{:?}", run.result);
    };
    assert!(why.starts_with("the OIDC token request failed: "), "{why}");
    assert!(run.aws.calls.borrow().is_empty());
}

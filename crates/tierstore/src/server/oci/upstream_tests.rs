//! The client of the upstream registry, against a registry in the process (see `fake_upstream`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures::stream::StreamExt as _;
use http_body_util::Empty;
use hyper::header::HOST;
use hyper::{Request, Uri};
use tierstore_testkit::dir::TestDir;

use super::{
    Challenge, Reply, Upstream, UpstreamError, UpstreamSettings, challenge, exchange,
    may_carry_secret, params, port_of, settle,
};
use crate::Error;
use crate::server::oci::acceptor;
use crate::server::oci::fake_upstream::{Ending, Fake, Handler, Registry, Seen, serve};
use crate::server::oci::test_pki::{ca, ip, leaf};

fn settings(url: &str) -> UpstreamSettings {
    UpstreamSettings {
        url: url.to_owned(),
        login: None,
        ca_pem: None,
        timeout: Duration::from_secs(30),
        max_redirects: 4,
        recheck_timeout: Duration::from_secs(5),
    }
}

fn logged_in(url: &str) -> UpstreamSettings {
    UpstreamSettings {
        login: Some(("alice".to_owned(), "s3cret".to_owned())),
        ..settings(url)
    }
}

fn client(settings: UpstreamSettings) -> Upstream {
    Upstream::new(settings).expect("a client")
}

fn basic_of(user: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
}

fn failure(result: Result<Reply, UpstreamError>) -> UpstreamError {
    match result {
        Err(failure) => failure,
        Ok(_) => panic!("the request was granted"),
    }
}

async fn text(reply: Reply) -> String {
    let body = reply.bytes(1 << 20).await.expect("a body");
    String::from_utf8(body.to_vec()).expect("text")
}

fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let own = |text: &str| text.to_owned();
    pairs.iter().map(|(a, b)| (own(a), own(b))).collect()
}

/// Asserts the parameters of a challenge are read: a quoted value may hold commas (a scope does),
/// a value may be unquoted, keys are lower case, an unterminated quote runs to the end, and text
/// with no `=` holds none.
#[test]
fn challenge_parameters_are_read_with_quoted_commas_and_unquoted_values() {
    let quoted = r#"realm="https://auth.example/token",service="registry.example",scope="repository:a/b:pull,push""#;
    let expected = owned(&[
        ("realm", "https://auth.example/token"),
        ("service", "registry.example"),
        ("scope", "repository:a/b:pull,push"),
    ]);
    assert_eq!(params(quoted), expected);
    let plain = owned(&[("realm", "abc"), ("service", "def")]);
    assert_eq!(params("Realm=abc, Service = def"), plain);
    assert_eq!(
        params(r#"realm="unterminated"#),
        owned(&[("realm", "unterminated")])
    );
    assert_eq!(params("no equals here"), owned(&[]));
    assert_eq!(params(""), owned(&[]));
}

/// Asserts the scheme of a challenge is told in any case, with or without parameters, and that
/// any other scheme is `Other`.
#[test]
fn a_challenge_is_basic_bearer_or_other() {
    assert_eq!(challenge(r#"Basic realm="x""#), Challenge::Basic);
    assert_eq!(challenge("Basic"), Challenge::Basic);
    let bearer = Challenge::Bearer(owned(&[("realm", "r")]));
    assert_eq!(challenge(r#"bearer realm="r""#), bearer);
    assert_eq!(challenge("  Bearer"), Challenge::Bearer(owned(&[])));
    assert_eq!(challenge("Negotiate abc"), Challenge::Other);
    assert_eq!(challenge(""), Challenge::Other);
}

/// Asserts a secret may go to an https address and to a loopback address, in IPv4 and IPv6, and
/// to no other: not plain http to a host name or another address.
#[test]
fn a_secret_goes_only_over_https_or_to_loopback() {
    let allowed = [
        "https://registry.example",
        "https://192.0.2.7:5000/token",
        "http://127.0.0.1:5000",
        "http://127.8.8.8/token",
        "http://[::1]:5000",
    ];
    for text in allowed {
        let uri: Uri = text.parse().expect("an address");
        assert!(may_carry_secret(&uri), "{text}");
    }
    let refused = [
        "http://registry.example",
        "http://localhost:5000",
        "http://192.0.2.7:5000",
        "http://[2001:db8::1]:5000",
    ];
    for text in refused {
        let uri: Uri = text.parse().expect("an address");
        assert!(!may_carry_secret(&uri), "{text}");
    }
}

/// Asserts the port of a request is the one the address names, else the default of its scheme.
#[test]
fn the_port_is_the_addresss_or_the_schemes_default() {
    let port = |text: &str| port_of(&text.parse::<Uri>().expect("an address"));
    assert_eq!(port("http://registry.example"), 80);
    assert_eq!(port("https://registry.example"), 443);
    assert_eq!(port("https://registry.example:8443"), 8443);
    assert_eq!(port("http://127.0.0.1:5000/path"), 5000);
}

/// Asserts the addresses that cannot work are refused with the whole message, and that the text
/// of an address that holds a password is not repeated.
#[test]
fn an_address_that_cannot_work_is_refused() {
    let refuse = |settings: UpstreamSettings| {
        let refused = Upstream::new(settings).err().expect("refused");
        refused.to_string()
    };
    assert_eq!(
        refuse(settings("not an address ")),
        "params.upstream: url: it is not an address"
    );
    assert_eq!(
        refuse(settings("ftp://registry.example")),
        "params.upstream: url: it must start with https:// or http://"
    );
    let with_secret = refuse(settings("https://alice:hunter2@registry.example"));
    assert_eq!(
        with_secret,
        "params.upstream: url: it must not hold a user name or password: use username and password_file"
    );
    let path = "params.upstream: url: it must be a host and a port, with no path or query";
    assert_eq!(refuse(settings("https://registry.example/v2/")), path);
    assert_eq!(refuse(settings("https://registry.example?x=1")), path);
    assert_eq!(
        refuse(logged_in("http://registry.example")),
        "params.upstream: url: a password is sent only over https, or to a loopback address"
    );
}

/// Asserts the addresses that can work are taken: a host with or without a port or a trailing
/// slash, and a login to https or to loopback.
#[test]
fn an_address_that_can_work_is_taken() {
    for text in ["https://registry.example", "https://registry.example:5000/"] {
        Upstream::new(settings(text)).expect(text);
    }
    for text in [
        "https://registry.example",
        "http://127.0.0.1:5000",
        "http://[::1]:5000",
    ] {
        Upstream::new(logged_in(text)).expect(text);
    }
}

/// Asserts a `ca_file` that is no certificate is refused: text with none in it, a certificate
/// whose base64 is wrong, and one whose bytes are no certificate.
#[test]
fn a_ca_file_that_holds_no_certificate_is_refused() {
    let refuse = |pem: &str| {
        let mut with = settings("https://registry.example");
        with.ca_pem = Some(pem.as_bytes().to_vec());
        Upstream::new(with).err().expect("refused").to_string()
    };
    assert_eq!(
        refuse("nothing here"),
        "params.upstream: ca_file: it holds no certificate"
    );
    let bad = "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
    assert!(refuse(bad).starts_with("params.upstream: ca_file: not PEM certificates: "));
    let junk = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    assert!(refuse(junk).starts_with("params.upstream: ca_file: "));
}

/// Asserts a 200 is a reply with the length it announced and its body, that the request had the
/// path and the `Accept` asked for, the registry's own `Host`, and no login.
#[tokio::test]
async fn a_200_is_a_reply_with_its_length_and_body() {
    let registry = Registry::start(|_| Fake::ok("hello")).await;
    let upstream = client(settings(&registry.url()));
    let reply = upstream.get("/v2/app/blobs/sha256:abc", "application/octet-stream");
    let reply = reply.await.expect("granted");
    assert_eq!(reply.length(), Some(5));
    assert_eq!(text(reply).await, "hello");
    let seen = registry.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v2/app/blobs/sha256:abc");
    assert_eq!(seen[0].accept.as_deref(), Some("application/octet-stream"));
    assert_eq!(seen[0].authorization, None);
    assert_eq!(seen[0].host, registry.authority());
}

/// Asserts a body whose length is not announced has no length, and is read all the same.
#[tokio::test]
async fn a_body_with_no_announced_length_is_read_to_its_end() {
    let registry = Registry::start(|_| Fake::chunked("hi")).await;
    let upstream = client(settings(&registry.url()));
    let reply = upstream.get("/x", "*/*").await.expect("granted");
    assert_eq!(reply.length(), None);
    assert_eq!(text(reply).await, "hi");
}

/// Asserts a body longer than the limit is refused, with the limit in the message.
#[tokio::test]
async fn a_body_over_the_limit_is_refused() {
    let registry = Registry::start(|_| Fake::ok("hello")).await;
    let upstream = client(settings(&registry.url()));
    let reply = upstream.get("/x", "*/*").await.expect("granted");
    let refused = reply.bytes(3).await.expect_err("refused");
    assert_eq!(refused.to_string(), "the body is over the limit of 3 bytes");
}

/// Asserts a 404 is `NotFound` and any other status is that status.
#[tokio::test]
async fn a_404_is_not_found_and_another_status_is_itself() {
    let registry = Registry::start(|seen| match seen.path.as_str() {
        "/missing" => Fake::status(404),
        _ => Fake::status(503),
    })
    .await;
    let upstream = client(settings(&registry.url()));
    let missing = failure(upstream.get("/missing", "*/*").await);
    assert!(matches!(missing, UpstreamError::NotFound), "{missing}");
    let other = failure(upstream.get("/other", "*/*").await);
    assert!(matches!(other, UpstreamError::Status(503)), "{other}");
    assert_eq!(other.to_string(), "the upstream answered 503");
}

/// Asserts a path that is no address is refused before any request is made.
#[tokio::test]
async fn a_path_that_is_not_valid_is_refused() {
    let registry = Registry::start(|_| Fake::ok("")).await;
    let upstream = client(settings(&registry.url()));
    let refused = failure(upstream.get("/a b", "*/*").await);
    assert_eq!(refused.to_string(), "the path is not valid");
    assert!(registry.seen().is_empty());
}

/// Asserts a `Basic` challenge is answered with the login, in a second request: the first goes
/// out with no login.
#[tokio::test]
async fn a_basic_challenge_is_answered_with_the_login() {
    let want = basic_of("alice", "s3cret");
    let accepted = want.clone();
    let registry = Registry::start(move |seen| match &seen.authorization {
        Some(given) if *given == accepted => Fake::ok("in"),
        _ => Fake::challenge(r#"Basic realm="fake""#),
    })
    .await;
    let upstream = client(logged_in(&registry.url()));
    let reply = upstream.get("/v2/x", "*/*").await.expect("granted");
    assert_eq!(text(reply).await, "in");
    let given: Vec<Option<String>> = registry
        .seen()
        .into_iter()
        .map(|seen| seen.authorization)
        .collect();
    assert_eq!(given, vec![None, Some(want)]);
}

/// Asserts a challenge the client cannot answer is a 401: `Basic` with no login, a scheme it does
/// not know, and a 401 with no challenge; and that no second request is made.
#[tokio::test]
async fn a_challenge_that_cannot_be_answered_is_a_401() {
    let registry = Registry::start(|seen| match seen.path.as_str() {
        "/basic" => Fake::challenge(r#"Basic realm="fake""#),
        "/negotiate" => Fake::challenge("Negotiate"),
        _ => Fake::status(401),
    })
    .await;
    let anonymous = client(settings(&registry.url()));
    let with_login = client(logged_in(&registry.url()));
    let basic = failure(anonymous.get("/basic", "*/*").await);
    assert!(matches!(basic, UpstreamError::Status(401)), "{basic}");
    let negotiate = failure(with_login.get("/negotiate", "*/*").await);
    assert!(
        matches!(negotiate, UpstreamError::Status(401)),
        "{negotiate}"
    );
    let silent = failure(with_login.get("/silent", "*/*").await);
    assert!(matches!(silent, UpstreamError::Status(401)), "{silent}");
    assert_eq!(registry.seen().len(), 3);
}

fn token_registry(realm_params: &'static str, token: &'static str) -> impl Fn(&Seen) -> Fake {
    move |seen| {
        if seen.path.starts_with("/token") {
            return Fake::ok(token);
        }
        match seen.authorization.as_deref() {
            Some("Bearer tok-1") => Fake::ok("in"),
            _ => {
                let host = &seen.host;
                Fake::challenge(&format!(
                    r#"Bearer realm="http://{host}/token{realm_params}"#
                ))
            }
        }
    }
}

/// Asserts a `Bearer` challenge is answered with a token fetched from its realm, with the
/// service and the scope in the query (encoded), asked with no login when there is none, and
/// that the first request goes out with no token.
#[tokio::test]
async fn a_bearer_challenge_is_answered_with_a_token_from_the_realm() {
    let registry = Registry::start(token_registry(
        r#"",service="fake registry",scope="repository:a/b:pull""#,
        r#"{"token":"tok-1"}"#,
    ))
    .await;
    let upstream = client(settings(&registry.url()));
    let reply = upstream
        .get("/v2/a/b/blobs/x", "*/*")
        .await
        .expect("granted");
    assert_eq!(text(reply).await, "in");
    let seen = registry.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].authorization, None);
    assert_eq!(
        seen[1].path,
        "/token?service=fake%20registry&scope=repository%3Aa%2Fb%3Apull"
    );
    assert_eq!(seen[1].authorization, None);
    assert_eq!(seen[2].authorization.as_deref(), Some("Bearer tok-1"));
}

/// Asserts the token request carries the login when there is one, that a token in `access_token`
/// is taken as well as one in `token`, and that a realm that has a query gets the rest after `&`.
#[tokio::test]
async fn the_token_request_carries_the_login_and_takes_either_key() {
    let registry = Registry::start(move |seen| {
        if seen.path.starts_with("/token") {
            return Fake::ok(r#"{"access_token":"tok-1"}"#);
        }
        match seen.authorization.as_deref() {
            Some("Bearer tok-1") => Fake::ok("in"),
            _ => {
                let host = &seen.host;
                let realm = if seen.path == "/two" {
                    "token?x=1"
                } else {
                    "token"
                };
                let more = if seen.path == "/two" {
                    r#",service="s""#
                } else {
                    r#",service="s0""#
                };
                Fake::challenge(&format!(r#"Bearer realm="http://{host}/{realm}"{more}"#))
            }
        }
    })
    .await;
    let upstream = client(logged_in(&registry.url()));
    let reply = upstream.get("/one", "*/*").await.expect("granted");
    assert_eq!(text(reply).await, "in");
    let reply = upstream.get("/two", "*/*").await.expect("granted");
    assert_eq!(text(reply).await, "in");
    let seen = registry.seen();
    let basic = Some(basic_of("alice", "s3cret"));
    assert_eq!(seen[1].path, "/token?service=s0");
    assert_eq!(seen[1].authorization, basic);
    assert_eq!(seen[4].path, "/token?x=1&service=s");
    assert_eq!(seen[4].authorization, basic);
}

/// Asserts each way a token cannot be had is a failure that says which: a challenge with no
/// realm, a realm that is not an address or not http(s), a token request refused, an answer that
/// is not JSON or has no token, and a token that cannot be a header.
#[tokio::test]
async fn a_token_that_cannot_be_had_is_a_failure_that_says_why() {
    type Case = (&'static str, Fake, &'static str);
    let cases: Vec<Case> = vec![
        (
            r#"Bearer service="x""#,
            Fake::ok(""),
            "the challenge names no realm",
        ),
        (
            r#"Bearer realm="not an address""#,
            Fake::ok(""),
            "the realm is not an address",
        ),
        (
            r#"Bearer realm="ftp://registry.example/token""#,
            Fake::ok(""),
            "the realm is not an http or https address",
        ),
        (
            r#"Bearer realm="HOST""#,
            Fake::ok("not json"),
            "the token is not JSON",
        ),
        (
            r#"Bearer realm="HOST""#,
            Fake::ok(r#"{"expires_in":5}"#),
            "the answer of the realm holds no token",
        ),
        (
            r#"Bearer realm="HOST""#,
            Fake::ok(r#"{"token":"a\nb"}"#),
            "the token is not a header value",
        ),
        (
            r#"Bearer realm="HOST""#,
            Fake::status(403),
            "the upstream answered 403",
        ),
    ];
    for (asked, token, why) in cases {
        let registry = Registry::start(move |seen| {
            if seen.path.starts_with("/token") {
                return token.clone();
            }
            let realm = asked.replace("HOST", &format!("http://{}/token", seen.host));
            Fake::challenge(&realm)
        })
        .await;
        let upstream = client(settings(&registry.url()));
        let refused = failure(upstream.get("/v2/x", "*/*").await);
        assert_eq!(refused.to_string(), why, "{asked}");
    }
}

/// Asserts redirects are followed, a relative one and an absolute one, and carry no login: the
/// request that is redirected was answered after its login, and the hops that follow have none.
#[tokio::test]
async fn redirects_are_followed_and_carry_no_login() {
    let want = basic_of("alice", "s3cret");
    let registry = Registry::start(move |seen| match seen.path.as_str() {
        "/a" if seen.authorization.as_deref() == Some(want.as_str()) => {
            Fake::redirect(302, "/b?sig=1")
        }
        "/a" => Fake::challenge(r#"Basic realm="fake""#),
        "/b?sig=1" => Fake::redirect(301, &format!("http://{}/c", seen.host)),
        _ => Fake::ok("end"),
    })
    .await;
    let upstream = client(logged_in(&registry.url()));
    let reply = upstream.get("/a", "*/*").await.expect("granted");
    assert_eq!(text(reply).await, "end");
    let seen: Vec<(String, bool)> = registry
        .seen()
        .into_iter()
        .map(|seen| (seen.path, seen.authorization.is_some()))
        .collect();
    let expected = [
        ("/a", false),
        ("/a", true),
        ("/b?sig=1", false),
        ("/c", false),
    ];
    let expected: Vec<(String, bool)> = expected
        .into_iter()
        .map(|(path, login)| (path.to_owned(), login))
        .collect();
    assert_eq!(seen, expected);
}

/// Asserts a challenge that comes after a redirect to another origin is not answered: the login
/// is sent to no one but the configured address and the realms it names, so the other registry,
/// which asks for a `Bearer` token, sees one request with no login and no token request is made.
#[tokio::test]
async fn a_challenge_after_a_redirect_to_another_origin_gets_no_login() {
    let elsewhere = Registry::start(|seen| {
        let host = &seen.host;
        Fake::challenge(&format!(r#"Bearer realm="http://{host}/token""#))
    })
    .await;
    let target = format!("{}/stolen", elsewhere.url());
    let first = Registry::start(move |_| Fake::redirect(302, &target)).await;
    let upstream = client(logged_in(&first.url()));
    let refused = failure(upstream.get("/v2/x", "*/*").await);
    assert!(matches!(refused, UpstreamError::Status(401)), "{refused}");
    let seen = elsewhere.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/stolen");
    assert_eq!(seen[0].authorization, None);
    assert_eq!(first.seen().len(), 1);
}

/// Asserts a request follows at most the redirects it is allowed: with two, three requests are
/// made and the failure says so; with none, one.
#[tokio::test]
async fn too_many_redirects_is_a_failure() {
    let registry = Registry::start(|_| Fake::redirect(302, "/loop")).await;
    for (allowed, made) in [(2_u32, 3_usize), (0, 1)] {
        let mut with = settings(&registry.url());
        with.max_redirects = allowed;
        let before = registry.seen().len();
        let refused = failure(client(with).get("/loop", "*/*").await);
        assert_eq!(refused.to_string(), "too many redirects");
        assert_eq!(registry.seen().len() - before, made);
    }
}

/// Asserts a redirect that leads nowhere is a failure: it has no `Location`, or one that is not
/// an http(s) address or a path.
#[tokio::test]
async fn a_redirect_that_leads_nowhere_is_a_failure() {
    let registry = Registry::start(|seen| match seen.path.as_str() {
        "/none" => Fake::status(302),
        "/ftp" => Fake::redirect(302, "ftp://registry.example/x"),
        _ => Fake::redirect(302, "no-slash"),
    })
    .await;
    let upstream = client(settings(&registry.url()));
    for path in ["/none", "/ftp", "/relative"] {
        let refused = failure(upstream.get(path, "*/*").await);
        assert_eq!(refused.to_string(), "a redirect leads nowhere", "{path}");
    }
}

/// Asserts an upstream that never answers is a failure after the timeout, and that the failure
/// names the timeout. The registry holds the request and never answers, so the outcome does not
/// depend on how fast the machine is.
#[tokio::test]
async fn an_upstream_that_never_answers_is_a_failure_after_the_timeout() {
    let registry = Registry::start(|_| Fake::ending("", Ending::Hang)).await;
    let mut with = settings(&registry.url());
    with.timeout = Duration::from_millis(50);
    let refused = failure(client(with).get("/x", "*/*").await);
    assert_eq!(refused.to_string(), "no answer within 50ms");
}

/// Asserts an upstream that closes the connection without an answer is a failure. The listener
/// stays bound for the whole test and closes each connection it accepts, so no port is freed for
/// another test or process to take.
#[tokio::test]
async fn an_upstream_that_closes_the_connection_is_a_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    let closing = tokio::spawn(async move {
        loop {
            let accepted = listener.accept().await;
            drop(accepted);
        }
    });
    let refused = failure(client(settings(&url)).get("/x", "*/*").await);
    closing.abort();
    assert!(matches!(refused, UpstreamError::Failed(_)), "{refused}");
}

/// Asserts an https upstream is reached when its CA is the `ca_file`, and is refused, with no
/// request seen, when the client does not trust the CA.
#[tokio::test]
async fn an_https_upstream_is_trusted_through_the_ca_file_and_not_without_it() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-upstream-tls");
    let ca = ca();
    let files = leaf(dir.path(), "registry", &ca, vec![ip("127.0.0.1")]);
    let listen = "127.0.0.1:443".parse().expect("an address");
    let tls = acceptor(&files.cert, &files.key, listen).expect("an acceptor");
    let registry = Registry::start_tls(tls, |_| Fake::ok("secure")).await;
    let url = format!("https://{}", registry.authority());
    let mut trusting = settings(&url);
    trusting.ca_pem = Some(ca.pem().into_bytes());
    let reply = client(trusting).get("/v2/", "*/*").await.expect("granted");
    assert_eq!(text(reply).await, "secure");
    let refused = failure(client(settings(&url)).get("/v2/", "*/*").await);
    assert!(matches!(refused, UpstreamError::Failed(_)), "{refused}");
    assert_eq!(registry.seen().len(), 1);
}

/// Asserts that a client that speaks no TLS to an https registry has its connection closed after
/// the failed handshake, and that no request is seen.
#[tokio::test]
async fn a_client_that_speaks_no_tls_to_an_https_registry_is_disconnected() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let dir = TestDir::under(std::env::temp_dir(), "oci-upstream-plain");
    let files = leaf(dir.path(), "registry", &ca(), vec![ip("127.0.0.1")]);
    let listen = "127.0.0.1:443".parse().expect("an address");
    let tls = acceptor(&files.cert, &files.key, listen).expect("an acceptor");
    let registry = Registry::start_tls(tls, |_| Fake::ok("secure")).await;
    let mut raw = tokio::net::TcpStream::connect(registry.authority())
        .await
        .expect("connected");
    raw.write_all(b"GET /v2/ HTTP/1.1\r\n\r\n")
        .await
        .expect("written");
    // The server closes the connection once the handshake has failed, so this ends.
    let mut rest = Vec::new();
    let _ = raw.read_to_end(&mut rest).await;
    assert!(registry.seen().is_empty());
}

/// Serves one request on one end of a duplex and gives the reply to it from the other, with
/// `idle` as the wait for each piece of the body. No socket and no clock but the paused one.
async fn over_a_duplex(fake: Fake, idle: Duration) -> Reply {
    let (near, far) = tokio::io::duplex(1 << 16);
    let handler: Arc<Handler> = Arc::new(move |_: &Seen| fake.clone());
    tokio::spawn(serve(far, handler, Arc::new(Mutex::new(Vec::new()))));
    let request = Request::builder()
        .uri("/x")
        .header(HOST, "fake")
        .body(Empty::<Bytes>::new())
        .expect("a request");
    let response = exchange(near, request).await.expect("headers");
    settle(response, idle).expect("granted")
}

/// Asserts a body that stops sending ends its stream with an error after the pieces that came,
/// once the wait for a piece is over (on a clock that is paused and moves when all is idle), and
/// that nothing follows the error.
#[tokio::test(start_paused = true)]
async fn a_body_that_stalls_ends_its_stream_with_an_error() {
    let reply = over_a_duplex(Fake::ending("abc", Ending::Stall), Duration::from_secs(30)).await;
    let mut pieces = reply.pieces();
    let first = pieces.next().await.expect("a piece").expect("data");
    assert_eq!(first, Bytes::from("abc"));
    let stalled = pieces.next().await.expect("an error").expect_err("failed");
    assert_eq!(
        stalled,
        Error::Other("the upstream sent no byte for 30s".to_owned())
    );
    assert!(pieces.next().await.is_none());
}

/// Asserts a body that is cut ends its stream with an error after the pieces that came, and that
/// reading it whole is a failure.
#[tokio::test]
async fn a_body_that_is_cut_ends_its_stream_with_an_error() {
    let cut = || Fake::ending("abc", Ending::Cut(10));
    let reply = over_a_duplex(cut(), Duration::from_secs(30)).await;
    let mut pieces = reply.pieces();
    assert_eq!(pieces.next().await.expect("a piece").expect("data"), "abc");
    let broken = pieces.next().await.expect("an error").expect_err("failed");
    assert!(matches!(broken, Error::Other(_)), "{broken}");
    assert!(pieces.next().await.is_none());
    let whole = over_a_duplex(cut(), Duration::from_secs(30)).await;
    let refused = whole.bytes(100).await.expect_err("failed");
    assert!(matches!(refused, UpstreamError::Failed(_)), "{refused}");
}

/// Asserts `head` sends a `HEAD` (never a `GET`), answers with the headers of the 200 (the
/// `Docker-Content-Digest` read by name, none for a header the answer lacks), follows a redirect
/// with a `HEAD`, answers a Basic challenge with the login, and tells a 404 and a 5xx as `get`
/// does.
#[tokio::test]
async fn a_head_is_sent_as_a_head_with_the_login_and_redirects_of_a_get() {
    let wanted = basic_of("alice", "s3cret");
    let registry = Registry::start(
        move |seen| match (seen.path.as_str(), &seen.authorization) {
            ("/v2/a/manifests/old", _) => Fake::redirect(302, "/v2/a/manifests/new"),
            ("/v2/a/manifests/new", _) => Fake::status(200),
            ("/v2/a/manifests/gone", _) => Fake::status(404),
            ("/v2/a/manifests/down", _) => Fake::status(503),
            (_, None) => Fake::challenge("Basic realm=\"r\""),
            (_, Some(sent)) if *sent == wanted => {
                Fake::status(200).with("docker-content-digest", "sha256:abc")
            }
            _ => Fake::status(403),
        },
    )
    .await;
    let upstream = client(logged_in(&registry.url()));
    let reply = upstream.head("/v2/a/manifests/t", "text/plain").await;
    let reply = reply.expect("a reply");
    assert_eq!(reply.header("docker-content-digest"), Some("sha256:abc"));
    assert_eq!(reply.header("etag"), None);
    let moved = upstream.head("/v2/a/manifests/old", "text/plain").await;
    assert!(moved.is_ok());
    let gone = upstream.head("/v2/a/manifests/gone", "text/plain").await;
    assert!(matches!(failure(gone), UpstreamError::NotFound));
    let down = upstream.head("/v2/a/manifests/down", "text/plain").await;
    assert!(matches!(failure(down), UpstreamError::Status(503)));
    let methods: Vec<String> = registry.seen().into_iter().map(|s| s.method).collect();
    assert!(methods.iter().all(|method| method == "HEAD"), "{methods:?}");
}

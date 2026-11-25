//! [`CredentialSource::Container`] against a fake container credentials endpoint.

use super::super::live::{Found, Plan};
use super::*;
use crate::s3::credentials::container;
use tierstore_testkit::dir::TestDir;
use tierstore_testkit::port::dead_addr;

/// Asserts that a full URI to loopback is asked with the authorization token of the
/// environment, and that its answer's keys and session token are the keys signed with.
#[tokio::test]
async fn a_full_uri_is_asked_with_the_authorization_token() {
    let answer = json_credentials(
        "ASIACONTAINER",
        "container-secret",
        "container-token",
        T0 + 3600,
    );
    let fake = Fake::start(move |_| (200, answer.clone())).await;
    let uri = format!("{}/v1/credentials", fake.url());
    let env = env(&[
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri),
        ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "the-auth-token"),
    ]);
    let clock = TestClock::at(T0);
    let keys = live(CredentialSource::Container, env, &clock)
        .keys()
        .await
        .unwrap();
    assert_eq!(keys.access_key_id, "ASIACONTAINER");
    assert_eq!(keys.secret_access_key, "container-secret");
    assert_eq!(keys.session_token.as_deref(), Some("container-token"));
    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, "/v1/credentials");
    assert_eq!(seen[0].header("authorization"), Some("the-auth-token"));
}

/// The error of asking the container source in `vars`, as text.
async fn container_error(vars: &[(&str, &str)]) -> String {
    let clock = TestClock::at(T0);
    live(CredentialSource::Container, env(vars), &clock)
        .keys()
        .await
        .err()
        .expect("the container source should fail")
        .to_string()
}

/// A fake endpoint that answers `status` and `body` to everything.
async fn answering(status: u16, body: String) -> Fake {
    Fake::start(move |_| (status, body.clone())).await
}

/// The endpoint `find` settles on, as its URL.
fn found_url(vars: &[(&str, &str)]) -> String {
    match container::find(&env(vars)).expect("find should succeed") {
        Found::Plan(Plan::Container(endpoint)) => endpoint.url.to_string(),
        _ => panic!("find should settle on the container endpoint"),
    }
}

/// The error of `find`, as text.
fn find_error(vars: &[(&str, &str)]) -> String {
    match container::find(&env(vars)) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("find should refuse"),
    }
}

const FULL: &str = "AWS_CONTAINER_CREDENTIALS_FULL_URI";
const RELATIVE: &str = "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI";

/// Asserts that a relative URI is asked at the ECS agent's address, with the path as given.
#[test]
fn a_relative_uri_is_asked_at_the_ecs_agent() {
    assert_eq!(
        found_url(&[(RELATIVE, "/v2/credentials/abc-123?x=1")]),
        "http://169.254.170.2/v2/credentials/abc-123?x=1"
    );
}

/// Asserts that a relative URI that does not start with a slash is refused with a message
/// naming the variable.
#[test]
fn a_relative_uri_without_a_leading_slash_is_refused() {
    assert_eq!(
        find_error(&[(RELATIVE, "v2/credentials")]),
        "credential source container: AWS_CONTAINER_CREDENTIALS_RELATIVE_URI does not start with /"
    );
}

/// Asserts that the relative URI wins when both it and a full URI are set, even if the full
/// URI would be refused.
#[test]
fn the_relative_uri_takes_precedence_over_the_full_uri() {
    assert_eq!(
        found_url(&[(RELATIVE, "/rel"), (FULL, "http://192.0.2.1/full")]),
        "http://169.254.170.2/rel"
    );
}

/// Asserts that neither variable set is "not set" for the source, naming both variables.
#[tokio::test]
async fn no_variable_is_not_set() {
    assert_eq!(
        container_error(&[]).await,
        "credential source container: AWS_CONTAINER_CREDENTIALS_RELATIVE_URI and \
         AWS_CONTAINER_CREDENTIALS_FULL_URI are not set"
    );
}

/// Asserts that a full URI is allowed over https to any host, and over http to loopback and to
/// the ECS and EKS agents' addresses, IPv4 and IPv6.
#[test]
fn full_uris_to_https_loopback_and_the_agents_are_allowed() {
    for uri in [
        "https://example.com/creds",
        "https://192.0.2.1/creds",
        "http://127.0.0.1:8080/creds",
        "http://127.9.9.9/creds",
        "http://[::1]:8080/creds",
        "http://169.254.170.2/creds",
        "http://169.254.170.23/creds",
        "http://[fd00:ec2::23]/creds",
    ] {
        let url = found_url(&[(FULL, uri)]);
        assert!(
            url.starts_with(&uri[..uri.find("/creds").unwrap()]),
            "{uri}: {url}"
        );
    }
}

/// Asserts that a full URI over http to another address or to a name is refused with the exact
/// message.
#[test]
fn a_full_uri_over_http_to_another_host_is_refused() {
    for uri in [
        "http://192.0.2.1/creds",
        "http://example.com/creds",
        "http://[2001:db8::1]/creds",
        "http://169.254.170.3/creds",
    ] {
        assert_eq!(
            find_error(&[(FULL, uri)]),
            "credential source container: AWS_CONTAINER_CREDENTIALS_FULL_URI is http to a host that \
             is not a loopback address or the ECS or EKS agent's; it must be https",
            "{uri}"
        );
    }
}

/// Asserts that a full URI whose scheme is neither http nor https is refused, naming the scheme.
#[test]
fn a_full_uri_with_another_scheme_is_refused() {
    assert_eq!(
        find_error(&[(FULL, "ftp://127.0.0.1/creds")]),
        "credential source container: AWS_CONTAINER_CREDENTIALS_FULL_URI has the scheme \"ftp\", \
         not http or https"
    );
}

/// Asserts that a full URI that is not a URL is refused with the parse error.
#[test]
fn a_full_uri_that_is_not_a_url_is_refused() {
    assert_eq!(
        find_error(&[(FULL, "not a url")]),
        "credential source container: AWS_CONTAINER_CREDENTIALS_FULL_URI is not a URL: \
         relative URL without a base"
    );
}

/// Asserts that without a token no authorization header is sent.
#[tokio::test]
async fn no_token_sends_no_authorization_header() {
    let fake = answering(200, json_credentials("AK", "SK", "TK", T0 + 3600)).await;
    let uri = format!("{}/c", fake.url());
    let clock = TestClock::at(T0);
    live(CredentialSource::Container, env(&[(FULL, &uri)]), &clock)
        .keys()
        .await
        .unwrap();
    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].header("authorization"), None);
}

/// Asserts that the token file wins over the token variable, is read again at each fetch (a
/// token rotated between fetches is the one sent), and has its surrounding whitespace removed.
#[tokio::test]
async fn the_token_file_wins_and_is_read_at_each_fetch() {
    let dir = TestDir::under(std::env::temp_dir(), "container-token-file");
    let file = dir.path().join("token");
    std::fs::write(&file, "first-token\n").unwrap();
    let fake = answering(200, json_credentials("AK", "SK", "TK", T0 + 3600)).await;
    let uri = format!("{}/c", fake.url());
    let file_text = file.to_str().unwrap().to_owned();
    let clock = TestClock::at(T0);
    let live = live(
        CredentialSource::Container,
        env(&[
            (FULL, &uri),
            ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", &file_text),
            ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "variable-token"),
        ]),
        &clock,
    );
    live.keys().await.unwrap();
    std::fs::write(&file, "  second-token \n\n").unwrap();
    live.keys().await.unwrap();
    assert_eq!(
        fake.seen().len(),
        1,
        "the keys are kept until their renewal"
    );
    clock.set(T0 + 3300);
    live.keys().await.unwrap();
    let tokens: Vec<_> = fake
        .seen()
        .iter()
        .map(|s| s.header("authorization").map(str::to_owned))
        .collect();
    assert_eq!(
        tokens,
        [
            Some("first-token".to_owned()),
            Some("second-token".to_owned())
        ]
    );
}

/// Asserts that a token file that is not there is an error naming the file and the cause, and
/// that no request is made.
#[tokio::test]
async fn a_missing_token_file_is_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "container-token-missing");
    let file = dir.path().join("absent");
    let file_text = file.to_str().unwrap().to_owned();
    let fake = answering(200, json_credentials("AK", "SK", "TK", T0 + 3600)).await;
    let uri = format!("{}/c", fake.url());
    let error = container_error(&[
        (FULL, &uri),
        ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", &file_text),
    ])
    .await;
    assert_eq!(
        error,
        format!(
            "credential source container: reading the authorization token file {file_text}: \
             No such file or directory (os error 2)"
        )
    );
    assert!(fake.seen().is_empty());
}

/// Asserts that a token with a line break inside it, from the file or from the variable, is
/// refused without being sent or repeated in the message.
#[tokio::test]
async fn a_token_with_a_line_break_is_refused_and_not_echoed() {
    let dir = TestDir::under(std::env::temp_dir(), "container-token-break");
    let file = dir.path().join("token");
    std::fs::write(&file, "secret-one\nsecret-two\n").unwrap();
    let file_text = file.to_str().unwrap().to_owned();
    let fake = answering(200, json_credentials("AK", "SK", "TK", T0 + 3600)).await;
    let uri = format!("{}/c", fake.url());
    let expected = "credential source container: the authorization token has a line break in it";
    let from_file = container_error(&[
        (FULL, &uri),
        ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", &file_text),
    ])
    .await;
    assert_eq!(from_file, expected);
    let from_variable = container_error(&[
        (FULL, &uri),
        (
            "AWS_CONTAINER_AUTHORIZATION_TOKEN",
            "secret-one\r\nsecret-two",
        ),
    ])
    .await;
    assert_eq!(from_variable, expected);
    assert!(fake.seen().is_empty());
}

/// Asserts that an answer other than 200 is an error with the status, and that the body is not
/// repeated.
#[tokio::test]
async fn a_status_other_than_200_is_an_error() {
    let fake = answering(403, "body-with-secret-xyz".to_owned()).await;
    let uri = format!("{}/c", fake.url());
    assert_eq!(
        container_error(&[(FULL, &uri)]).await,
        "credential source container: the container credentials endpoint answered 403"
    );
}

/// Asserts that an answer that is not JSON is an error with the line and column, and never the
/// body.
#[tokio::test]
async fn an_answer_that_is_not_json_is_an_error_without_the_body() {
    let fake = answering(200, "secret-body".to_owned()).await;
    let uri = format!("{}/c", fake.url());
    assert_eq!(
        container_error(&[(FULL, &uri)]).await,
        "credential source container: the answer is not the JSON of credentials (at line 1 column 1)"
    );
}

/// Asserts that an answer missing a field, or with it empty, is an error naming the field.
#[tokio::test]
async fn an_answer_missing_or_with_an_empty_field_is_an_error() {
    let all = [
        ("AccessKeyId", "AKID"),
        ("SecretAccessKey", "SKEY"),
        ("Token", "TOK"),
        ("Expiration", "2027-01-15T09:00:00Z"),
    ];
    for (name, _) in all {
        for empty in [false, true] {
            let fields: Vec<String> = all
                .iter()
                .filter(|(n, _)| empty || *n != name)
                .map(|(n, v)| {
                    let v = if empty && *n == name { "" } else { v };
                    format!("\"{n}\": \"{v}\"")
                })
                .collect();
            let fake = answering(200, format!("{{{}}}", fields.join(", "))).await;
            let uri = format!("{}/c", fake.url());
            assert_eq!(
                container_error(&[(FULL, &uri)]).await,
                format!("credential source container: the answer has no {name}"),
                "{name} empty={empty}"
            );
        }
    }
}

/// Asserts that a Code other than Success is an error naming the code and nothing else of the
/// answer.
#[tokio::test]
async fn a_code_other_than_success_is_an_error() {
    let answer = json_credentials("AKID-LEAK", "SECRET-LEAK", "TOKEN-LEAK", T0 + 3600)
        .replace("\"Success\"", "\"AssumeRoleUnauthorizedAccess\"");
    let fake = answering(200, answer).await;
    let uri = format!("{}/c", fake.url());
    assert_eq!(
        container_error(&[(FULL, &uri)]).await,
        "credential source container: the answer's Code is \"AssumeRoleUnauthorizedAccess\", \
         not \"Success\""
    );
}

/// Asserts that an Expiration that is not a UTC time is an error.
#[tokio::test]
async fn a_bad_expiration_is_an_error() {
    let answer =
        json_credentials("AK", "SK", "TK", T0 + 3600).replace(&iso(T0 + 3600), "tomorrow at noon");
    let fake = answering(200, answer).await;
    let uri = format!("{}/c", fake.url());
    assert_eq!(
        container_error(&[(FULL, &uri)]).await,
        "credential source container: its Expiration is not a UTC time YYYY-MM-DDTHH:MM:SSZ"
    );
}

/// Asserts that an answer longer than 64 KiB is refused.
#[tokio::test]
async fn an_answer_longer_than_64_kib_is_refused() {
    let fake = answering(200, "x".repeat(64 * 1024 + 1)).await;
    let uri = format!("{}/c", fake.url());
    assert_eq!(
        container_error(&[(FULL, &uri)]).await,
        "credential source container: the answer is longer than 65536 bytes"
    );
}

/// Asserts that an answer of exactly 64 KiB is read (and then fails as JSON, not as too long).
#[tokio::test]
async fn an_answer_of_exactly_64_kib_is_read() {
    let fake = answering(200, " ".repeat(64 * 1024)).await;
    let uri = format!("{}/c", fake.url());
    let error = container_error(&[(FULL, &uri)]).await;
    assert!(error.contains("is not the JSON of credentials"), "{error}");
}

/// Asserts that a request to an address nobody listens on is an error saying the request failed.
#[tokio::test]
async fn a_dead_endpoint_is_an_error() {
    let uri = format!("http://{}/c", dead_addr());
    let error = container_error(&[(FULL, &uri)]).await;
    assert!(
        error.starts_with("credential source container: the request failed"),
        "{error}"
    );
}

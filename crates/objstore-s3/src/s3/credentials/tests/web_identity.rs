//! [`CredentialSource::WebIdentity`] against a fake STS.

use std::path::PathBuf;

use tierstore_testkit::dir::TestDir;
use tierstore_testkit::port::dead_addr;

use super::super::sts;
use super::*;

const TOKEN: &str = "the-web-identity-token";
const ROLE: &str = "arn:aws:iam::123456789012:role/my role";
const ROLE_ENCODED: &str = "arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fmy%20role";

/// A scratch directory with a token file holding `text`.
fn token_file(text: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), "web-identity");
    let path = dir.path().join("token");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

/// The environment of a web identity role whose STS is at `endpoint`.
fn role_env(token: &std::path::Path, endpoint: &str) -> AwsEnv {
    env(&[
        ("AWS_WEB_IDENTITY_TOKEN_FILE", token.to_str().unwrap()),
        ("AWS_ROLE_ARN", ROLE),
        ("AWS_ENDPOINT_URL_STS", endpoint),
    ])
}

/// The error of the web identity source in `env`, which must fail.
async fn error_of(env: AwsEnv) -> super::super::CredentialsError {
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::WebIdentity, env, &clock);
    live.keys().await.err().unwrap()
}

/// Asserts that the failure message is exactly `expected` and holds no token.
fn assert_failed(error: &super::super::CredentialsError, expected: &str) {
    assert_eq!(error.to_string(), expected);
    assert!(!error.to_string().contains(TOKEN));
}

/// A fake STS that answers `status` and `body` to everything.
async fn sts_saying(status: u16, body: &str) -> Fake {
    let body = body.to_owned();
    Fake::start(move |_| (status, body.clone())).await
}

/// Asserts the request STS is sent: a POST to `/` as a form, whose body has the action, the
/// version, the percent-encoded role, the session name and the trimmed token; and that the
/// answer's keys, session token and expiry are what is signed with and renewed by.
#[tokio::test]
async fn the_token_is_exchanged_with_sts_for_the_keys_of_the_role() {
    let answer = sts_answer("ASIAROLE", "role-secret", "role-session-token", T0 + 3600);
    let fake = Fake::start(move |_| (200, answer.clone())).await;
    let (_dir, path) = token_file(&format!("  {TOKEN} \n"));
    let env = role_env(&path, fake.url());
    let clock = TestClock::at(T0);
    let keys = live(CredentialSource::WebIdentity, env, &clock)
        .keys()
        .await
        .unwrap();
    assert_eq!(keys.access_key_id, "ASIAROLE");
    assert_eq!(keys.secret_access_key, "role-secret");
    assert_eq!(keys.session_token.as_deref(), Some("role-session-token"));
    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/");
    assert_eq!(
        seen[0].header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        seen[0].body,
        format!(
            "Action=AssumeRoleWithWebIdentity&Version=2011-06-15&RoleArn={ROLE_ENCODED}\
             &RoleSessionName=objstore-s3-{T0}&WebIdentityToken={TOKEN}"
        )
    );
}

/// Asserts that `AWS_ROLE_SESSION_NAME` is the role session name sent.
#[tokio::test]
async fn the_role_session_name_of_the_environment_is_sent() {
    let answer = sts_answer("A", "S", "T", T0 + 3600);
    let fake = Fake::start(move |_| (200, answer.clone())).await;
    let (_dir, path) = token_file(TOKEN);
    let clock = TestClock::at(T0);
    let env = env(&[
        ("AWS_WEB_IDENTITY_TOKEN_FILE", path.to_str().unwrap()),
        ("AWS_ROLE_ARN", ROLE),
        ("AWS_ENDPOINT_URL_STS", fake.url()),
        ("AWS_ROLE_SESSION_NAME", "my-session"),
    ]);
    live(CredentialSource::WebIdentity, env, &clock)
        .keys()
        .await
        .unwrap();
    assert!(
        fake.seen()[0]
            .body
            .contains("&RoleSessionName=my-session&WebIdentityToken=")
    );
}

/// Asserts that an STS endpoint with a trailing slash is asked at the same path `/`.
#[tokio::test]
async fn an_endpoint_with_a_trailing_slash_is_asked_at_the_root() {
    let answer = sts_answer("A", "S", "T", T0 + 3600);
    let fake = Fake::start(move |_| (200, answer.clone())).await;
    let (_dir, path) = token_file(TOKEN);
    let endpoint = format!("{}/", fake.url());
    let clock = TestClock::at(T0);
    live(
        CredentialSource::WebIdentity,
        role_env(&path, &endpoint),
        &clock,
    )
    .keys()
    .await
    .unwrap();
    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/");
}

/// Asserts that the token file is read again at each exchange: after the clock passes the
/// renewal time, the request carries the token the file holds then, and the new keys are used.
#[tokio::test]
async fn the_token_file_is_read_again_at_each_renewal() {
    let first = sts_answer("FIRST", "s1", "t1", T0 + 3600);
    let fake = Fake::start(move |_| (200, first.clone())).await;
    let (_dir, path) = token_file("token-one");
    let clock = TestClock::at(T0);
    let live = live(
        CredentialSource::WebIdentity,
        role_env(&path, fake.url()),
        &clock,
    );
    assert_eq!(live.keys().await.unwrap().access_key_id, "FIRST");
    std::fs::write(&path, "token-two\n").unwrap();
    let second = sts_answer("SECOND", "s2", "t2", T0 + 3300 + 3600);
    fake.reroute(move |_| (200, second.clone()));
    clock.set(T0 + 3299);
    assert_eq!(live.keys().await.unwrap().access_key_id, "FIRST");
    assert_eq!(fake.seen().len(), 1);
    clock.set(T0 + 3300);
    assert_eq!(live.keys().await.unwrap().access_key_id, "SECOND");
    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].body.ends_with("&WebIdentityToken=token-one"));
    assert!(seen[1].body.ends_with("&WebIdentityToken=token-two"));
    assert!(seen[1].body.contains(&format!("objstore-s3-{}", T0 + 3300)));
}

/// Asserts that the source named with no `AWS_WEB_IDENTITY_TOKEN_FILE` is not set up.
#[tokio::test]
async fn without_a_token_file_variable_the_source_is_not_set() {
    let error = error_of(env(&[("AWS_ROLE_ARN", ROLE)])).await;
    assert_eq!(
        error.to_string(),
        "credential source web-identity: AWS_WEB_IDENTITY_TOKEN_FILE is not set"
    );
    assert!(matches!(
        error,
        super::super::CredentialsError::NotSet { .. }
    ));
}

/// Asserts that a token file with no role is refused with both variable names.
#[tokio::test]
async fn a_token_file_without_a_role_arn_is_refused() {
    let (_dir, path) = token_file(TOKEN);
    let env = env(&[("AWS_WEB_IDENTITY_TOKEN_FILE", path.to_str().unwrap())]);
    assert_failed(
        &error_of(env).await,
        "credential source web-identity: AWS_WEB_IDENTITY_TOKEN_FILE is set and AWS_ROLE_ARN is not",
    );
}

/// Asserts that a token file that is not there fails the exchange, naming the file, and that STS
/// is not asked.
#[tokio::test]
async fn a_missing_token_file_fails_without_asking_sts() {
    let fake = sts_saying(200, "unused").await;
    let dir = TestDir::under(std::env::temp_dir(), "web-identity-missing");
    let path = dir.path().join("absent");
    let error = error_of(role_env(&path, fake.url())).await;
    assert_failed(
        &error,
        &format!(
            "credential source web-identity: reading the web identity token file {}: \
             No such file or directory (os error 2)",
            path.display()
        ),
    );
    assert!(fake.seen().is_empty());
}

/// Asserts that a token file holding only whitespace fails the exchange as empty.
#[tokio::test]
async fn an_empty_token_file_fails() {
    let fake = sts_saying(200, "unused").await;
    let (_dir, path) = token_file(" \n\t\n");
    let error = error_of(role_env(&path, fake.url())).await;
    assert_failed(
        &error,
        &format!(
            "credential source web-identity: the web identity token file {} is empty",
            path.display()
        ),
    );
    assert!(fake.seen().is_empty());
}

/// Asserts that STS's error answer is reported with its status, code and message, and that the
/// token is in no part of it.
#[tokio::test]
async fn an_sts_error_answer_gives_its_code_and_message() {
    let body = format!(
        "<ErrorResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><Error>\
         <Type>Sender</Type><Code>InvalidIdentityToken</Code>\
         <Message>Token expired {TOKEN}</Message></Error></ErrorResponse>"
    );
    let fake = sts_saying(400, &body).await;
    let (_dir, path) = token_file(TOKEN);
    let error = error_of(role_env(&path, fake.url())).await;
    assert_eq!(
        error.to_string(),
        format!(
            "credential source web-identity: STS AssumeRoleWithWebIdentity answered 400 \
             InvalidIdentityToken: Token expired {TOKEN}"
        )
    );
}

/// Asserts that an error answer with a code and no message is reported with an empty message.
#[tokio::test]
async fn an_sts_error_answer_without_a_message_has_an_empty_one() {
    let body = "<ErrorResponse><Error><Code>AccessDenied</Code></Error></ErrorResponse>";
    let fake = sts_saying(403, body).await;
    let (_dir, path) = token_file(TOKEN);
    assert_failed(
        &error_of(role_env(&path, fake.url())).await,
        "credential source web-identity: STS AssumeRoleWithWebIdentity answered 403 AccessDenied: ",
    );
}

/// Asserts that an error answer that is not XML, and one that is XML with no `Code`, are
/// reported as having no error code, and that the body (which holds the token) is not repeated.
#[tokio::test]
async fn an_error_answer_without_a_code_says_so_and_not_its_body() {
    let (_dir, path) = token_file(TOKEN);
    let not_xml = sts_saying(500, &format!("oops {TOKEN}")).await;
    assert_failed(
        &error_of(role_env(&path, not_xml.url())).await,
        "credential source web-identity: STS AssumeRoleWithWebIdentity answered 500 with no error code",
    );
    let no_code = sts_saying(503, &format!("<Error><Message>{TOKEN}</Message></Error>")).await;
    assert_failed(
        &error_of(role_env(&path, no_code.url())).await,
        "credential source web-identity: STS AssumeRoleWithWebIdentity answered 503 with no error code",
    );
}

/// Asserts that a 200 answer that is not XML is refused as such.
#[tokio::test]
async fn a_success_answer_that_is_not_xml_is_refused() {
    let (_dir, path) = token_file(TOKEN);
    let fake = sts_saying(200, &format!("not xml {TOKEN}")).await;
    assert_failed(
        &error_of(role_env(&path, fake.url())).await,
        "credential source web-identity: STS AssumeRoleWithWebIdentity answered what is not XML",
    );
}

/// Asserts that a 200 answer with no `Credentials` element is refused as such.
#[tokio::test]
async fn a_success_answer_without_credentials_is_refused() {
    let (_dir, path) = token_file(TOKEN);
    let fake = sts_saying(200, "<AssumeRoleWithWebIdentityResponse/>").await;
    assert_failed(
        &error_of(role_env(&path, fake.url())).await,
        "credential source web-identity: STS AssumeRoleWithWebIdentity answered no Credentials",
    );
}

/// Asserts that each field of the credentials missing or empty is named in the refusal.
#[tokio::test]
async fn a_success_answer_without_a_field_names_the_field() {
    let (_dir, path) = token_file(TOKEN);
    let expiration = iso(T0 + 3600);
    let fields = [
        ("AccessKeyId", "A"),
        ("SecretAccessKey", "S"),
        ("SessionToken", "T"),
        ("Expiration", expiration.as_str()),
    ];
    for (missing, _) in fields {
        for empty in [false, true] {
            let inner: String = fields
                .iter()
                .map(|(name, value)| match (*name == missing, empty) {
                    (false, _) => format!("<{name}>{value}</{name}>"),
                    (true, true) => format!("<{name}>  </{name}>"),
                    (true, false) => String::new(),
                })
                .collect();
            let body = format!("<R><Credentials>{inner}</Credentials></R>");
            let fake = sts_saying(200, &body).await;
            assert_failed(
                &error_of(role_env(&path, fake.url())).await,
                &format!(
                    "credential source web-identity: STS AssumeRoleWithWebIdentity answered no {missing}"
                ),
            );
        }
    }
}

/// Asserts that an `Expiration` that is not a UTC time is refused with the reason.
#[tokio::test]
async fn a_bad_expiration_is_refused() {
    let (_dir, path) = token_file(TOKEN);
    let body = "<R><Credentials><AccessKeyId>A</AccessKeyId><SecretAccessKey>S</SecretAccessKey>\
                <SessionToken>T</SessionToken><Expiration>tomorrow</Expiration></Credentials></R>";
    let fake = sts_saying(200, body).await;
    assert_failed(
        &error_of(role_env(&path, fake.url())).await,
        "credential source web-identity: STS AssumeRoleWithWebIdentity: its Expiration is not a UTC time YYYY-MM-DDTHH:MM:SSZ",
    );
}

/// Asserts that an STS that cannot be reached fails the exchange with the request's failure, and
/// that the token is not in the error.
#[tokio::test]
async fn an_unreachable_sts_fails_the_exchange() {
    let (_dir, path) = token_file(TOKEN);
    let endpoint = format!("http://{}", dead_addr());
    let error = error_of(role_env(&path, &endpoint)).await;
    assert!(
        error.to_string().starts_with(
            "credential source web-identity: STS AssumeRoleWithWebIdentity: the request failed: "
        ),
        "{error}"
    );
    assert!(!error.to_string().contains(TOKEN));
}

/// Asserts that the STS endpoint is `https://sts.<region>.amazonaws.com`, with `.com.cn` for a
/// region of China, and that `AWS_ENDPOINT_URL_STS` replaces it with its trailing slashes cut.
#[test]
fn the_sts_endpoint_follows_the_region() {
    let none = env(&[]);
    assert_eq!(
        sts::endpoint(&none, "us-east-1"),
        "https://sts.us-east-1.amazonaws.com"
    );
    assert_eq!(
        sts::endpoint(&none, "eu-west-3"),
        "https://sts.eu-west-3.amazonaws.com"
    );
    assert_eq!(
        sts::endpoint(&none, "cn-north-1"),
        "https://sts.cn-north-1.amazonaws.com.cn"
    );
    let own = env(&[("AWS_ENDPOINT_URL_STS", "http://sts.internal:8080//")]);
    assert_eq!(
        sts::endpoint(&own, "cn-north-1"),
        "http://sts.internal:8080"
    );
    let empty = env(&[("AWS_ENDPOINT_URL_STS", "")]);
    assert_eq!(
        sts::endpoint(&empty, "us-east-1"),
        "https://sts.us-east-1.amazonaws.com"
    );
}

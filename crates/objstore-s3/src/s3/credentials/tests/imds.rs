//! [`CredentialSource::InstanceMetadata`] against a fake instance metadata service.

use super::super::live::{Found, Plan};
use super::*;
use crate::s3::credentials::imds;
use tierstore_testkit::port::dead_addr;

const ENDPOINT: &str = "AWS_EC2_METADATA_SERVICE_ENDPOINT";
const MODE: &str = "AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE";
const DISABLED: &str = "AWS_EC2_METADATA_DISABLED";
const ROLES: &str = "/latest/meta-data/iam/security-credentials/";

/// A fake IMDSv2 with the role `role` and the session token `token`; `credentials` is what the
/// role's path answers.
async fn imds_fake(role: &'static str, token: &'static str, credentials: String) -> Fake {
    Fake::start(
        move |seen| match (seen.method.as_str(), seen.path.as_str()) {
            ("PUT", "/latest/api/token") => (200, token.to_owned()),
            ("GET", ROLES) => (200, format!("{role}\n")),
            ("GET", path) if path == format!("{ROLES}{role}") => (200, credentials.clone()),
            _ => (404, String::new()),
        },
    )
    .await
}

/// The error of asking the instance metadata source in `vars`, as text.
async fn imds_error(vars: &[(&str, &str)]) -> String {
    let clock = TestClock::at(T0);
    live(CredentialSource::InstanceMetadata, env(vars), &clock)
        .keys()
        .await
        .err()
        .expect("the instance metadata source should fail")
        .to_string()
}

/// The error of `find`, as text.
fn find_error(vars: &[(&str, &str)]) -> String {
    match imds::find(&env(vars)) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("find should refuse"),
    }
}

/// Whether `find` settles on the service.
fn is_plan(vars: &[(&str, &str)]) -> bool {
    matches!(
        imds::find(&env(vars)).expect("find should succeed"),
        Found::Plan(Plan::Imds(_))
    )
}

/// Asserts that a fetch asks for a session token with its time to live, then reads the role
/// list and the role's credentials, both with the token, in that order, and parses the keys.
#[tokio::test]
async fn the_token_is_asked_first_and_sent_with_both_reads() {
    let answer = json_credentials("ASIAIMDS", "imds-secret", "imds-token", T0 + 3600);
    let fake = imds_fake("web-role", "session-token-1", answer).await;
    let url = fake.url().to_owned();
    let clock = TestClock::at(T0);
    let keys = live(
        CredentialSource::InstanceMetadata,
        env(&[(ENDPOINT, &url)]),
        &clock,
    )
    .keys()
    .await
    .unwrap();
    assert_eq!(keys.access_key_id, "ASIAIMDS");
    assert_eq!(keys.secret_access_key, "imds-secret");
    assert_eq!(keys.session_token.as_deref(), Some("imds-token"));
    let seen = fake.seen();
    let order: Vec<_> = seen
        .iter()
        .map(|s| (s.method.as_str(), s.path.as_str()))
        .collect();
    assert_eq!(
        order,
        [
            ("PUT", "/latest/api/token"),
            ("GET", ROLES),
            ("GET", "/latest/meta-data/iam/security-credentials/web-role"),
        ]
    );
    assert_eq!(
        seen[0].header("x-aws-ec2-metadata-token-ttl-seconds"),
        Some("21600")
    );
    assert_eq!(seen[0].header("x-aws-ec2-metadata-token"), None);
    assert_eq!(
        seen[1].header("x-aws-ec2-metadata-token"),
        Some("session-token-1")
    );
    assert_eq!(
        seen[2].header("x-aws-ec2-metadata-token"),
        Some("session-token-1")
    );
}

/// Asserts that an endpoint with a trailing slash is asked at the same paths as one without.
#[tokio::test]
async fn a_trailing_slash_on_the_endpoint_is_dropped() {
    let answer = json_credentials("AK", "SK", "TK", T0 + 3600);
    let fake = imds_fake("r", "t", answer).await;
    let url = format!("{}/", fake.url());
    let clock = TestClock::at(T0);
    live(
        CredentialSource::InstanceMetadata,
        env(&[(ENDPOINT, &url)]),
        &clock,
    )
    .keys()
    .await
    .unwrap();
    let paths: Vec<_> = fake.seen().iter().map(|s| s.path.clone()).collect();
    assert_eq!(
        paths,
        [
            "/latest/api/token".to_owned(),
            ROLES.to_owned(),
            format!("{ROLES}r")
        ]
    );
}

/// Asserts that the first non-blank line of the role list, trimmed, is the role asked for.
#[tokio::test]
async fn the_role_is_the_first_non_blank_line_of_the_list() {
    let answer = json_credentials("AK", "SK", "TK", T0 + 3600);
    let creds = answer.clone();
    let fake = Fake::start(
        move |seen| match (seen.method.as_str(), seen.path.as_str()) {
            ("PUT", _) => (200, "  tok\n".to_owned()),
            ("GET", ROLES) => (200, "\n  \r\n   first-role  \nsecond-role\n".to_owned()),
            ("GET", path) if path.ends_with("/first-role") => (200, creds.clone()),
            _ => (404, String::new()),
        },
    )
    .await;
    let url = fake.url().to_owned();
    let clock = TestClock::at(T0);
    live(
        CredentialSource::InstanceMetadata,
        env(&[(ENDPOINT, &url)]),
        &clock,
    )
    .keys()
    .await
    .unwrap();
    let seen = fake.seen();
    assert_eq!(seen[2].path, format!("{ROLES}first-role"));
    assert_eq!(seen[1].header("x-aws-ec2-metadata-token"), Some("tok"));
}

/// Asserts that a role name with characters that need encoding is percent-encoded in the path.
#[tokio::test]
async fn a_role_name_is_percent_encoded_in_the_path() {
    let answer = json_credentials("AK", "SK", "TK", T0 + 3600);
    let fake = Fake::start(
        move |seen| match (seen.method.as_str(), seen.path.as_str()) {
            ("PUT", _) => (200, "t".to_owned()),
            ("GET", ROLES) => (200, "my role/with+odd\u{e9}".to_owned()),
            ("GET", path) if path.ends_with("my%20role%2Fwith%2Bodd%C3%A9") => {
                (200, answer.clone())
            }
            _ => (404, String::new()),
        },
    )
    .await;
    let url = fake.url().to_owned();
    let clock = TestClock::at(T0);
    live(
        CredentialSource::InstanceMetadata,
        env(&[(ENDPOINT, &url)]),
        &clock,
    )
    .keys()
    .await
    .unwrap();
    assert_eq!(
        fake.seen()[2].path,
        format!("{ROLES}my%20role%2Fwith%2Bodd%C3%A9")
    );
}

/// Asserts that `AWS_EC2_METADATA_DISABLED=true`, in any case, turns the named source off with
/// an exact message, and that no request is made.
#[tokio::test]
async fn a_disabled_service_is_not_set() {
    let fake = imds_fake("r", "t", json_credentials("AK", "SK", "TK", T0 + 3600)).await;
    let url = fake.url().to_owned();
    for value in ["true", "TRUE", "True"] {
        assert_eq!(
            imds_error(&[(ENDPOINT, &url), (DISABLED, value)]).await,
            "credential source instance-metadata: AWS_EC2_METADATA_DISABLED is true",
            "{value}"
        );
    }
    assert!(fake.seen().is_empty());
}

/// Asserts that a `AWS_EC2_METADATA_DISABLED` other than true leaves the source on.
#[test]
fn a_disabled_variable_that_is_not_true_leaves_the_service_on() {
    assert!(is_plan(&[(DISABLED, "false")]));
    assert!(is_plan(&[(DISABLED, "")]));
}

/// Asserts that an endpoint that is not a URL is refused with the parse error.
#[test]
fn an_endpoint_that_is_not_a_url_is_refused() {
    assert_eq!(
        find_error(&[(ENDPOINT, "not a url")]),
        "credential source instance-metadata: AWS_EC2_METADATA_SERVICE_ENDPOINT is not a URL: \
         relative URL without a base"
    );
}

/// Asserts that an endpoint that is neither http nor https is refused.
#[test]
fn an_endpoint_with_another_scheme_is_refused() {
    assert_eq!(
        find_error(&[(ENDPOINT, "ftp://169.254.169.254")]),
        "credential source instance-metadata: AWS_EC2_METADATA_SERVICE_ENDPOINT is not an http \
         or https URL"
    );
}

/// Asserts that an https endpoint is accepted.
#[test]
fn an_https_endpoint_is_accepted() {
    assert!(is_plan(&[(ENDPOINT, "https://imds.example.com/")]));
}

/// Asserts that the endpoint mode is IPv4 or IPv6 in any case, or unset, and that another mode
/// is refused with an exact message; and that an endpoint wins over the mode.
#[test]
fn the_endpoint_mode_is_ipv4_or_ipv6() {
    assert!(is_plan(&[]));
    for mode in ["IPv4", "ipv4", "IPV4", "IPv6", "ipv6"] {
        assert!(is_plan(&[(MODE, mode)]), "{mode}");
    }
    assert_eq!(
        find_error(&[(MODE, "dual")]),
        "credential source instance-metadata: AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE is \"dual\", \
         not IPv4 or IPv6"
    );
    assert!(is_plan(&[(ENDPOINT, "http://127.0.0.1:1"), (MODE, "dual")]));
}

/// Asserts that a token request answered 403 or 405 is an error, with no fall-back to IMDSv1:
/// the role is never read.
#[tokio::test]
async fn a_refused_token_request_does_not_fall_back_to_imdsv1() {
    for status in [403, 405] {
        let fake = Fake::start(move |seen| {
            if seen.method == "PUT" {
                (status, "denied-body".to_owned())
            } else {
                (200, "should-not-be-asked".to_owned())
            }
        })
        .await;
        let url = fake.url().to_owned();
        assert_eq!(
            imds_error(&[(ENDPOINT, &url)]).await,
            format!(
                "credential source instance-metadata: the IMDSv2 token request answered {status}"
            )
        );
        let seen = fake.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "PUT");
    }
}

/// Asserts that an empty token, or one with a line break inside, is an error.
#[tokio::test]
async fn an_empty_token_is_an_error() {
    for body in ["", "  \n ", "ab\ncd"] {
        let fake = Fake::start(move |_| (200, body.to_owned())).await;
        let url = fake.url().to_owned();
        assert_eq!(
            imds_error(&[(ENDPOINT, &url)]).await,
            "credential source instance-metadata: the IMDSv2 token request answered no token",
            "{body:?}"
        );
        assert_eq!(fake.seen().len(), 1);
    }
}

/// Asserts that a role list answered 404, or answered empty or blank, says the instance has no
/// instance profile.
#[tokio::test]
async fn no_role_is_no_instance_profile() {
    let expected = "credential source instance-metadata: the instance has no instance profile \
                    (no IAM role)";
    for (status, body) in [(404, ""), (200, ""), (200, " \n\r\n  \n")] {
        let fake = Fake::start(move |seen| {
            if seen.method == "PUT" {
                (200, "t".to_owned())
            } else {
                (status, body.to_owned())
            }
        })
        .await;
        let url = fake.url().to_owned();
        assert_eq!(
            imds_error(&[(ENDPOINT, &url)]).await,
            expected,
            "{status} {body:?}"
        );
        assert_eq!(fake.seen().len(), 2);
    }
}

/// Asserts that a role list answered 500 is an error with the status.
#[tokio::test]
async fn a_failing_role_list_is_an_error() {
    let fake = Fake::start(move |seen| {
        if seen.method == "PUT" {
            (200, "t".to_owned())
        } else {
            (500, "oops".to_owned())
        }
    })
    .await;
    let url = fake.url().to_owned();
    assert_eq!(
        imds_error(&[(ENDPOINT, &url)]).await,
        "credential source instance-metadata: reading the instance's role answered 500"
    );
}

/// Asserts that credentials answered 404 are an error naming the role.
#[tokio::test]
async fn credentials_answered_404_are_an_error_naming_the_role() {
    let fake = Fake::start(move |seen| match seen.method.as_str() {
        "PUT" => (200, "t".to_owned()),
        _ if seen.path == ROLES => (200, "the-role".to_owned()),
        _ => (404, "secret-body".to_owned()),
    })
    .await;
    let url = fake.url().to_owned();
    assert_eq!(
        imds_error(&[(ENDPOINT, &url)]).await,
        "credential source instance-metadata: reading the credentials of role \"the-role\" \
         answered 404"
    );
}

/// Asserts that a Code other than Success is an error naming the code and no key.
#[tokio::test]
async fn a_code_other_than_success_is_an_error() {
    let answer = json_credentials("AKID-LEAK", "SECRET-LEAK", "TOKEN-LEAK", T0 + 3600)
        .replace("\"Success\"", "\"Failure\"");
    let fake = imds_fake("r", "t", answer).await;
    let url = fake.url().to_owned();
    assert_eq!(
        imds_error(&[(ENDPOINT, &url)]).await,
        "credential source instance-metadata: the answer's Code is \"Failure\", not \"Success\""
    );
}

/// Asserts that a service nobody listens on is an error saying the token request failed.
#[tokio::test]
async fn a_dead_endpoint_is_an_error() {
    let url = format!("http://{}", dead_addr());
    let error = imds_error(&[(ENDPOINT, &url)]).await;
    assert!(
        error.starts_with(
            "credential source instance-metadata: the IMDSv2 token request: the request failed"
        ),
        "{error}"
    );
}

/// Asserts that a role list too long is an error naming the step that failed.
#[tokio::test]
async fn a_role_list_too_long_names_the_step() {
    let fake = Fake::start(move |seen| match seen.method.as_str() {
        "PUT" => (200, "t".to_owned()),
        _ => (200, "x".repeat(64 * 1024 + 1)),
    })
    .await;
    let url = fake.url().to_owned();
    assert_eq!(
        imds_error(&[(ENDPOINT, &url)]).await,
        "credential source instance-metadata: reading the instance's role: the answer is longer \
         than 65536 bytes"
    );
}

/// Asserts that credentials answered too long name the role in the error.
#[tokio::test]
async fn a_credentials_answer_too_long_names_the_role() {
    let fake = Fake::start(move |seen| match seen.method.as_str() {
        "PUT" => (200, "t".to_owned()),
        _ if seen.path == ROLES => (200, "r1".to_owned()),
        _ => (200, "x".repeat(64 * 1024 + 1)),
    })
    .await;
    let url = fake.url().to_owned();
    assert_eq!(
        imds_error(&[(ENDPOINT, &url)]).await,
        "credential source instance-metadata: reading the credentials of role \"r1\": the \
         answer is longer than 65536 bytes"
    );
}

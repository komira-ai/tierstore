//! [`CredentialSource::Container`] against a fake container credentials endpoint.

use super::*;

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

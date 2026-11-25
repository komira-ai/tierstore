//! [`CredentialSource::Auto`]: the order of the sources, what a source that is set up and broken
//! does to the chain, and what it says when none is set up.

use tierstore_testkit::dir::TestDir;
use tierstore_testkit::port::dead_addr;

use super::*;
use crate::s3::CredentialsError;
use crate::s3::credentials::live::RETRY_AFTER;

const NONE_SET: &str = "environment: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are not set; \
     profile: AWS_SHARED_CREDENTIALS_FILE and AWS_CONFIG_FILE are not set; \
     web-identity: AWS_WEB_IDENTITY_TOKEN_FILE is not set; \
     container: AWS_CONTAINER_CREDENTIALS_RELATIVE_URI and AWS_CONTAINER_CREDENTIALS_FULL_URI are not set";

/// A container endpoint that gives the keys `id`.
async fn container_fake(id: &'static str) -> Fake {
    let answer = json_credentials(id, "container-secret", "container-token", T0 + 3600);
    Fake::start(move |_| (200, answer.clone())).await
}

/// An instance metadata service that gives the keys `id`.
async fn imds_fake(id: &'static str) -> Fake {
    let answer = json_credentials(id, "imds-secret", "imds-token", T0 + 3600);
    Fake::start(
        move |seen| match (seen.method.as_str(), seen.path.as_str()) {
            ("PUT", "/latest/api/token") => (200, "session-token".to_owned()),
            (_, "/latest/meta-data/iam/security-credentials/") => (200, "the-role".to_owned()),
            _ => (200, answer.clone()),
        },
    )
    .await
}

/// An STS that gives the keys `id`.
async fn sts_fake(id: &'static str) -> Fake {
    let answer = sts_answer(id, "sts-secret", "sts-token", T0 + 3600);
    Fake::start(move |_| (200, answer.clone())).await
}

/// The shared files of a test: `credentials` holds the profile `default` with the keys
/// `PROFILEKEY`, and the directory also has a web identity token file.
struct Files {
    dir: TestDir,
}

impl Files {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        std::fs::write(dir.path().join("token"), "web-identity-token\n").unwrap();
        Self { dir }
    }

    fn path(&self, file: &str) -> String {
        self.dir.path().join(file).display().to_string()
    }

    fn write_profile(&self) {
        let text =
            "[default]\naws_access_key_id = PROFILEKEY\naws_secret_access_key = profile-secret\n";
        std::fs::write(self.dir.path().join("credentials"), text).unwrap();
    }
}

/// Asserts that with the environment's keys, a container endpoint, a profile, a web identity and
/// an instance metadata service all set, the environment's keys are used and no fake is asked.
#[tokio::test]
async fn the_environment_wins() {
    let (container, imds, sts) = (
        container_fake("CONTAINER").await,
        imds_fake("IMDS").await,
        sts_fake("STS").await,
    );
    let files = Files::new("chain-env");
    files.write_profile();
    let (credentials, config) = (files.path("credentials"), files.path("config"));
    let (token, uri) = (files.path("token"), format!("{}/c", container.url()));
    let env = env(&[
        ("AWS_ACCESS_KEY_ID", "ENVKEY"),
        ("AWS_SECRET_ACCESS_KEY", "env-secret"),
        ("AWS_SESSION_TOKEN", "env-token"),
        ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
        ("AWS_CONFIG_FILE", &config),
        ("AWS_WEB_IDENTITY_TOKEN_FILE", &token),
        ("AWS_ROLE_ARN", "arn:aws:iam::1:role/r"),
        ("AWS_ENDPOINT_URL_STS", sts.url()),
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri),
        ("AWS_EC2_METADATA_SERVICE_ENDPOINT", imds.url()),
    ]);
    let clock = TestClock::at(T0);
    let keys = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .unwrap();
    assert_eq!(keys.access_key_id, "ENVKEY");
    assert_eq!(keys.secret_access_key, "env-secret");
    assert_eq!(keys.session_token.as_deref(), Some("env-token"));
    assert!(container.seen().is_empty());
    assert!(imds.seen().is_empty());
    assert!(sts.seen().is_empty());
}

/// Asserts that without the environment's keys the profile is used before the web identity,
/// the container endpoint and the instance metadata service, none of which is asked.
#[tokio::test]
async fn the_profile_wins_over_the_rest() {
    let (container, imds, sts) = (
        container_fake("CONTAINER").await,
        imds_fake("IMDS").await,
        sts_fake("STS").await,
    );
    let files = Files::new("chain-profile");
    files.write_profile();
    let (credentials, config) = (files.path("credentials"), files.path("config"));
    let (token, uri) = (files.path("token"), format!("{}/c", container.url()));
    let env = env(&[
        ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
        ("AWS_CONFIG_FILE", &config),
        ("AWS_WEB_IDENTITY_TOKEN_FILE", &token),
        ("AWS_ROLE_ARN", "arn:aws:iam::1:role/r"),
        ("AWS_ENDPOINT_URL_STS", sts.url()),
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri),
        ("AWS_EC2_METADATA_SERVICE_ENDPOINT", imds.url()),
    ]);
    let clock = TestClock::at(T0);
    let keys = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .unwrap();
    assert_eq!(keys.access_key_id, "PROFILEKEY");
    assert_eq!(keys.session_token, None);
    assert!(container.seen().is_empty());
    assert!(imds.seen().is_empty());
    assert!(sts.seen().is_empty());
}

/// Asserts that with no profile (the files are not there) the web identity is used before the
/// container endpoint and the instance metadata service, which are not asked.
#[tokio::test]
async fn the_web_identity_wins_over_the_container_and_imds() {
    let (container, imds, sts) = (
        container_fake("CONTAINER").await,
        imds_fake("IMDS").await,
        sts_fake("STS").await,
    );
    let files = Files::new("chain-web");
    let (credentials, config) = (files.path("credentials"), files.path("config"));
    let (token, uri) = (files.path("token"), format!("{}/c", container.url()));
    let env = env(&[
        ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
        ("AWS_CONFIG_FILE", &config),
        ("AWS_WEB_IDENTITY_TOKEN_FILE", &token),
        ("AWS_ROLE_ARN", "arn:aws:iam::1:role/r"),
        ("AWS_ENDPOINT_URL_STS", sts.url()),
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri),
        ("AWS_EC2_METADATA_SERVICE_ENDPOINT", imds.url()),
    ]);
    let clock = TestClock::at(T0);
    let keys = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .unwrap();
    assert_eq!(keys.access_key_id, "STS");
    assert_eq!(keys.session_token.as_deref(), Some("sts-token"));
    assert_eq!(sts.seen().len(), 1);
    assert!(container.seen().is_empty());
    assert!(imds.seen().is_empty());
}

/// Asserts that with no web identity the container endpoint is used before the instance
/// metadata service, which is not asked.
#[tokio::test]
async fn the_container_wins_over_imds() {
    let (container, imds) = (container_fake("CONTAINER").await, imds_fake("IMDS").await);
    let uri = format!("{}/c", container.url());
    let env = env(&[
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri),
        ("AWS_EC2_METADATA_SERVICE_ENDPOINT", imds.url()),
    ]);
    let clock = TestClock::at(T0);
    let keys = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .unwrap();
    assert_eq!(keys.access_key_id, "CONTAINER");
    assert_eq!(container.seen().len(), 1);
    assert!(imds.seen().is_empty());
}

/// Asserts that with only the instance metadata service reachable its role's keys are used.
#[tokio::test]
async fn imds_is_used_when_nothing_else_is_set() {
    let imds = imds_fake("IMDS").await;
    let env = env(&[("AWS_EC2_METADATA_SERVICE_ENDPOINT", imds.url())]);
    let clock = TestClock::at(T0);
    let keys = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .unwrap();
    assert_eq!(keys.access_key_id, "IMDS");
    assert_eq!(keys.secret_access_key, "imds-secret");
    assert_eq!(keys.session_token.as_deref(), Some("imds-token"));
}

/// Asserts that with nothing set and the instance metadata service turned off the error is
/// `NoSource` whose text says, in the chain's order, why each source is not set up.
#[tokio::test]
async fn no_source_lists_each_reason_in_order() {
    let env = env(&[("AWS_EC2_METADATA_DISABLED", "true")]);
    let clock = TestClock::at(T0);
    let error = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        format!(
            "no credential source is set up: {NONE_SET}; \
             instance-metadata: AWS_EC2_METADATA_DISABLED is true"
        )
    );
    assert!(matches!(error, CredentialsError::NoSource(_)));
}

/// Asserts that with nothing set and an instance metadata service that cannot be reached the
/// error is `NoSource` with the four reasons and then why the service gave nothing.
#[tokio::test]
async fn an_unreachable_imds_is_the_last_reason() {
    let endpoint = format!("http://{}", dead_addr());
    let env = env(&[("AWS_EC2_METADATA_SERVICE_ENDPOINT", &endpoint)]);
    let clock = TestClock::at(T0);
    let error = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .err()
        .unwrap();
    let prefix = format!(
        "no credential source is set up: {NONE_SET}; \
         instance-metadata: the IMDSv2 token request: the request failed: "
    );
    let message = error.to_string();
    assert!(message.starts_with(&prefix), "{message}");
    assert!(matches!(error, CredentialsError::NoSource(_)));
}

/// Asserts that a half of the environment's keys stops the chain with the environment's error,
/// and the container endpoint that is set is not asked.
#[tokio::test]
async fn a_broken_environment_stops_the_chain() {
    let container = container_fake("CONTAINER").await;
    let uri = format!("{}/c", container.url());
    let env = env(&[
        ("AWS_ACCESS_KEY_ID", "ENVKEY"),
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri),
    ]);
    let clock = TestClock::at(T0);
    let error = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "credential source environment: AWS_ACCESS_KEY_ID is set and AWS_SECRET_ACCESS_KEY is not"
    );
    assert!(container.seen().is_empty());
}

/// Asserts that a named profile that is in neither file stops the chain with the profile's
/// error, and the container endpoint that is set is not asked.
#[tokio::test]
async fn a_named_profile_that_is_missing_stops_the_chain() {
    let container = container_fake("CONTAINER").await;
    let files = Files::new("chain-missing-profile");
    let (credentials, config) = (files.path("credentials"), files.path("config"));
    let uri = format!("{}/c", container.url());
    let env = env(&[
        ("AWS_PROFILE", "absent"),
        ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
        ("AWS_CONFIG_FILE", &config),
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri),
    ]);
    let clock = TestClock::at(T0);
    let error = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        format!("credential source profile: no profile \"absent\" in {credentials} or {config}")
    );
    assert!(container.seen().is_empty());
}

/// Asserts that a web identity token file that is not there stops the chain with the web
/// identity's error, and the container endpoint that is set is not asked.
#[tokio::test]
async fn a_missing_web_identity_token_stops_the_chain() {
    let container = container_fake("CONTAINER").await;
    let files = Files::new("chain-missing-token");
    let token = files.path("no-such-token");
    let uri = format!("{}/c", container.url());
    let env = env(&[
        ("AWS_WEB_IDENTITY_TOKEN_FILE", &token),
        ("AWS_ROLE_ARN", "arn:aws:iam::1:role/r"),
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri),
    ]);
    let clock = TestClock::at(T0);
    let error = live(CredentialSource::Auto, env, &clock)
        .keys()
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        format!(
            "credential source web-identity: reading the web identity token file {token}: \
             No such file or directory (os error 2)"
        )
    );
    assert!(container.seen().is_empty());
}

/// Asserts that a first fetch that fails settles on nothing: the container endpoint that
/// answered 500 is not asked again within `RETRY_AFTER` (the same error, no new request), and is
/// asked again after it, which gets its keys once it answers.
#[tokio::test]
async fn a_failed_first_fetch_is_asked_again() {
    let container = Fake::start(|_| (500, String::new())).await;
    let uri = format!("{}/c", container.url());
    let env = env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri)]);
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Auto, env, &clock);
    let error = live.keys().await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "credential source container: the container credentials endpoint answered 500"
    );
    let answer = json_credentials(
        "CONTAINER",
        "container-secret",
        "container-token",
        T0 + 3600,
    );
    container.reroute(move |_| (200, answer.clone()));
    clock.set(T0 + RETRY_AFTER.as_secs() - 1);
    assert_eq!(live.keys().await.err().unwrap(), error);
    assert_eq!(container.seen().len(), 1);
    clock.set(T0 + RETRY_AFTER.as_secs());
    assert_eq!(live.keys().await.unwrap().access_key_id, "CONTAINER");
    assert_eq!(container.seen().len(), 2);
}

/// Asserts that a chain that found no source is walked again once `RETRY_AFTER` has passed (not
/// before: the same error is given): a profile file written after the first failure is found.
#[tokio::test]
async fn a_chain_that_found_nothing_is_walked_again() {
    let files = Files::new("chain-walked-again");
    let (credentials, config) = (files.path("credentials"), files.path("config"));
    let env = env(&[
        ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
        ("AWS_CONFIG_FILE", &config),
        ("AWS_EC2_METADATA_DISABLED", "true"),
    ]);
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Auto, env, &clock);
    let error = live.keys().await.err().unwrap();
    assert!(matches!(error, CredentialsError::NoSource(_)), "{error}");
    files.write_profile();
    clock.set(T0 + RETRY_AFTER.as_secs() - 1);
    assert_eq!(live.keys().await.err().unwrap(), error);
    clock.set(T0 + RETRY_AFTER.as_secs());
    assert_eq!(live.keys().await.unwrap().access_key_id, "PROFILEKEY");
}

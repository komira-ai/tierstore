//! Credentials at the store: what a request is signed with, and what a store whose credentials
//! come from a source does with them.

use bytes::Bytes;
use reqwest::Method;

use super::*;
use crate::s3::fake::FakeS3;
use crate::s3::sigv4::{EMPTY_SHA256, Request, Signer};
use crate::s3::tests::{config, keys};
use crate::s3::{S3Config, S3Store};
use crate::types::{ByteRange, ObjectKey};
use crate::{Capabilities, ObjectStore, ObjectStoreError};

/// Far past any clock of the test machine: 2099-01-01T00:00:00Z.
const FAR: u64 = 4_070_908_800;

/// The request of `keys` for the bucket `example-bucket` of a store at `127.0.0.1:9000`, and the
/// `Authorization` the signer makes for the headers `signed` (those the store adds besides the
/// date, in order) with the date the request carries.
fn built(
    keys_of: &crate::s3::credentials::Keys,
    extra: &[(&str, &str)],
) -> (reqwest::Request, String) {
    let store = S3Store::new(config("http://127.0.0.1:9000", "example-bucket")).unwrap();
    let request = store
        .conn
        .signed_payload(keys_of, Method::GET, None, &[], Vec::new(), EMPTY_SHA256)
        .build()
        .unwrap();
    let date = request.headers()["x-amz-date"].to_str().unwrap().to_owned();
    let mut headers = vec![
        ("x-amz-content-sha256", EMPTY_SHA256.to_owned()),
        ("x-amz-date", date.clone()),
    ];
    headers.extend(extra.iter().map(|(n, v)| (*n, (*v).to_owned())));
    let expected = Signer {
        access_key_id: &keys_of.access_key_id,
        secret_access_key: &keys_of.secret_access_key,
        region: "us-east-1",
    }
    .authorization(
        &Request {
            method: "GET",
            host: "127.0.0.1:9000",
            path: "/example-bucket",
            query: &[],
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        },
        &date,
    );
    (request, expected)
}

/// Asserts that keys with a session token are sent with `x-amz-security-token`, that the header
/// is among the signed headers, and that the signature is the one the signer makes over it.
#[test]
fn a_session_token_is_sent_and_signed() {
    let (request, expected) = built(
        &keys("ASIA", Some("the-session-token")),
        &[("x-amz-security-token", "the-session-token")],
    );
    assert_eq!(
        request.headers()["x-amz-security-token"],
        "the-session-token"
    );
    let authorization = request.headers()["authorization"].to_str().unwrap();
    assert_eq!(authorization, expected);
    assert!(
        authorization.contains("x-amz-security-token"),
        "{authorization}"
    );
}

/// Asserts that keys with no session token send no `x-amz-security-token`, and sign without it.
#[test]
fn no_session_token_means_no_token_header() {
    let (request, expected) = built(&keys("AKIA", None), &[]);
    assert!(!request.headers().contains_key("x-amz-security-token"));
    let authorization = request.headers()["authorization"].to_str().unwrap();
    assert_eq!(authorization, expected);
    assert!(!authorization.contains("x-amz-security-token"));
}

/// A configuration of the store of `s3` whose credentials are the container endpoint at `uri`.
fn from_container(s3: &FakeS3, uri: &str) -> S3Config {
    let mut config = s3.config(Capabilities::default());
    let env = env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", uri)]);
    config.credentials = Credentials::from_source(CredentialSource::Container, env);
    config
}

/// Asserts that a store whose credentials come from a container endpoint signs with the keys it
/// gave, so a service that requires those keys accepts a write and a read, and that the
/// endpoint was asked once for both.
#[tokio::test]
async fn a_store_signs_with_the_keys_of_its_source() {
    let answer = json_credentials("ASIACONTAINER", "container-secret", "container-token", FAR);
    let container = Fake::start(move |_| (200, answer.clone())).await;
    let s3 = FakeS3::start().await;
    s3.require_credentials("ASIACONTAINER", "container-secret");
    let uri = format!("{}/creds", container.url());
    let store = S3Store::new(from_container(&s3, &uri)).unwrap();
    let key = ObjectKey::new("object".to_owned()).unwrap();
    store
        .put_new(&key, Bytes::from_static(b"hello"), None)
        .await
        .unwrap();
    let got = store
        .get_range(&key, ByteRange::new(0, 5).unwrap())
        .await
        .unwrap();
    assert_eq!(&got[..], b"hello");
    assert_eq!(container.seen().len(), 1);
}

/// Asserts that when the source cannot give credentials the write fails with
/// `ObjectStoreError::Credentials` and its exact text, and the service was sent nothing.
#[tokio::test]
async fn a_store_whose_source_fails_sends_nothing() {
    let container = Fake::start(|_| (500, "secret-in-a-body".to_owned())).await;
    let s3 = FakeS3::start().await;
    let uri = format!("{}/creds", container.url());
    let store = S3Store::new(from_container(&s3, &uri)).unwrap();
    let key = ObjectKey::new("object".to_owned()).unwrap();
    let error = store
        .put_new(&key, Bytes::from_static(b"hello"), None)
        .await
        .unwrap_err();
    assert!(matches!(error, ObjectStoreError::Credentials(_)));
    assert_eq!(
        error.to_string(),
        "credential source container: the container credentials endpoint answered 500"
    );
    assert!(s3.requests().is_empty());
}

/// Asserts that `Debug` of a key pair with a session token shows the access key id, and
/// `<redacted>` for the secret and the token, never their text; one with no token has no token
/// field.
#[test]
fn debug_redacts_the_secrets_of_fixed_keys() {
    let text = format!(
        "{:?}",
        Credentials::session("AKID", "the-secret", "the-token")
    );
    assert_eq!(
        text,
        "Credentials { access_key_id: \"AKID\", secret_access_key: \"<redacted>\", \
         session_token: \"<redacted>\" }"
    );
    let text = format!("{:?}", Credentials::new("AKID", "the-secret"));
    assert_eq!(
        text,
        "Credentials { access_key_id: \"AKID\", secret_access_key: \"<redacted>\" }"
    );
}

/// Asserts that `Debug` of credentials from a source shows the source, and nothing of the
/// environment's values.
#[test]
fn debug_shows_the_source() {
    let env = env(&[("AWS_SECRET_ACCESS_KEY", "the-secret")]);
    let text = format!(
        "{:?}",
        Credentials::from_source(CredentialSource::Profile, env)
    );
    assert_eq!(text, "Credentials { source: Profile }");
}

/// Asserts the name each source has in text: the names the module's errors use.
#[test]
fn sources_display_their_names() {
    let names = [
        (CredentialSource::Auto, "auto"),
        (CredentialSource::Environment, "environment"),
        (CredentialSource::Profile, "profile"),
        (CredentialSource::WebIdentity, "web-identity"),
        (CredentialSource::Container, "container"),
        (CredentialSource::InstanceMetadata, "instance-metadata"),
    ];
    for (source, name) in names {
        assert_eq!(source.to_string(), name);
    }
}

/// Asserts that a store whose credentials come from a source loads with an `http` endpoint and
/// asks nothing at that time.
#[tokio::test]
async fn a_store_with_a_source_loads_without_asking() {
    let container = Fake::start(|_| (500, String::new())).await;
    let uri = format!("{}/creds", container.url());
    let mut config = config("http://127.0.0.1:9000", "example-bucket");
    let env = env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri)]);
    config.credentials = Credentials::from_source(CredentialSource::Auto, env);
    assert!(S3Store::new(config).is_ok());
    assert!(container.seen().is_empty());
}

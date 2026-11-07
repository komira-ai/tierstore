//! `S3Store::new_rotating`: one store reached through several endpoints, each request going to the
//! next endpoint in turn, against S3 services on loopback that check the signature of every
//! request against the `Host` they received.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::time::Duration;

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::s3::{Credentials, S3Config, S3ConfigError, S3Store, S3Tuning};
use objstore_s3::{Capabilities, ObjectKey, ObjectStore};

fn config(endpoint: &str) -> S3Config {
    S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tierstore-fake".to_owned(),
        credentials: Credentials::new("fake-key", "fake-secret"),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(60),
        min_body_rate: None,
    }
}

/// `count` services that refuse a request not signed for the `Host` they received.
async fn services(count: usize) -> Vec<FakeS3> {
    let mut all = Vec::new();
    for _ in 0..count {
        let fake = FakeS3::start().await;
        fake.require_credentials("fake-key", "fake-secret");
        all.push(fake);
    }
    all
}

fn key(n: usize) -> ObjectKey {
    ObjectKey::new(format!("k{n}")).unwrap()
}

/// Asserts that nine requests over three endpoints reach them first, second, third, first, ...,
/// and that each is signed for the endpoint it reached (a service answers 403 to any other).
#[tokio::test]
async fn requests_go_to_the_endpoints_in_turn_each_signed_for_its_host() {
    let fakes = services(3).await;
    let also = [
        fakes[1].endpoint().to_owned(),
        fakes[2].endpoint().to_owned(),
    ];
    let store =
        S3Store::new_rotating(config(fakes[0].endpoint()), S3Tuning::default(), &also).unwrap();
    let shown = format!("{store:?}");
    assert!(
        shown.contains(&format!("also: [\"{}/\", \"{}/\"]", also[0], also[1])),
        "{shown}"
    );
    for n in 0..9 {
        store
            .put_new(&key(n), Bytes::from_static(b"x"), None)
            .await
            .unwrap();
    }
    for (at, fake) in fakes.iter().enumerate() {
        let expected: Vec<String> = (0..9)
            .filter(|n| n % 3 == at)
            .map(|n| format!("PUT /tierstore-fake/k{n}?"))
            .collect();
        assert_eq!(fake.requests(), expected, "endpoint {at}");
    }
}

/// Asserts that a list of one endpoint, and `new_tuned`, send every request to that endpoint,
/// signed for it, as a store with `endpoint` alone always did.
#[tokio::test]
async fn one_endpoint_gets_every_request_as_before() {
    let fakes = services(2).await;
    let rotating =
        S3Store::new_rotating(config(fakes[0].endpoint()), S3Tuning::default(), &[]).unwrap();
    let plain = S3Store::new_tuned(config(fakes[1].endpoint()), S3Tuning::default()).unwrap();
    for n in 0..4 {
        for store in [&rotating, &plain] {
            store
                .put_new(&key(n), Bytes::from_static(b"x"), None)
                .await
                .unwrap();
        }
    }
    let expected: Vec<String> = (0..4)
        .map(|n| format!("PUT /tierstore-fake/k{n}?"))
        .collect();
    assert_eq!(fakes[0].requests(), expected);
    assert_eq!(fakes[1].requests(), expected);
}

/// Asserts that an endpoint given twice is refused, also when written another way (a trailing
/// slash), and so is a listed endpoint that is not `http://host[:port]`, naming that entry.
#[tokio::test]
async fn a_repeated_or_malformed_endpoint_in_the_list_is_refused() {
    let fake = FakeS3::start().await;
    let slash = format!("{}/", fake.endpoint());
    let refusal = |also: &[String]| {
        let built = S3Store::new_rotating(config(fake.endpoint()), S3Tuning::default(), also);
        match built {
            Err(S3ConfigError::Endpoint { endpoint, why }) => (endpoint, why),
            other => panic!("expected an endpoint refusal, got {:?}", other.map(|_| ())),
        }
    };
    assert_eq!(
        refusal(std::slice::from_ref(&slash)),
        (slash.clone(), "given twice"),
        "same host, written with a slash"
    );
    let tls = "https://127.0.0.1:1".to_owned();
    assert_eq!(
        refusal(std::slice::from_ref(&tls)),
        (tls, "TLS is not built yet; use http")
    );
}

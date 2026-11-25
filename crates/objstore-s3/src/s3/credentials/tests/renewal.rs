//! How [`Live`] keeps and renews temporary credentials, against a fake container endpoint and a
//! clock a test sets.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::s3::CredentialsError;
use crate::s3::credentials::live::{MARGIN, RENEW_BEFORE, RETRY_AFTER};

/// A fake container endpoint that answers each request with the credentials `ID<n>` (`n` the
/// count of requests so far, from 1), expiring at `expires`; and its environment.
async fn endpoint(expires: u64) -> (Fake, AwsEnv) {
    let counter = Arc::new(AtomicUsize::new(0));
    let fake = Fake::start(move |_| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        (
            200,
            json_credentials(&format!("ID{n}"), "the-secret", "the-token", expires),
        )
    })
    .await;
    let uri = format!("{}/creds", fake.url());
    let env = env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri)]);
    (fake, env)
}

/// Asserts that credentials expiring in an hour are fetched once and kept: asking again at once
/// makes no request.
#[tokio::test]
async fn credentials_are_fetched_once_and_kept() {
    let (fake, env) = endpoint(T0 + 3600).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    let first = live.keys().await.unwrap();
    let second = live.keys().await.unwrap();
    assert_eq!(first.access_key_id, "ID1");
    assert_eq!(second.access_key_id, "ID1");
    assert_eq!(fake.seen().len(), 1);
}

/// Asserts that the kept keys are signed with until one second before 5 minutes before the
/// expiry, and that at 5 minutes before it new ones are fetched.
#[tokio::test]
async fn credentials_are_renewed_five_minutes_before_they_expire() {
    let (fake, env) = endpoint(T0 + 3600).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    live.keys().await.unwrap();
    let renew_at = T0 + 3600 - RENEW_BEFORE.as_secs();
    clock.set(renew_at - 1);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    assert_eq!(fake.seen().len(), 1);
    clock.set(renew_at);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID2");
    assert_eq!(fake.seen().len(), 2);
}

/// Asserts that a renewal that fails while more than a minute is left signs with the kept keys,
/// asks nothing again until 10 seconds have passed, and then asks again.
#[tokio::test]
async fn a_failed_renewal_signs_with_the_kept_keys_and_waits_to_retry() {
    let (fake, env) = endpoint(T0 + 3600).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    live.keys().await.unwrap();
    fake.reroute(|_| (500, String::new()));
    let due = T0 + 3600 - RENEW_BEFORE.as_secs();
    clock.set(due);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    assert_eq!(fake.seen().len(), 2);
    let retry = RETRY_AFTER.as_secs();
    assert_eq!(retry, 10);
    clock.set(due + retry - 1);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    assert_eq!(fake.seen().len(), 2);
    clock.set(due + retry);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    assert_eq!(fake.seen().len(), 3);
    let answer = json_credentials("NEW", "the-secret", "the-token", T0 + 7200);
    fake.reroute(move |_| (200, answer.clone()));
    clock.set(due + 2 * retry);
    assert_eq!(live.keys().await.unwrap().access_key_id, "NEW");
    assert_eq!(fake.seen().len(), 4);
}

/// Asserts that a renewal that fails with a minute or less left is `Expired` with the full
/// message (the expiry and the inner error), and never signs with the kept keys; with 61
/// seconds left the kept keys are still signed with.
#[tokio::test]
async fn a_failed_renewal_with_a_minute_left_is_expired() {
    let margin = MARGIN.as_secs();
    assert_eq!(margin, 60);
    // With 61 seconds left the kept keys are still signed with.
    let (fake, env) = endpoint(T0 + 3600).await;
    let clock = TestClock::at(T0);
    let kept = live(CredentialSource::Container, env, &clock);
    kept.keys().await.unwrap();
    fake.reroute(|_| (500, String::new()));
    clock.set(T0 + 3600 - margin - 1);
    assert_eq!(kept.keys().await.unwrap().access_key_id, "ID1");
    // With 60 left they are not.
    let (fake, env) = endpoint(T0 + 3600).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    live.keys().await.unwrap();
    fake.reroute(|_| (500, String::new()));
    clock.set(T0 + 3600 - margin);
    let error = live.keys().await.err().unwrap();
    let message = error.to_string();
    assert_eq!(
        message,
        format!(
            "credential source container: the credentials expire at {} and could not be \
             renewed: credential source container: the container credentials endpoint answered 500",
            iso(T0 + 3600)
        )
    );
    assert!(!message.contains("the-secret"));
    assert!(!message.contains("the-token"));
    assert!(matches!(error, CredentialsError::Expired { .. }));
}

/// Asserts that credentials that last 4 minutes are renewed at half their lifetime, not 5
/// minutes before the end.
#[tokio::test]
async fn short_lived_credentials_are_renewed_at_half_their_lifetime() {
    let (fake, env) = endpoint(T0 + 240).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    live.keys().await.unwrap();
    clock.set(T0 + 119);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    assert_eq!(fake.seen().len(), 1);
    clock.set(T0 + 120);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID2");
    assert_eq!(fake.seen().len(), 2);
}

/// Asserts that credentials that arrive with a minute or less left are refused with the full
/// `Expired` message, and that with 61 seconds left they are accepted.
#[tokio::test]
async fn credentials_that_arrive_with_a_minute_left_are_refused() {
    let (_fake, env) = endpoint(T0 + 60).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    let error = live.keys().await.err().unwrap();
    assert_eq!(
        error.to_string(),
        format!(
            "credential source container: the credentials expire at {} and could not be \
             renewed: the source gave credentials with a minute or less left",
            iso(T0 + 60)
        )
    );
    let (_fake, env) = endpoint(T0 + 61).await;
    let live = super::live(CredentialSource::Container, env, &clock);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
}

/// Asserts that credentials that are already expired when they arrive are refused as `Expired`
/// with their expiry in the message, and that they are not kept: a call within `RETRY_AFTER`
/// gets the same error with no request, and one after it asks again.
#[tokio::test]
async fn credentials_expired_on_arrival_are_refused() {
    let (fake, env) = endpoint(T0 - 10).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    for (asked, at) in [(1, T0), (1, T0 + 9), (2, T0 + 10)] {
        clock.set(at);
        let error = live.keys().await.err().unwrap();
        assert_eq!(
            error.to_string(),
            format!(
                "credential source container: the credentials expire at {} and could not be \
                 renewed: the source gave credentials with a minute or less left",
                iso(T0 - 10)
            )
        );
        assert_eq!(fake.seen().len(), asked);
    }
}

/// Asserts that a key pair given directly is never renewed and never asks the endpoint of
/// [`Live::new`]: the closure that would make the client is not called.
#[tokio::test]
async fn fixed_keys_ask_nothing() {
    let clock = TestClock::at(T0);
    let fixed = Credentials::new("AKIDFIXED", "fixed-secret");
    let live = Live::new(
        &fixed,
        || -> Result<Http, String> { panic!("a fixed key pair needs no client") },
        "us-east-1",
        clock.clock(),
    )
    .unwrap();
    let keys = live.keys().await.unwrap();
    assert_eq!(keys.access_key_id, "AKIDFIXED");
    assert_eq!(keys.secret_access_key, "fixed-secret");
    assert_eq!(keys.session_token, None);
    clock.set(T0 + 1_000_000_000);
    assert_eq!(live.keys().await.unwrap().access_key_id, "AKIDFIXED");
}

/// Asserts that a key pair with a session token is handed out as it is, token included, with no
/// client made.
#[tokio::test]
async fn fixed_session_keys_ask_nothing() {
    let clock = TestClock::at(T0);
    let fixed = Credentials::session("AKIDFIXED", "fixed-secret", "fixed-token");
    let live = Live::new(
        &fixed,
        || -> Result<Http, String> { Err("no client".to_owned()) },
        "us-east-1",
        clock.clock(),
    )
    .unwrap();
    let keys = live.keys().await.unwrap();
    assert_eq!(keys.session_token.as_deref(), Some("fixed-token"));
}

/// Asserts that making the live credentials of a source returns the error of the closure that
/// makes the client.
#[test]
fn a_source_returns_the_error_of_the_client() {
    let clock = TestClock::at(T0);
    let credentials = Credentials::from_source(CredentialSource::Container, env(&[]));
    let made = Live::new(
        &credentials,
        || -> Result<Http, String> { Err("no client".to_owned()) },
        "us-east-1",
        clock.clock(),
    );
    assert_eq!(made.err(), Some("no client".to_owned()));
}

/// Asserts that 8 concurrent requests for keys make one request of the endpoint, first and when
/// a renewal is due: at first the others wait for the keys of the one asking; at a renewal they
/// sign with the kept keys, so one gets the new keys and seven the kept ones.
#[tokio::test]
async fn concurrent_requests_make_one_fetch() {
    let (fake, env) = endpoint(T0 + 3600).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    let all = futures::future::join_all((0..8).map(|_| live.keys())).await;
    assert!(
        all.iter()
            .all(|keys| keys.as_ref().unwrap().access_key_id == "ID1")
    );
    assert_eq!(fake.seen().len(), 1);
    clock.set(T0 + 3600 - RENEW_BEFORE.as_secs());
    let all = futures::future::join_all((0..8).map(|_| live.keys())).await;
    let renewed = all
        .iter()
        .filter(|keys| keys.as_ref().unwrap().access_key_id == "ID2")
        .count();
    let kept = all
        .iter()
        .filter(|keys| keys.as_ref().unwrap().access_key_id == "ID1")
        .count();
    assert_eq!((renewed, kept), (1, 7));
    assert_eq!(fake.seen().len(), 2);
}

/// Asserts that `Auto` settles on the source it found: a renewal asks the container endpoint
/// again and does not walk the chain, so the instance metadata service is never asked.
#[tokio::test]
async fn auto_renews_from_the_source_it_settled_on() {
    let (fake, container_env) = endpoint(T0 + 3600).await;
    let imds = Fake::start(|_| (500, String::new())).await;
    let uri = format!("{}/creds", fake.url());
    let _ = container_env;
    let env = env(&[
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri),
        ("AWS_EC2_METADATA_SERVICE_ENDPOINT", imds.url()),
    ]);
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Auto, env, &clock);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    clock.set(T0 + 3600 - RENEW_BEFORE.as_secs());
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID2");
    assert_eq!(fake.seen().len(), 2);
    assert!(imds.seen().is_empty());
}

/// Asserts that keys with 90 seconds of life (renewed 45 seconds before their end) asked at 61
/// seconds left are kept and make no request, and asked at 59 seconds left (still fresh, but
/// within the margin) are not signed with: a second request is made and its keys are returned.
#[tokio::test]
async fn kept_keys_within_the_margin_are_not_signed_with_though_fresh() {
    let (fake, env) = endpoint(T0 + 90).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    clock.set(T0 + 29);
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    assert_eq!(fake.seen().len(), 1);
    let answer = json_credentials("NEW", "the-secret", "the-token", T0 + 3600);
    fake.reroute(move |_| (200, answer.clone()));
    clock.set(T0 + 31);
    assert_eq!(live.keys().await.unwrap().access_key_id, "NEW");
    assert_eq!(fake.seen().len(), 2);
}

/// Asserts that when that fetch fails the kept keys within the margin are not signed with: the
/// result is `Expired` with the full message.
#[tokio::test]
async fn a_failed_fetch_for_fresh_keys_within_the_margin_is_expired() {
    let (fake, env) = endpoint(T0 + 90).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    live.keys().await.unwrap();
    fake.reroute(|_| (500, String::new()));
    clock.set(T0 + 31);
    let error = live.keys().await.err().unwrap();
    assert_eq!(
        error.to_string(),
        format!(
            "credential source container: the credentials expire at {} and could not be \
             renewed: credential source container: the container credentials endpoint answered 500",
            iso(T0 + 90)
        )
    );
    assert!(matches!(error, CredentialsError::Expired { .. }));
    assert_eq!(fake.seen().len(), 2);
}

/// Asserts that a fetch that fails with no usable keys kept is remembered: requests within
/// `RETRY_AFTER` get the same error and make no request, and at `RETRY_AFTER` the source is asked
/// again.
#[tokio::test]
async fn a_failed_fetch_with_no_keys_is_remembered_for_the_retry_window() {
    let fake = Fake::start(|_| (500, String::new())).await;
    let uri = format!("{}/creds", fake.url());
    let env = env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri)]);
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    let error = live.keys().await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "credential source container: the container credentials endpoint answered 500"
    );
    assert_eq!(fake.seen().len(), 1);
    for at in [T0, T0 + 5, T0 + RETRY_AFTER.as_secs() - 1] {
        clock.set(at);
        assert_eq!(live.keys().await.err().unwrap(), error);
        assert_eq!(fake.seen().len(), 1);
    }
    clock.set(T0 + RETRY_AFTER.as_secs());
    assert_eq!(live.keys().await.err().unwrap(), error);
    assert_eq!(fake.seen().len(), 2);
}

/// Asserts that a successful fetch after a remembered failure gives its keys, and that a later
/// failure is remembered as its own error, not as the earlier one.
#[tokio::test]
async fn a_successful_fetch_replaces_the_remembered_failure() {
    let fake = Fake::start(|_| (500, String::new())).await;
    let uri = format!("{}/creds", fake.url());
    let env = env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri)]);
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    live.keys().await.err().unwrap();
    let answer = json_credentials("OK", "the-secret", "the-token", T0 + 100);
    fake.reroute(move |_| (200, answer.clone()));
    clock.set(T0 + 10);
    assert_eq!(live.keys().await.unwrap().access_key_id, "OK");
    fake.reroute(|_| (503, String::new()));
    clock.set(T0 + 100);
    let error = live.keys().await.err().unwrap();
    assert!(matches!(error, CredentialsError::Expired { .. }), "{error}");
    assert!(error.to_string().ends_with("answered 503"), "{error}");
    assert_eq!(fake.seen().len(), 3);
}

/// Asserts that when the kept keys can no longer sign and the fetch gives credentials that are
/// themselves within the margin, the error is the fetch's own `Expired` (the expiry of the new
/// credentials, not wrapped in a second `Expired`).
#[tokio::test]
async fn new_credentials_within_the_margin_after_unusable_ones_are_the_fetchs_expired() {
    let (fake, env) = endpoint(T0 + 3600).await;
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    live.keys().await.unwrap();
    let answer = json_credentials("SHORT", "the-secret", "the-token", T0 + 3620);
    fake.reroute(move |_| (200, answer.clone()));
    clock.set(T0 + 3600 - 30);
    let error = live.keys().await.err().unwrap();
    assert_eq!(
        error.to_string(),
        format!(
            "credential source container: the credentials expire at {} and could not be \
             renewed: the source gave credentials with a minute or less left",
            iso(T0 + 3620)
        )
    );
    assert!(matches!(error, CredentialsError::Expired { .. }));
}

/// A container endpoint on loopback that answers its first request at once with the keys `ID1`
/// (expiring at `T0 + 3600`, closing the connection) and holds the second: it tells `accepted`
/// when that request has arrived and answers it with the keys `ID2` once `release` fires.
async fn holding_second_request() -> (
    AwsEnv,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/creds", listener.local_addr().unwrap());
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut head = [0_u8; 4096];
        let answer = |id: &str| {
            let body = json_credentials(id, "the-secret", "the-token", T0 + 3600);
            format!(
                "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = first.read(&mut head).await.unwrap();
        first.write_all(answer("ID1").as_bytes()).await.unwrap();
        drop(first);
        let (mut second, _) = listener.accept().await.unwrap();
        let _ = second.read(&mut head).await.unwrap();
        accepted_tx.send(()).unwrap();
        release_rx.await.unwrap();
        second.write_all(answer("ID2").as_bytes()).await.unwrap();
    });
    (
        env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri)]),
        accepted_rx,
        release_tx,
    )
}

/// Asserts that a request that finds a renewal in flight signs with the kept keys, which may
/// still sign, without waiting for it: with the renewal held at the endpoint the second call
/// returns the kept keys, and once the renewal is released the first call gets the new keys.
#[tokio::test(flavor = "current_thread")]
async fn a_request_during_a_renewal_signs_with_the_kept_keys() {
    let (env, accepted, release) = holding_second_request().await;
    let clock = TestClock::at(T0);
    let live = Arc::new(live(CredentialSource::Container, env, &clock));
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    clock.set(T0 + 3600 - RENEW_BEFORE.as_secs());
    let renewing = tokio::spawn({
        let live = Arc::clone(&live);
        async move { live.keys().await }
    });
    accepted.await.unwrap();
    assert_eq!(live.keys().await.unwrap().access_key_id, "ID1");
    release.send(()).unwrap();
    assert_eq!(renewing.await.unwrap().unwrap().access_key_id, "ID2");
}

/// Asserts that a request that waited for a fetch in flight, with no keys kept, gets the error of
/// that fetch when it fails, and asks the source nothing itself: two requests, one fetch.
#[tokio::test(flavor = "current_thread")]
async fn a_request_that_waited_for_a_failed_fetch_gets_its_error() {
    let fake = Fake::start(|_| (500, String::new())).await;
    let uri = format!("{}/creds", fake.url());
    let env = env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri)]);
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    let (first, second) = futures::future::join(live.keys(), live.keys()).await;
    let why = "credential source container: the container credentials endpoint answered 500";
    assert_eq!(first.err().unwrap().to_string(), why);
    assert_eq!(second.err().unwrap().to_string(), why);
    assert_eq!(fake.seen().len(), 1);
}

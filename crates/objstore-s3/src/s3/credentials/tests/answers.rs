//! Answers of a credential endpoint that the HTTP fake cannot give, from a socket on loopback
//! that writes the bytes of a test: a body cut short, a body that is not UTF-8, and a redirect.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::s3::CredentialsError;

/// The environment of a container endpoint on loopback that answers every connection with
/// `answer` and then closes it.
async fn answering(answer: &[u8]) -> AwsEnv {
    let answer = answer.to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/creds", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head = [0_u8; 4096];
            let _ = stream.read(&mut head).await.unwrap();
            stream.write_all(&answer).await.unwrap();
        }
    });
    env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri)])
}

/// Asserts that an answer whose body ends before its `Content-Length` fails as a read of the
/// answer, with the cause, and no credentials.
#[tokio::test]
async fn a_body_cut_short_fails_the_read_of_the_answer() {
    let env = answering(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"AccessKeyId\"").await;
    let clock = TestClock::at(T0);
    let failed = live(CredentialSource::Container, env, &clock)
        .keys()
        .await
        .err()
        .unwrap();
    let text = failed.to_string();
    assert!(
        text.starts_with("credential source container: reading the answer failed: "),
        "{text}"
    );
    assert!(
        matches!(failed, CredentialsError::Failed { .. }),
        "{failed:?}"
    );
}

/// Asserts that an answer whose body is not UTF-8 is refused as such, without its bytes.
#[tokio::test]
async fn an_answer_that_is_not_utf8_is_refused() {
    let env = answering(b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\n\xff\xfe\xfd").await;
    let clock = TestClock::at(T0);
    let failed = live(CredentialSource::Container, env, &clock)
        .keys()
        .await
        .err()
        .unwrap();
    assert_eq!(
        failed.to_string(),
        "credential source container: the answer is not UTF-8 text"
    );
}

/// Asserts that the credential client follows no redirect: a container endpoint that answers 302
/// with a `Location` naming a second endpoint fails as an answer of 302, and the second endpoint
/// is never asked.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let second = Fake::start(|_| (200, json_credentials("ID", "s", "t", T0 + 3600))).await;
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nlocation: {}/creds\r\ncontent-length: 0\r\n\r\n",
        second.url()
    );
    let env = answering(redirect.as_bytes()).await;
    let clock = TestClock::at(T0);
    let failed = live(CredentialSource::Container, env, &clock)
        .keys()
        .await
        .err()
        .unwrap();
    assert_eq!(
        failed.to_string(),
        "credential source container: the container credentials endpoint answered 302"
    );
    assert!(second.seen().is_empty());
}

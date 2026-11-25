//! Answers of a credential endpoint that the HTTP fake cannot give, from a socket on loopback
//! that writes the bytes of a test: a body cut short, and a body that is not UTF-8.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::s3::CredentialsError;

/// The environment of a container endpoint on loopback that answers every connection with
/// `answer` and then closes it.
async fn answering(answer: &'static [u8]) -> AwsEnv {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/creds", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head = [0_u8; 4096];
            let _ = stream.read(&mut head).await.unwrap();
            stream.write_all(answer).await.unwrap();
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
    assert!(matches!(failed, CredentialsError::Failed { .. }), "{failed:?}");
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

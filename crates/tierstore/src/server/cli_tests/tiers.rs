//! The chunk sizes and depths of the I/O keys, as they reach the tiers the flags open.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use tierstore_testkit::dir::TestDir;

use crate::Backend as _;
use crate::server::cli::{S3Flags, dir_tier, open_tier, s3_backend};
use crate::server::config_tuning::S3Client;

/// Asserts that the chunk sizes of the settings reach a `dir:<path>` tier as `open_tier` makes
/// it: an object of 2500 bytes is read as chunks of 1000, 1000 and 500 bytes by a tier whose read
/// chunk is 1000, and the copy chunk is the one given, and a tier made with the defaults reads it
/// whole.
#[tokio::test]
async fn a_dir_tier_takes_its_chunk_sizes_from_the_settings() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-tier-chunks");
    let path = dir.path().join("tier");
    let path = path.to_str().unwrap();
    let spec = format!("dir:{path}");
    let flags = S3Flags {
        endpoint: None,
        region: None,
        credentials: None,
        conditional_delete: None,
    };
    let body: Vec<u8> = (0..2500_u32).map(|at| (at % 251) as u8).collect();
    let timeout = Duration::from_secs(30);

    let client = S3Client {
        dir_copy_chunk: 7,
        dir_read_chunk: 1000,
        ..S3Client::default()
    };
    let tier = open_tier("--hot", &spec, &flags, timeout, &client).unwrap();
    tier.put("k", Bytes::from(body.clone())).await.unwrap();
    let mut read = tier.get_stream("k", None).await.unwrap();
    let mut sizes = Vec::new();
    while let Some(chunk) = read.next().await {
        sizes.push(chunk.unwrap().len());
    }
    assert_eq!(sizes, [1000, 1000, 500]);
    let opened = dir_tier(path, &client).unwrap();
    assert_eq!(opened.copy_chunk(), 7);

    let plain = open_tier("--hot", &spec, &flags, timeout, &S3Client::default()).unwrap();
    let mut read = plain.get_stream("k", None).await.unwrap();
    let mut sizes = Vec::new();
    while let Some(chunk) = read.next().await {
        sizes.push(chunk.unwrap().len());
    }
    assert_eq!(sizes, [2500]);
}

/// Asserts that the depth of the settings reaches the backend an S3 tier opens as: a depth of 3
/// is held by the backend, and the default depth by one made with the default settings.
#[test]
fn an_s3_tier_takes_its_chunks_waiting_from_the_settings() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-tier-waiting");
    let credentials = dir.path().join("credentials");
    std::fs::write(&credentials, "test-key\ntest-secret\n").unwrap();
    std::fs::set_permissions(&credentials, Permissions::from_mode(0o600)).unwrap();
    let flags = S3Flags {
        endpoint: Some("http://127.0.0.1:9"),
        region: None,
        credentials: Some(credentials.as_path()),
        conditional_delete: None,
    };
    let timeout = Duration::from_secs(30);
    let deep = S3Client {
        chunks_waiting: 3,
        ..S3Client::default()
    };
    let built = s3_backend(
        "--cold",
        "s3:tier-test",
        "tier-test",
        &flags,
        (timeout, &deep),
    );
    assert_eq!(built.unwrap().chunks_waiting(), 3);
    let plain = S3Client::default();
    let built = s3_backend(
        "--cold",
        "s3:tier-test",
        "tier-test",
        &flags,
        (timeout, &plain),
    );
    assert_eq!(built.unwrap().chunks_waiting(), plain.chunks_waiting);
}

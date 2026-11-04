//! The format of a hot tier that is made at the start.

use super::start_format;
use crate::journal::FORMAT_KEY;
use crate::server::cli::Cli;
use crate::server::config_testing::MINIMAL;
use crate::{Backend, Error, FaultyBackend, MemoryBackend, Op};

fn cli(hot: &str) -> Cli {
    let tier = match hot.strip_prefix("dir:") {
        Some(path) => format!("kind = \"dir\"\npath = {path:?}"),
        None => "kind = \"memory\"".to_owned(),
    };
    let memory = "[hot_store]\nkind   = \"memory\"";
    assert!(MINIMAL.contains(memory));
    let text = MINIMAL.replacen(memory, &format!("[hot_store]\n{tier}"), 1);
    Cli::from_config_text(&text).unwrap()
}

/// Asserts that a `memory` hot tier is given the active format at epoch 2, byte for byte as the
/// verb writes it, and that a tier of another kind is left as it is (no format object is written,
/// since its format is the operator's to write).
#[tokio::test]
async fn a_memory_hot_tier_starts_active_and_another_kind_is_left_alone() {
    let memory = MemoryBackend::new();
    start_format(&cli("memory"), &memory).await.unwrap();
    let body = memory.get(FORMAT_KEY, None).await.unwrap();
    assert_eq!(&body[..], br#"{"phase":"active","epoch":2}"#);

    let other = MemoryBackend::new();
    start_format(&cli("dir:/var/lib/tierstore/hot"), &other)
        .await
        .unwrap();
    assert_eq!(
        other.head(FORMAT_KEY).await,
        Err(Error::NotFound(FORMAT_KEY.to_owned()))
    );
}

/// Asserts that a failed write of the format is an error that names the tier, so that a start
/// over a memory tier that cannot take it stops.
#[tokio::test]
async fn a_failed_write_of_the_format_stops_the_start() {
    let hot = FaultyBackend::new(std::sync::Arc::new(MemoryBackend::new()));
    hot.fail_next_with(Op::Put, Error::Unavailable("down".to_owned()));
    let why = start_format(&cli("memory"), &hot).await.unwrap_err();
    assert!(
        why.to_string().contains("memory hot tier") && why.to_string().contains("down"),
        "{why}"
    );
}

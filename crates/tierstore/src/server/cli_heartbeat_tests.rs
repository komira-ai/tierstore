//! The flags of the heartbeat: their defaults, the one check of the start, and where they go.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use super::cli::{Cli, build};
use super::cli_heartbeat::HeartbeatFlags;
use super::config::TierTable;
use crate::{ProxyConfig, ReconcilerConfig, SystemClock};

const SECOND: Duration = Duration::from_secs(1);

/// The flags a start needs, and `more`.
fn cli(more: &[&str]) -> Cli {
    let mut flags = vec![
        "tierstore",
        "--listen=127.0.0.1:0",
        "--proxy-id=p1",
        "--hot=memory",
        "--cold=memory",
        "--hot-bytes=1000",
        "--bucket=tier-bucket",
        "--access-key-file=key",
    ];
    flags.extend_from_slice(more);
    Cli::try_parse_from(flags).unwrap()
}

/// What a start says of flags that break the check, or `None` if it takes them.
fn refused(more: &[&str]) -> Option<String> {
    build(&cli(more)).err().map(|why| why.to_string())
}

/// Asserts the defaults (a beat every 10 seconds, departure after 60, a margin of 10 seconds and a
/// registration retention of 10 minutes), and that the keys of `[hot_store]` left out have the same
/// defaults as the flags left out.
#[test]
fn the_flags_have_their_defaults_and_the_keys_have_the_same() {
    let flags = cli(&[]).heartbeat;
    assert_eq!(flags.heartbeat, 10 * SECOND);
    assert_eq!(flags.departure, 60 * SECOND);
    assert_eq!(flags.deadline_margin, 10 * SECOND);
    assert_eq!(flags.registration_retention, 600 * SECOND);
    assert_eq!(HeartbeatFlags::from_table(&TierTable::default()), flags);
}

/// Asserts that the flags are carried to the settings of the proxy (heartbeat, departure and margin)
/// and of the reconciler (the registration retention), and written as keys that read back.
#[test]
fn the_flags_reach_the_settings_of_the_proxy_and_the_reconciler_and_the_keys() {
    let flags = cli(&[
        "--heartbeat=5s",
        "--departure=45s",
        "--deadline-margin=8s",
        "--registration-retention=20m",
    ])
    .heartbeat;
    let clock = Arc::new(SystemClock);
    let mut proxy = ProxyConfig::new("p1", 1000, clock.clone());
    let mut reconciler = ReconcilerConfig::new("p1", 1000, clock);
    flags.apply(&mut proxy, &mut reconciler);
    assert_eq!(proxy.heartbeat, 5 * SECOND);
    assert_eq!(proxy.departure, 45 * SECOND);
    assert_eq!(proxy.deadline_margin, 8 * SECOND);
    assert_eq!(reconciler.registration_retention, 1200 * SECOND);

    let mut table = TierTable::default();
    flags.put_in(&mut table);
    assert_eq!(HeartbeatFlags::from_table(&table), flags);
}

/// Asserts the one check of the start, by the text it prints: a beat or a margin of zero, a
/// departure under six beats, a margin over a quarter of the departure, and a retention under the
/// departure are each refused with the values named; the value on each boundary is taken.
#[test]
fn a_start_refuses_values_under_which_a_departed_owner_could_be_taken_for_alive() {
    let cases: [(&[&str], &str); 5] = [
        (&["--heartbeat=0s"], "--heartbeat must be longer than zero"),
        (
            &["--deadline-margin=0s"],
            "--deadline-margin must be longer than zero",
        ),
        (
            &["--departure=59s"],
            "--departure (59s) must be at least 6 times --heartbeat (10s)",
        ),
        (
            &["--deadline-margin=16s"],
            "--deadline-margin (16s) must be at most a quarter of --departure (60s)",
        ),
        (
            &["--registration-retention=59s"],
            "--registration-retention (59s) must be at least --departure (60s)",
        ),
    ];
    for (flags, text) in cases {
        assert_eq!(refused(flags).as_deref(), Some(text), "{flags:?}");
    }
    let boundaries: [&[&str]; 5] = [
        &[],
        &["--departure=60s", "--heartbeat=10s"],
        &["--deadline-margin=15s"],
        &["--registration-retention=60s"],
        &["--registration-retention=0s"],
    ];
    let rules = [
        "must be longer than zero",
        "must be at least",
        "must be at most",
    ];
    for flags in boundaries {
        let why = refused(flags).unwrap_or_default();
        assert!(
            !rules.iter().any(|rule| why.contains(rule)),
            "{flags:?}: {why}"
        );
    }
}

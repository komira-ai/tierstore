//! The settings of the heartbeat: their defaults, the one check of the start, and where they go.

use std::sync::Arc;
use std::time::Duration;

use super::cli::{Cli, build};
use super::cli_heartbeat::HeartbeatFlags;
use super::config::TierTable;
use super::config_testing::MINIMAL;
use crate::{ProxyConfig, ReconcilerConfig, SystemClock};

const SECOND: Duration = Duration::from_secs(1);

/// The settings of a file that is `MINIMAL` and gives `more` (lines of `key = "value"`) in
/// `[hot_store]`.
fn cli(more: &[&str]) -> Cli {
    let hot = "[hot_store]\nkind   = \"memory\"";
    assert!(MINIMAL.contains(hot));
    let text = MINIMAL.replacen(hot, &format!("{hot}\n{}", more.join("\n")), 1);
    Cli::from_config_text(&text).unwrap()
}

/// What a start says of flags that break the check, or `None` if it takes them.
fn refused(more: &[&str]) -> Option<String> {
    build(&cli(more)).err().map(|why| why.to_string())
}

/// Asserts the defaults (a beat every 10 seconds, departure after 60, a margin of 10 seconds and a
/// registration retention of zero, which removes none), and that the keys of `[hot_store]` left out have those defaults.
#[test]
fn the_keys_left_out_have_their_defaults() {
    let flags = cli(&[]).heartbeat;
    assert_eq!(flags.heartbeat, 10 * SECOND);
    assert_eq!(flags.departure, 60 * SECOND);
    assert_eq!(flags.deadline_margin, 10 * SECOND);
    assert_eq!(flags.registration_retention, Duration::ZERO);
    assert_eq!(HeartbeatFlags::from_table(&TierTable::default()), flags);
}

/// Asserts that the keys are carried to the settings of the proxy (heartbeat, departure and margin)
/// and of the reconciler (the registration retention),
#[test]
fn the_keys_reach_the_settings_of_the_proxy_and_the_reconciler() {
    let flags = cli(&[
        "heartbeat = \"5s\"",
        "departure = \"45s\"",
        "deadline_margin = \"8s\"",
        "registration_retention = \"20m\"",
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
}

/// Asserts the one check of the start, by the text it prints: a beat or a margin of zero, a
/// departure under six beats, a margin over a quarter of the departure, and a retention under the
/// departure are each refused with the values named; the value on each boundary is taken.
#[test]
fn a_start_refuses_values_under_which_a_departed_owner_could_be_taken_for_alive() {
    let cases: [(&[&str], &str); 5] = [
        (
            &["heartbeat = \"0s\""],
            "--heartbeat must be longer than zero",
        ),
        (
            &["deadline_margin = \"0s\""],
            "--deadline-margin must be longer than zero",
        ),
        (
            &["departure = \"59s\""],
            "--departure (59s) must be at least 6 times --heartbeat (10s)",
        ),
        (
            &["deadline_margin = \"16s\""],
            "--deadline-margin (16s) must be at most a quarter of --departure (60s)",
        ),
        (
            &["registration_retention = \"59s\""],
            "--registration-retention (59s) must be at least --departure (60s)",
        ),
    ];
    for (flags, text) in cases {
        assert_eq!(refused(flags).as_deref(), Some(text), "{flags:?}");
    }
    let boundaries: [&[&str]; 5] = [
        &[],
        &["departure = \"60s\"", "heartbeat = \"10s\""],
        &["deadline_margin = \"15s\""],
        &["registration_retention = \"60s\""],
        &["registration_retention = \"0s\""],
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

//! Two small helpers of the start of a node: [`probe_run_id`], the id of a start for the probe of
//! the cache directory's file system, and [`parse_duration`], the parser of a duration flag. Moved
//! here from `server/cli.rs` unchanged, which re-exports them, so that the file stays under the
//! limit of AGENTS.md R12.

use std::time::Duration;

/// The id of a start for the probe of the cache directory's file system: the proxy id (with any character a
/// key cannot carry replaced by `_`), the time of the start and the process id, so that two
/// starts never share a probe object.
pub(super) fn probe_run_id(proxy_id: &str, now: u64, process: u32) -> String {
    let safe = |c: char| {
        if c.is_ascii_alphanumeric() || c == '-' {
            c
        } else {
            '_'
        }
    };
    let proxy: String = proxy_id.chars().map(safe).collect();
    format!("{proxy}-{now}-{process}")
}

/// A duration: a whole number of `ms`, `s`, `m` or `h`, such as `250ms` or `15m`.
pub(crate) fn parse_duration(text: &str) -> Result<Duration, String> {
    let bad = || format!("{text:?} is not a duration; use a number and ms, s, m or h, as in 15m");
    let unit_at = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (digits, unit) = text.split_at(unit_at);
    let count: u64 = digits.parse().map_err(|_| bad())?;
    let millis = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err(bad()),
    };
    count
        .checked_mul(millis)
        .map(Duration::from_millis)
        .ok_or_else(bad)
}

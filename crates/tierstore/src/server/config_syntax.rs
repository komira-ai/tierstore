//! The refusals of a configuration file that read its text: a syntax fault that does not repeat
//! the line or the value, and a key that looks like a secret.

use super::config::{ConfigError, KEYS};

/// Whether a key's name says it holds a secret: it has a word of one, or is a key itself and
/// not the path of a file. A hint for a message that says what to do, not a gate: what keeps a
/// secret out of the file is this check of every key's name, known or not (an unknown key is
/// only warned about, so it is not what refuses one), and the name of a known key that
/// takes a path (`credentials_file`) cannot tell a path from a secret by its value. The messages
/// of the file never repeat a value, so a secret that is given anyway is not echoed.
fn looks_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if key.ends_with("_file") || key.ends_with("_files") {
        return false;
    }
    let words = [
        "secret",
        "password",
        "passphrase",
        "token",
        "credentials",
        "apikey",
        "psk",
    ];
    words.iter().any(|word| key.contains(word))
        || key == "key"
        || key.starts_with("access_key")
        || key.ends_with("_key")
}

/// The keys of `[local_cache]` that are in no flag, and so not in [`KEYS`].
const LOCAL_CACHE_KEYS: &[&str] = &[
    "path",
    "layout",
    "max_concurrent_fills",
    "max_concurrent_keeps",
    "keep_wait",
    "keep_deadline",
    "write_through",
    "whole_fill_limit",
    "pin_limit",
    "scrub_rate",
    "scrub_min_round",
    "pass_interval",
    "lock_interval",
    "scrub_tick",
    "listing_calls",
    "forget_limit",
    "fill_chunk",
    "keep_queue_bytes",
    "revalidate_after",
    "head_before_put",
    "fill_socket",
    "fill_socket_concurrency",
    "fill_socket_wait",
    "prefetch_concurrency",
    "prefetch_queue",
    "prefetch_queue_ttl",
];

/// Whether `key` is a name some table of the file holds: the last part of a key in [`KEYS`], a
/// `[local_cache]` key, a `[[policy]]` key or a key of an `[[interface]]` (its `params` are in
/// [`KEYS`] by their last part).
fn known_key(key: &str) -> bool {
    matches!(
        key,
        "prefix" | "cache" | "type" | "params" | "port" | "addresses"
    ) || LOCAL_CACHE_KEYS.contains(&key)
        || KEYS
            .iter()
            .flat_map(|(_, keys)| keys.iter())
            .any(|name| name.rsplit('.').next() == Some(key))
}

/// The fault `error` found in `text`, as a [`ConfigError::Syntax`] that does not repeat the line
/// or the value (the display of the `toml` error does: it shows the line, and its messages for a
/// value of the wrong type show the value).
pub(super) fn syntax(text: &str, error: &toml::de::Error) -> ConfigError {
    let at = error.span().map_or(0, |span| span.start).min(text.len());
    let before = &text.as_bytes()[..at];
    let line = before.iter().filter(|byte| **byte == b'\n').count() + 1;
    let column = before
        .iter()
        .rev()
        .take_while(|byte| **byte != b'\n')
        .count()
        + 1;
    let (key, value) = match text.lines().nth(line - 1).and_then(|l| l.split_once('=')) {
        Some((key, value)) => (Some(key.trim()), value.trim().trim_matches('"')),
        None => (None, ""),
    };
    // Only a name the file can hold is shown as the key: inside a multi-line string the text before
    // an `=` is part of a value, which may be a pasted secret.
    let key = key.filter(|key| known_key(key));
    let message = error.message();
    let shows_value = [
        "invalid type",
        "invalid value",
        "invalid length",
        "unknown variant",
    ];
    let what = if shows_value.iter().any(|start| message.starts_with(start)) {
        let expected = message
            .rsplit_once(", expected ")
            .map_or("another value", |(_, expected)| expected);
        let key = key.unwrap_or("a value");
        format!("{key}: not a value this key takes (it is not shown); expected {expected}")
    } else {
        let message = if value.is_empty() {
            message.to_owned()
        } else {
            message.replace(value, "(value)")
        };
        // A value's own parse error (a size, a duration, an address) does not name its key.
        match key {
            Some(key) => format!("{key}: {message}"),
            None => message,
        }
    };
    ConfigError::Syntax { line, column, what }
}

/// The one key whose name holds the word `secret` and whose value is a number of bytes, not a
/// secret.
const MIN_SECRET_KEY: &str = "server.min_secret_bytes";

/// Refuses the first key of `table` (and of the tables inside it) that looks like a secret.
pub(super) fn find_secret(path: &str, table: &toml::Table) -> Result<(), ConfigError> {
    for (key, value) in table {
        let here = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        if here != MIN_SECRET_KEY && looks_secret(key) {
            return Err(ConfigError::InlineSecret { key: here });
        }
        match value {
            toml::Value::Table(inner) => find_secret(&here, inner)?,
            toml::Value::Array(items) => {
                for inner in items.iter().filter_map(toml::Value::as_table) {
                    find_secret(&here, inner)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

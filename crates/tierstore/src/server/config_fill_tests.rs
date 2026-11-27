//! The keys the fill socket had in `[local_cache]`: `fill_socket`, read and ignored with its line
//! on stderr whatever its value, the two keys that size the pool of the file system's fills, and
//! a storage node with a cache, which loads a file that sets the keys of the removed prefetch queue.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::Level;
use tracing::subscriber::with_default;

use super::super::build_info::stderr_log::LineLog;
use super::super::config::{PerRun, Settings, settings_from_text};
use super::super::config_cache::CacheSettings;
use super::super::config_testing::DEFAULTS;
use super::super::config_unknown::unknown_key_warnings;
use super::super::local_cache::{FILL_SOCKET_CONCURRENCY, FILL_SOCKET_WAIT};
use super::IGNORED_FILL_SOCKET;

/// A build node's file with `more` lines in `[local_cache]` and `server` in `[server]`.
fn file(server: &str, more: &str) -> String {
    format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"
{server}

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = 1000
{more}

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#
    )
}

/// The run of a service whose state directory is `/var/lib/tierstore`.
fn serviced() -> PerRun {
    PerRun {
        state_dir: Some(PathBuf::from("/var/lib/tierstore")),
        ..DEFAULTS
    }
}

fn cache_of(text: &str, run: PerRun) -> CacheSettings {
    settings_from_text(text, run).unwrap().into_cache().unwrap()
}

fn refusal(text: &str, run: PerRun) -> String {
    settings_from_text(text, run).unwrap_err().to_string()
}

/// A writer whose bytes the test reads back: the operator's stderr.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<Vec<u8>>>);

impl Stderr {
    fn text(&self) -> String {
        let bytes = self.0.lock().expect("the buffer is not poisoned").clone();
        String::from_utf8(bytes).expect("the lines are text")
    }
}

impl Write for Stderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut held = self.0.lock().expect("the buffer is not poisoned");
        held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The settings of `text` for `run`, and what their load said on stderr.
fn loaded(text: &str, run: PerRun) -> (Settings, String) {
    let stderr = Stderr::default();
    let settings = with_default(LineLog::new(stderr.clone(), Level::INFO), || {
        settings_from_text(text, run)
    });
    (settings.expect("the file loads"), stderr.text())
}

/// Asserts that a build node's file with `local_cache.fill_socket` loads, whatever the path (one
/// a release that served the socket took, a relative one, one longer than a Unix socket's
/// address, the hand-off socket's, one inside the cache directory: each refused by such a
/// release), and says once on stderr, word for word, that the key is ignored; that the settings
/// are those of the same file without the key; and that the file without it says nothing.
#[test]
fn a_file_with_local_cache_fill_socket_starts_and_says_it_is_ignored() {
    assert_eq!(
        IGNORED_FILL_SOCKET,
        "tierstore: local_cache.fill_socket is ignored: no fill socket is served, the file \
         system of an fs interface calls the cache in this process"
    );
    let handoff = "handoff_socket = \"/var/lib/tierstore/handoff.sock\"";
    let long = format!("/run/{}", "s".repeat(120));
    let (plain, said) = loaded(&file(handoff, ""), serviced());
    assert_eq!(said, "");
    let plain = plain.into_cache().unwrap();
    for path in [
        "/run/tierstore/fill.sock",
        "fill.sock",
        long.as_str(),
        "/var/lib/tierstore/handoff.sock",
        "/var/lib/tierstore-cache/fill.sock",
    ] {
        let text = file(handoff, &format!("fill_socket = \"{path}\""));
        let (settings, said) = loaded(&text, serviced());
        assert_eq!(said, format!("{IGNORED_FILL_SOCKET}\n"), "{path}");
        let settings = settings.into_cache().unwrap();
        let (got, wanted) = (
            format!("{:?}", settings.cache),
            format!("{:?}", plain.cache),
        );
        assert_eq!(got, wanted, "{path}");
        assert_eq!(settings.handoff, plain.handoff, "{path}");
    }
}

/// Asserts the pool's keys: their defaults are the constants, a value reaches the cache, and a
/// value out of range is refused naming the key and the range, the ends being taken.
#[test]
fn the_pool_of_the_socket_has_defaults_and_ranges() {
    let cache = cache_of(&file("", ""), DEFAULTS).cache;
    assert_eq!(cache.fill_socket_concurrency, FILL_SOCKET_CONCURRENCY);
    assert_eq!(cache.fill_socket_wait, FILL_SOCKET_WAIT);
    assert_eq!(
        (FILL_SOCKET_CONCURRENCY, FILL_SOCKET_WAIT),
        (16, Duration::from_secs(60))
    );
    let given = "fill_socket_concurrency = 3\nfill_socket_wait = \"7s\"";
    let cache = cache_of(&file("", given), DEFAULTS).cache;
    assert_eq!(cache.fill_socket_concurrency, 3);
    assert_eq!(cache.fill_socket_wait, Duration::from_secs(7));
    for (value, bad) in [(0, "0"), (1025, "1025")] {
        let why = refusal(
            &file("", &format!("fill_socket_concurrency = {value}")),
            DEFAULTS,
        );
        assert_eq!(
            why,
            format!("local_cache.fill_socket_concurrency: {bad} is not between 1 and 1024")
        );
    }
    for value in [1, 1024] {
        let cache = cache_of(
            &file("", &format!("fill_socket_concurrency = {value}")),
            DEFAULTS,
        );
        assert_eq!(cache.cache.fill_socket_concurrency, value);
    }
    let why = refusal(&file("", "fill_socket_wait = \"0s\""), DEFAULTS);
    assert_eq!(
        why,
        "local_cache.fill_socket_wait: 0ms is not between 1ms and 3600000ms"
    );
}

/// Asserts that a storage node with a cache in front of its proxy, whose file sets the eight keys
/// of the removed prefetch queue (an earlier release refused them there), loads: the node is
/// made, and the keys are warned about by name, in the order of the names, and ignored; and that
/// its `local_cache.fill_socket` is read and ignored with its line on stderr, as a build node's is.
#[test]
fn a_storage_node_with_a_cache_loads_a_file_that_sets_the_removed_queue_keys() {
    let node = r#"
[[interface]]
type   = "s3"
params = { addresses = ["192.0.2.10:9400"] }

[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "/etc/tierstore/access.key"

[hot_store]
kind   = "dir"
path   = "/srv/hot"
budget = "500GiB"

[cold_store]
kind = "dir"
path = "/srv/cold"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"
KEY

[[policy]]
prefix = "cas/b/"
cache  = true
"#;
    let lines = [
        "prefetch_concurrency = 1",
        "prefetch_queue = 10",
        "prefetch_queue_ttl = \"5s\"",
        "hydrate_max_trees = 3",
        "hydrate_max_blobs = 3",
        "hydrate_max_bytes = \"2MiB\"",
        "hold_max = \"1h\"",
        "hold_max_count = 4",
    ];
    let given = node.replace("KEY", &lines.join("\n"));
    let settings = settings_from_text(&given, serviced()).unwrap();
    assert!(settings.into_combined().is_some());
    let table: toml::Table = toml::from_str(&given).unwrap();
    assert_eq!(
        unknown_key_warnings(&table),
        [
            "tierstore: config key local_cache.hold_max is not known to this release and is ignored",
            "tierstore: config key local_cache.hold_max_count is not known to this release and is ignored",
            "tierstore: config key local_cache.hydrate_max_blobs is not known to this release and is ignored",
            "tierstore: config key local_cache.hydrate_max_bytes is not known to this release and is ignored",
            "tierstore: config key local_cache.hydrate_max_trees is not known to this release and is ignored",
            "tierstore: config key local_cache.prefetch_concurrency is not known to this release and is ignored",
            "tierstore: config key local_cache.prefetch_queue is not known to this release and is ignored",
            "tierstore: config key local_cache.prefetch_queue_ttl is not known to this release and is ignored",
        ]
    );
    let plain = node.replace("KEY", "");
    let (settings, said) = loaded(&plain, serviced());
    assert_eq!(said, "");
    assert!(settings.into_combined().is_some());
    let named = node.replace("KEY", "fill_socket = \"/var/lib/tierstore/fill.sock\"");
    let (settings, said) = loaded(&named, serviced());
    assert_eq!(said, format!("{IGNORED_FILL_SOCKET}\n"));
    assert!(settings.into_combined().is_some());
}

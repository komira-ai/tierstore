//! The keys of the file that tune the data path and that no other module of the loader owns: the
//! S3 client's connect timeout, body rate, idle-connection timeout and read retries, which
//! `[server]` has for every tier it reaches (as it has `s3_request_timeout`), and the counts,
//! rounds and sizes of the proxy and the reconciler that `[hot_store]` has.
//!
//! Each has the value that was a constant of the code before it was a key as its default, and a
//! range that a value outside is refused with, naming the key.

use std::num::NonZeroU64;
use std::time::Duration;

use crate::reconciler::COMPARE_RANGE;
use crate::s3::S3Tuning;

use super::config::{ConfigError, ServerTable, TierTable, duration, invalid, within};
use super::config_units::{Dur, Size};

/// The settings of the client of an S3 tier that `[server]` gives, the same for every tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct S3Client {
    /// `server.s3_connect_timeout`: how long a connection to an S3 tier may take to be opened.
    pub connect_timeout: Duration,
    /// `server.s3_min_bytes_per_sec`: the slowest rate, in bytes a second, at which the body of a
    /// write is taken for granted to go, which a write's time limit is stretched by; none turns
    /// the stretch off.
    pub min_body_rate: Option<NonZeroU64>,
    /// `server.s3_pool_idle_timeout`, `server.s3_read_retries` and `server.s3_read_retry_wait`.
    pub tuning: S3Tuning,
}

impl Default for S3Client {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            min_body_rate: NonZeroU64::new(1 << 20),
            tuning: S3Tuning::default(),
        }
    }
}

/// The settings of the proxy and the reconciler that `[hot_store]` gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotTuning {
    /// `hot_store.journal_batch_entries`: the most entries one journal object holds.
    pub journal_batch_entries: usize,
    /// `hot_store.usage_refresh`: how long the usage object that was read is used before it is
    /// read again.
    pub usage_refresh: Duration,
    /// `hot_store.journal_page_size`: the most journal objects asked for in one listing.
    pub journal_page_size: usize,
    /// `hot_store.copy_rounds`: how many times a copy reads the cold object again after it
    /// changed under the copy.
    pub copy_rounds: u32,
    /// `hot_store.compare_range`: the most bytes of an object one read of a comparison holds.
    pub compare_range: NonZeroU64,
    /// `hot_store.delete_rounds`: how many times a delete looks at the cold object again after it
    /// changed under the delete.
    pub delete_rounds: u32,
    /// `hot_store.listing_heads`: how many cold objects of a listing are asked about at once.
    pub listing_heads: usize,
    /// `hot_store.upload_listing_calls`: the most calls to the cold tier one listing of uploads
    /// makes.
    pub upload_listing_calls: u32,
}

impl Default for HotTuning {
    fn default() -> Self {
        Self {
            journal_batch_entries: 64,
            usage_refresh: Duration::from_secs(1),
            journal_page_size: 100,
            copy_rounds: 3,
            compare_range: COMPARE_RANGE,
            delete_rounds: 3,
            listing_heads: 16,
            upload_listing_calls: 100,
        }
    }
}

/// The duration of `table.key`, or `default`, from `low` to `high`, both included.
fn span(
    place: (&str, &str),
    value: Option<Dur>,
    default: Duration,
    limits: (Duration, Duration),
) -> Result<Duration, ConfigError> {
    let got = duration(value, default);
    let (low, high) = limits;
    if got < low || got > high {
        let why = format!(
            "{}ms is not between {}ms and {}ms",
            got.as_millis(),
            low.as_millis(),
            high.as_millis()
        );
        return Err(invalid(&format!("{}.{}", place.0, place.1), why));
    }
    Ok(got)
}

/// The count of `hot_store.key`, or `default`, from 1 to `high`.
fn count(key: &str, value: Option<u32>, default: u32, high: u32) -> Result<u32, ConfigError> {
    within("hot_store", key, value.unwrap_or(default), 1, high)
}

const MS: Duration = Duration::from_millis(1);
const HOUR: Duration = Duration::from_secs(3600);

/// The client settings `[server]` gives, or the defaults of those it leaves out.
pub(super) fn s3_client(server: &ServerTable) -> Result<S3Client, ConfigError> {
    let old = S3Client::default();
    let connect_timeout = span(
        ("server", "s3_connect_timeout"),
        server.s3_connect_timeout,
        old.connect_timeout,
        (MS, Duration::from_secs(600)),
    )?;
    let min_body_rate = match server.s3_min_bytes_per_sec {
        Some(rate) => NonZeroU64::new(rate),
        None => old.min_body_rate,
    };
    let pool_idle_timeout = span(
        ("server", "s3_pool_idle_timeout"),
        server.s3_pool_idle_timeout,
        old.tuning.pool_idle_timeout,
        (MS, HOUR),
    )?;
    let read_retries = within(
        "server",
        "s3_read_retries",
        server.s3_read_retries.unwrap_or(old.tuning.read_retries),
        0,
        8,
    )?;
    let read_retry_wait = span(
        ("server", "s3_read_retry_wait"),
        server.s3_read_retry_wait,
        old.tuning.read_retry_wait,
        (MS, Duration::from_secs(10)),
    )?;
    Ok(S3Client {
        connect_timeout,
        min_body_rate,
        tuning: S3Tuning {
            pool_idle_timeout,
            read_retries,
            read_retry_wait,
        },
    })
}

/// The proxy and reconciler settings `[hot_store]` gives, or the defaults of those it leaves out.
pub(super) fn hot_tuning(hot: &TierTable) -> Result<HotTuning, ConfigError> {
    let old = HotTuning::default();
    let batch = hot
        .journal_batch_entries
        .unwrap_or(old.journal_batch_entries as u32);
    let page = hot
        .journal_page_size
        .unwrap_or(old.journal_page_size as u32);
    let heads = hot.listing_heads.unwrap_or(old.listing_heads as u32);
    Ok(HotTuning {
        journal_batch_entries: within("hot_store", "journal_batch_entries", batch, 1, 65_536)?
            as usize,
        usage_refresh: span(
            ("hot_store", "usage_refresh"),
            hot.usage_refresh,
            old.usage_refresh,
            (MS, HOUR),
        )?,
        journal_page_size: within("hot_store", "journal_page_size", page, 1, 1000)? as usize,
        copy_rounds: count("copy_rounds", hot.copy_rounds, old.copy_rounds, 100)?,
        compare_range: NonZeroU64::new(within(
            "hot_store",
            "compare_range",
            hot.compare_range
                .map_or(old.compare_range.get(), |size: Size| size.0),
            4096,
            1 << 30,
        )?)
        .unwrap_or(old.compare_range),
        delete_rounds: count("delete_rounds", hot.delete_rounds, old.delete_rounds, 100)?,
        listing_heads: within("hot_store", "listing_heads", heads, 1, 1024)? as usize,
        upload_listing_calls: count(
            "upload_listing_calls",
            hot.upload_listing_calls,
            old.upload_listing_calls,
            100_000,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use tierstore_testkit::dir::TestDir;

    use super::super::cli::{Built, Cli, build};
    use super::super::config::{from_text, settings_from_text};
    use super::super::config_testing::{DEFAULTS, MINIMAL};
    use super::{HotTuning, S3Client};

    const BUILD_NODE: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:0"] }

[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"
s3_read_retries = 5

[local_cache]
path   = "/c"
budget = 1000

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#;

    /// `MINIMAL` with `line` as one more key of `[server]`.
    fn with_server(line: &str) -> String {
        MINIMAL.replacen("\n[hot_store]", &format!("{line}\n[hot_store]"), 1)
    }

    /// `MINIMAL` with `line` as one more key of `[hot_store]`.
    fn with_hot(line: &str) -> String {
        MINIMAL.replacen("budget = 1000", &format!("budget = 1000\n{line}"), 1)
    }

    /// What `build` makes of `cli` with a good key file as its full key.
    fn built(cli: &mut Cli) -> Built {
        let dir = TestDir::under(std::env::temp_dir(), "tierstore-tuning");
        let path = dir.path().join("key");
        std::fs::write(&path, "AKID\nsecretsecretsecretsecret\n").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        cli.access_key_file = path;
        build(cli).unwrap()
    }

    fn refusal(text: &str) -> String {
        from_text(text, DEFAULTS).unwrap_err().to_string()
    }

    /// A file that gives none of the keys has the values the code had before they were keys: the
    /// connect timeout, body rate, idle timeout and retries of the S3 client, and the proxy's and
    /// reconciler's counts and sizes, in the file's settings and in what `build` makes of them.
    #[test]
    fn a_file_that_gives_none_of_the_keys_has_the_old_values() {
        let mut cli = from_text(MINIMAL, DEFAULTS).unwrap();
        let client = cli.s3_client;
        assert_eq!(client.connect_timeout, Duration::from_secs(5));
        assert_eq!(client.min_body_rate.map(|rate| rate.get()), Some(1 << 20));
        assert_eq!(client.tuning.pool_idle_timeout, Duration::from_secs(30));
        assert_eq!(client.tuning.read_retries, 2);
        assert_eq!(client.tuning.read_retry_wait, Duration::from_millis(50));
        assert_eq!(client, S3Client::default());
        let hot = cli.hot_tuning;
        assert_eq!(hot.journal_batch_entries, 64);
        assert_eq!(hot.usage_refresh, Duration::from_secs(1));
        assert_eq!(hot.journal_page_size, 100);
        assert_eq!(hot.copy_rounds, 3);
        assert_eq!(hot.compare_range.get(), 1 << 20);
        assert_eq!(hot.delete_rounds, 3);
        assert_eq!(hot.listing_heads, 16);
        assert_eq!(hot.upload_listing_calls, 100);
        assert_eq!(hot, HotTuning::default());
        let built = built(&mut cli);
        assert_eq!(built.proxy.batch_entries, 64);
        assert_eq!(built.proxy.usage_refresh, Duration::from_secs(1));
        assert_eq!(built.proxy.delete_rounds, 3);
        assert_eq!(built.proxy.listing_heads, 16);
        assert_eq!(built.proxy.upload_listing_calls, 100);
        assert_eq!(built.reconciler.page_size, 100);
        assert_eq!(built.reconciler.copy_rounds, 3);
        assert_eq!(built.reconciler.compare_range.get(), 1 << 20);
    }

    /// Every key of `[server]` and `[hot_store]` of this change parses, and a value that is not
    /// the default reaches the settings of the S3 client, the proxy and the reconciler.
    #[test]
    fn every_key_parses_and_reaches_the_settings() {
        let text = with_server(
            "s3_connect_timeout = \"2s\"\ns3_min_bytes_per_sec = 4096\n\
             s3_pool_idle_timeout = \"10s\"\ns3_read_retries = 4\ns3_read_retry_wait = \"7ms\"",
        );
        let text = text.replacen(
            "budget = 1000",
            "budget = 1000\njournal_batch_entries = 8\nusage_refresh = \"250ms\"\n\
             journal_page_size = 20\ncopy_rounds = 5\ncompare_range = \"64KiB\"\n\
             delete_rounds = 6\nlisting_heads = 4\nupload_listing_calls = 7",
            1,
        );
        let mut cli = from_text(&text, DEFAULTS).unwrap();
        let client = cli.s3_client;
        assert_eq!(client.connect_timeout, Duration::from_secs(2));
        assert_eq!(client.min_body_rate.map(|rate| rate.get()), Some(4096));
        assert_eq!(client.tuning.pool_idle_timeout, Duration::from_secs(10));
        assert_eq!(client.tuning.read_retries, 4);
        assert_eq!(client.tuning.read_retry_wait, Duration::from_millis(7));
        let built = built(&mut cli);
        assert_eq!(built.proxy.batch_entries, 8);
        assert_eq!(built.proxy.usage_refresh, Duration::from_millis(250));
        assert_eq!(built.proxy.delete_rounds, 6);
        assert_eq!(built.proxy.listing_heads, 4);
        assert_eq!(built.proxy.upload_listing_calls, 7);
        assert_eq!(built.reconciler.page_size, 20);
        assert_eq!(built.reconciler.copy_rounds, 5);
        assert_eq!(built.reconciler.compare_range.get(), 64 << 10);
    }

    /// `server.s3_min_bytes_per_sec = 0` turns the stretch of a write's time limit off.
    #[test]
    fn a_body_rate_of_zero_turns_the_stretch_off() {
        let cli = from_text(&with_server("s3_min_bytes_per_sec = 0"), DEFAULTS).unwrap();
        assert_eq!(cli.s3_client.min_body_rate, None);
    }

    /// A value outside the range of its key is refused, naming the key, the value and the range.
    #[test]
    fn a_value_out_of_range_is_refused_by_name() {
        let server = [
            (
                "s3_connect_timeout = \"0s\"",
                "server.s3_connect_timeout: 0ms is not between 1ms and 600000ms",
            ),
            (
                "s3_connect_timeout = \"11m\"",
                "server.s3_connect_timeout: 660000ms is not between 1ms and 600000ms",
            ),
            (
                "s3_pool_idle_timeout = \"0s\"",
                "server.s3_pool_idle_timeout: 0ms is not between 1ms and 3600000ms",
            ),
            (
                "s3_pool_idle_timeout = \"2h\"",
                "server.s3_pool_idle_timeout: 7200000ms is not between 1ms and 3600000ms",
            ),
            (
                "s3_read_retries = 9",
                "server.s3_read_retries: 9 is not between 0 and 8",
            ),
            (
                "s3_read_retry_wait = \"0ms\"",
                "server.s3_read_retry_wait: 0ms is not between 1ms and 10000ms",
            ),
            (
                "s3_read_retry_wait = \"11s\"",
                "server.s3_read_retry_wait: 11000ms is not between 1ms and 10000ms",
            ),
        ];
        for (line, want) in server {
            assert_eq!(refusal(&with_server(line)), want);
        }
        let hot = [
            (
                "journal_batch_entries = 0",
                "hot_store.journal_batch_entries: 0 is not between 1 and 65536",
            ),
            (
                "journal_batch_entries = 65537",
                "hot_store.journal_batch_entries: 65537 is not between 1 and 65536",
            ),
            (
                "usage_refresh = \"0s\"",
                "hot_store.usage_refresh: 0ms is not between 1ms and 3600000ms",
            ),
            (
                "usage_refresh = \"2h\"",
                "hot_store.usage_refresh: 7200000ms is not between 1ms and 3600000ms",
            ),
            (
                "journal_page_size = 0",
                "hot_store.journal_page_size: 0 is not between 1 and 1000",
            ),
            (
                "journal_page_size = 1001",
                "hot_store.journal_page_size: 1001 is not between 1 and 1000",
            ),
            (
                "copy_rounds = 0",
                "hot_store.copy_rounds: 0 is not between 1 and 100",
            ),
            (
                "copy_rounds = 101",
                "hot_store.copy_rounds: 101 is not between 1 and 100",
            ),
            (
                "compare_range = 4095",
                "hot_store.compare_range: 4095 is not between 4096 and 1073741824",
            ),
            (
                "compare_range = \"2GiB\"",
                "hot_store.compare_range: 2147483648 is not between 4096 and 1073741824",
            ),
            (
                "delete_rounds = 0",
                "hot_store.delete_rounds: 0 is not between 1 and 100",
            ),
            (
                "listing_heads = 0",
                "hot_store.listing_heads: 0 is not between 1 and 1024",
            ),
            (
                "listing_heads = 1025",
                "hot_store.listing_heads: 1025 is not between 1 and 1024",
            ),
            (
                "upload_listing_calls = 0",
                "hot_store.upload_listing_calls: 0 is not between 1 and 100000",
            ),
            (
                "upload_listing_calls = 100001",
                "hot_store.upload_listing_calls: 100001 is not between 1 and 100000",
            ),
        ];
        for (line, want) in hot {
            assert_eq!(refusal(&with_hot(line)), want);
        }
    }

    /// The edges of the ranges are taken.
    #[test]
    fn the_ends_of_the_ranges_are_taken() {
        let low = with_server(
            "s3_connect_timeout = \"1ms\"\ns3_pool_idle_timeout = \"1ms\"\n\
             s3_read_retries = 0\ns3_read_retry_wait = \"1ms\"",
        );
        let cli = from_text(&low, DEFAULTS).unwrap();
        assert_eq!(cli.s3_client.tuning.read_retries, 0);
        let high = with_server(
            "s3_connect_timeout = \"10m\"\ns3_pool_idle_timeout = \"1h\"\n\
             s3_read_retries = 8\ns3_read_retry_wait = \"10s\"",
        );
        let cli = from_text(&high, DEFAULTS).unwrap();
        assert_eq!(cli.s3_client.tuning.read_retries, 8);
        let edge = with_hot(
            "journal_batch_entries = 65536\nusage_refresh = \"1h\"\njournal_page_size = 1000\n\
             copy_rounds = 100\ncompare_range = \"1GiB\"\ndelete_rounds = 100\n\
             listing_heads = 1024\nupload_listing_calls = 100000",
        );
        assert_eq!(
            from_text(&edge, DEFAULTS)
                .unwrap()
                .hot_tuning
                .compare_range
                .get(),
            1 << 30
        );
        let least = with_hot("compare_range = 4096\nusage_refresh = \"1ms\"");
        assert_eq!(
            from_text(&least, DEFAULTS)
                .unwrap()
                .hot_tuning
                .compare_range
                .get(),
            4096
        );
    }

    /// The keys of `[hot_store]` are refused in `[cold_store]`, as the other keys only the hot tier
    /// has are.
    #[test]
    fn a_hot_key_in_the_cold_store_is_refused() {
        let text = MINIMAL.replacen(
            "[cold_store]\nkind = \"memory\"",
            "[cold_store]\nkind = \"memory\"\nlisting_heads = 4",
            1,
        );
        assert_eq!(
            refusal(&text),
            "cold_store.listing_heads: only [hot_store] has it"
        );
    }

    /// A build node takes the keys of the S3 client from `[server]`, and has the defaults of
    /// those it leaves out.
    #[test]
    fn a_build_node_takes_the_s3_client_keys() {
        let settings = settings_from_text(BUILD_NODE, DEFAULTS).unwrap();
        let cache = settings.into_cache().unwrap();
        assert_eq!(cache.s3_client.tuning.read_retries, 5);
        assert_eq!(cache.s3_client.connect_timeout, Duration::from_secs(5));
    }
}

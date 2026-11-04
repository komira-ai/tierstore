//! Fixtures of the tests of the configuration file, which `config_tests.rs` and
//! `launch_tests.rs` share.

use super::config::{DEFAULT_DRAIN_DEADLINE, DEFAULT_DRAIN_LB_WAIT, PerRun};

/// The knobs of a run that gives none of them.
pub(crate) const DEFAULTS: PerRun = PerRun {
    drain_lb_wait: DEFAULT_DRAIN_LB_WAIT,
    drain_deadline: DEFAULT_DRAIN_DEADLINE,
};

/// A storage node's file: two S3 tiers, every key of the key table given, a value that is not
/// the default wherever the key has a default.
pub(crate) const GOLDEN: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["192.0.2.10:9300"] }

[[interface]]
type   = "oci"
params = { addresses = ["192.0.2.10:5000"], tls_cert_file = "/etc/tierstore/oci.crt", tls_key_file = "/etc/tierstore/oci.key", prefix = "images/", max_connections = 16, connection_lifetime = "30m" }

[server]
proxy_id              = "tierstore-1"
bucket                = "tierstore"
region                = "eu-west-1"
max_request_bytes     = 134217728
access_key_file       = "/etc/tierstore/access.key"
tls_cert_file         = "/etc/tierstore/s3.crt"
tls_key_file          = "/etc/tierstore/s3.key"
read_only_key_files   = ["/etc/tierstore/daemons.key", "/etc/tierstore/other.key"]
presign_key_files     = ["/etc/tierstore/pack-presign.key"]
gc_key_files          = ["/etc/tierstore/pack-gc.key"]
scoped_key_files      = ["/etc/tierstore/scoped.key"]
presign_max_expires   = "10m"
max_concurrent_puts   = 8
put_permit_wait       = "25s"
put_min_bytes_per_sec = 2048
put_rate_grace        = "45s"
body_idle_timeout     = "20s"
min_secret_bytes      = 24
s3_request_timeout    = "2m"
backend_check_wait    = "90s"

[hot_store]
kind               = "s3"
bucket             = "tierstore-hot"
endpoint           = "http://192.0.2.10:9200"
region             = "us-east-1"
credentials_file   = "/etc/tierstore/hot.credentials"
budget             = "500GiB"
high_water         = 85
low_water          = 60
critical           = 97
copy_parallel      = 8
lease_ttl          = "40s"
reconcile_interval = "4s"
lock_file          = "/var/lib/tierstore/instance.lock"
usage_stale_after  = "3m"
journal_linger     = "50ms"
journal_retention  = "12h"
heartbeat          = "5s"
departure          = "45s"
deadline_margin    = "8s"
registration_retention = "20m"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoint         = "http://127.0.0.1:30900"
region           = "us-west-2"
credentials_file = "/etc/tierstore/cold.credentials"
skip_checks      = ["multipart-conditional=store-ignores-if-none-match-on-completion"]
"#;

/// The least a file can say: two tiers that are in memory.
pub(crate) const MINIMAL: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:0"] }

[server]
proxy_id        = "p1"
bucket          = "tier-bucket"
access_key_file = "key"

[hot_store]
kind   = "memory"
budget = 1000

[cold_store]
kind = "memory"
"#;

/// An `[[interface]]` entry of type `oci` that listens at `listen`, with `more` after the address
/// (each param after a comma: `, prefix = "cas/"`), as text to append to a file.
pub(crate) fn oci_interface(listen: &str, more: &str) -> String {
    format!("\n[[interface]]\ntype = \"oci\"\nparams = {{ addresses = [\"{listen}\"]{more} }}\n")
}

//! The arguments and the start-up of the `tierstore` binary.
//!
//! One process runs three things over the same pair of tiers: a [`Proxy`](crate::Proxy) with the given id,
//! the S3 front of that proxy ([`Server`](super::Server)), and a [`Reconciler`](crate::Reconciler), which works only while it
//! holds the lease in the hot tier, so that any number of these processes may run over the same
//! tiers. With `hot_store.lock_file` it works only while the process also holds an exclusive lock on
//! that file (see [`Gate`](super::cli_reconcile::Gate)): a second process over the same node, started beside the first,
//! serves at once and reconciles when the first has exited. The data path is described by one
//! TOML file given with `--config` (see [`config`](super::config) for the format); the only
//! flags are those of one run ([`Args`]). A flag that used to hold a setting of the file is
//! refused with status 2 and the key that replaces it. The process reads no environment
//! variable, and the secret access key is read from a file, not from the command line or the
//! config file. The file is read into the [`Cli`] settings.
//!
//! The tiers are `memory`, `dir:<path>` and `s3:<bucket>`. A memory tier keeps nothing when the
//! process stops, and its objects carry the time zero. A directory tier keeps its objects, and
//! the multipart uploads in progress, in a directory of the local file system (see
//! [`DirBackend`](crate::DirBackend)), so that they outlive a restart; the directory is made if
//! it does not exist. A tier `s3:<bucket>` is a bucket of an S3-compatible store (see
//! [`S3Backend`]), which the keys of the tier say how to reach, in the table of the tier: `hot_store.endpoint` or `cold_store.endpoint` (`http://host[:port]`, required),
//! `hot_store.region` or `cold_store.region` (the signing region, `us-east-1` if not given), and
//! `hot_store.credentials_file` or `cold_store.credentials_file` (required: a file only its owner may
//! read, in the form of `server.access_key_file` below; a secret in the config file would be
//! readable by more users). The two tiers have a table of their own, so they may be two stores. These
//! keys are refused for a tier that is not an S3 bucket, since they would do nothing. A request
//! to an S3 tier is bounded by `server.s3_request_timeout`. The cold tier must do multipart uploads,
//! which all three kinds do, and the proxy passes them to it.
//!
//! The clients of the S3 front sign with one of its access keys. `server.access_key_file` is the full
//! key, which may do everything the front serves; `server.read_only_key_files` is an array of key-file paths, each a key that may only read objects and list the bucket (see the
//! [`server`](super) docs), such as the key of the daemons that read blobs. The id of a key is
//! what a request names it by, so the ids must all differ. The file of every key and credential
//! key holds the id and the secret either as JSON, `{"access_key_id": "...",
//! "secret_access_key": "..."}` (tierstore also refuses any other
//! field and an empty value), or as two lines, the id and then the secret; it is JSON when its
//! first byte that is not white space is `{`. Only its owner may read the file (mode 0600 or
//! 0400), and no message of the process names a secret.
//!
//! An `oci` interface adds a fourth thing: a read-only OCI distribution front (see
//! [`oci`](super::oci)), served from the same proxy, so that podman pulls an image by digest from
//! the tiers. With `params.tls_cert_file` and `params.tls_key_file` it is HTTPS (the certificate must carry an
//! IP subjectAltName for the address and be valid now, and the key file must not be readable by
//! every user; the start checks). With neither it is plain HTTP, at any address; off loopback
//! the start prints one warning line on stderr, and a certificate is advised. One of the two
//! params without the other, and a certificate, key or prefix that cannot work, exit with status
//! 2; a file that cannot be read
//! with status 1. The start line gets ` oci-listen=<address> (http)` or ` (https)`.
//! The start line ends with ` commit=<the commit the binary was built from>`.
//!
//! The `s3` interfaces and the `oci` interfaces may each be given more than once (`port` is one
//! address, `addresses` a list; see [`config`](super::config)): a socket is bound for each
//! address, an IPv6 one for IPv6 only (see
//! [`listen`](super::listen)), and one server and one drain serve them
//! all. An address given twice is refused with status 2, and a start that cannot bind every address
//! is refused with status 1. The start line names every address, separated by commas.
//!
//! On SIGTERM or SIGINT the process drains (see the [`server`](super) docs): `/readyz` says 503,
//! it serves for `--drain-lb-wait`, stops accepting and closes idle connections, waits for the
//! requests in flight, the answers it is sending and the connections it closes for at most
//! `--drain-deadline`, gives the reconciler's lease up and
//! exits with status 0. A start that fails exits with status 1. Arguments that are not valid exit
//! with status 2: those the parser refuses, a flag that went, two keys that have one id, and a
//! config file that is refused.
//!
//! Before it opens the proxy or binds the address, the process asks each tier the questions its
//! guarantees rest on (see [`self_test`](crate::self_test)) under a probe key in `_tiering/selftest/` that is
//! new for each start and deleted afterwards. A tier that answers one wrongly (a create-only put
//! that overwrites, an `If-Match` that is ignored, a range answered with the whole object, a
//! head with no `Last-Modified`, a listing that ignores `start-after`, and on the cold tier a
//! multipart completion that overwrites) is not served over: the process says which tier, which
//! check and what the tier answered, and exits with status 3. A tier that cannot be reached (an
//! error that says it is down, 503, a timeout) is asked again, after pauses from one second doubling
//! to thirty, for `server.backend_check_wait` (five minutes by default; each tier has all of it), and
//! then the start exits with status 1 and `tier unreachable: <tier>: <error>`. A tier that is full starts: the checks that
//! need a write are skipped, with a line that says so. `cold_store.skip_checks` leaves the
//! multipart-completion check out, for a reason that is logged at the start; no other check can
//! be left out.
//!
//! A file of the build-node shape (`[local_cache]` and `[cold_store]` and no `[hot_store]`) is a
//! different start: a read cache in front of one upstream, with none of the above (no proxy, no
//! tiers to test, no reconciler). `cache_cli.rs` has its start and its checks; the exit statuses
//! are the same, and a file system under the cache that fails its check is status 3.

use std::ffi::OsString;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use crate::s3::{Capabilities, Credentials, S3Config};
use crate::{
    Backend, Check, Clock, DirBackend, MemoryBackend, ProxyConfig, ReconcilerConfig, S3Backend,
    SelfTestError, SystemClock,
};

use super::ServerConfig;
use super::build_info::{CAPABILITIES, COMMIT, RELEASE_VERSION, VERSION, capabilities_text};
use super::cache_cli::serve_cache_until_signalled;
use super::cli_heartbeat::HeartbeatFlags;
pub use super::cli_serve::{Addresses, run, run_serving};
use super::combined_cli::serve_combined_until_signalled;
use super::config::{OciFlags, Settings, keys_for_flags};
#[cfg(test)]
use super::front::read_key;
use super::front::{InvalidFlags, S3Tls, refuse, server_config};
use super::keys::read_credentials;
use super::launch::{Launch, Refused, resolve};
use super::listen::joined;
use super::local_cache::LocalTierFailure;
use super::lock::check_lock_path;
use super::oci::{self, OciError, OciLimits, OciMetrics, OciMode, OciPrefix};

/// A start-up error, for the operator.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub(super) const DEFAULT_MAX_REQUEST_BYTES: u64 = 256 << 20;

/// The streamed bodies taken at once: 32 of them hold at most 32 x (16 MiB + a few pieces).
pub(super) const DEFAULT_MAX_CONCURRENT_PUTS: u32 = 32;

/// How long a put that finds all the slots taken waits for one before it is answered 503
/// (`server.put_permit_wait`'s default, `15s`).
pub(super) const DEFAULT_PUT_PERMIT_WAIT: Duration = Duration::from_secs(15);

/// The least average rate of a body, in bytes a second.
pub(super) const DEFAULT_PUT_MIN_BYTES_PER_SEC: u64 = 1024;

/// The signing region of an S3 tier whose region is not given.
pub(super) const DEFAULT_REGION: &str = "us-east-1";

/// How long a connection to an S3 tier may take to be opened.
const S3_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The slowest rate, in bytes a second, at which the body of a write to an S3 tier is taken for
/// granted to go: a write may take its request timeout plus its length at this rate.
const S3_MIN_BODY_RATE: Option<NonZeroU64> = NonZeroU64::new(1 << 20);

/// What `tierstore` is started with: the flags of one run. Everything that describes the data
/// path is in the file `--config` names (see [`config`](super::config)); a flag that used to
/// hold a setting of it is refused with the key that replaces it.
#[derive(Clone, Debug, PartialEq, Parser)]
#[command(name = "tierstore", version = RELEASE_VERSION)]
pub struct Args {
    /// The configuration file: a TOML file that holds the interfaces, the tiers, the key files
    /// and the limits (`docs/configuration.md` lists every key). A start without it is refused
    /// with status 2.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// With `--config`: loads and checks the file as a start does (the file, and the key files it
    /// names) and exits with status 0 and no output, or prints the refusal and exits with status
    /// 2. Nothing is bound, opened or asked of the network. Without `--config` it is a usage
    /// error.
    #[arg(long)]
    pub check_config: bool,
    /// Prints the capabilities of this build, one to a line, and exits with status 0 without
    /// starting anything. It needs no other flag and is refused with any (status 2). A host
    /// updater asks it of a release before it hands over to it.
    // Never read: `main` finds the flag in the raw arguments before they are parsed. The field is
    // here so that `--help` and the docs checks know the flag.
    #[arg(long)]
    pub capabilities: bool,
    /// How long, after the drain began, the server goes on serving everything, so that the
    /// 503 of `/readyz` is seen: a number and `ms`, `s`, `m` or `h`.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub drain_lb_wait: Duration,
    /// How long the drain waits for requests in flight, for the answers being sent, and for the
    /// connections it closes, once the settle is over.
    #[arg(long, default_value = "15m", value_parser = parse_duration)]
    pub drain_deadline: Duration,
}

/// The settings of a storage node, made of its configuration file (see
/// [`Cli::from_config_text`]): the tiers, the proxy, the reconciler, the S3 front and the OCI
/// front.
///
/// It serves one bucket to the clients that sign with its full access key or with one of its
/// read-only keys, writes new objects to the hot tier, copies them to the cold tier in the
/// background, and frees the hot tier when it fills.
#[derive(Clone, Debug, PartialEq)]
pub struct Cli {
    /// The addresses the S3 API, `/healthz`, `/readyz` and `/usagez` are served on: those of the
    /// `s3` interfaces (for instance `0.0.0.0:P` and `[::]:P`, which a firewall
    /// redirect to the port needs): one server and one drain serve
    /// them all. An address that is given twice is refused.
    pub listen: Vec<SocketAddr>,
    /// The id of this proxy, which names its journal in the hot tier and its lease. Keep it
    /// across restarts, and give each process over the same tiers one of its own.
    pub proxy_id: String,
    /// The certificate and key that make the S3 front serve HTTPS (`server.tls_cert_file` and
    /// `server.tls_key_file`).
    pub tls: S3Tls,
    /// The slot port this instance runs as, 1 to 65535: the private port of one of the two slots
    /// of a hand-over, which the slot unit passes as `server.slot %i`. It is the `slot` label of
    /// `tierstore_build_info` on `/metricz`, by which the update hook's probe through the public
    /// port knows which instance answered. It binds nothing (an `s3` interface does); without it the
    /// gauge has no such label.
    pub slot: Option<u16>,
    /// The hot tier: `memory` (its objects are lost when the process stops), `dir:<path>` (a
    /// directory, made if it does not exist) or `s3:<bucket>` (a bucket of an S3-compatible
    /// store, which needs `hot_store.endpoint` and `hot_store.credentials_file`).
    pub hot: String,
    /// The cold tier: `memory`, `dir:<path>` or `s3:<bucket>` (which needs `cold_store.endpoint`
    /// and `cold_store.credentials_file`). Multipart uploads go straight to it, part by part, so it
    /// must do them; all three kinds do.
    pub cold: String,
    /// The endpoint of the hot tier when it is `s3:<bucket>`: `http://host[:port]`, with no
    /// path. TLS is not built yet.
    pub hot_endpoint: Option<String>,
    /// The region in the credential scope of the requests to the hot tier when it is
    /// `s3:<bucket>`; `us-east-1` if not given.
    pub hot_region: Option<String>,
    /// The file that holds the access key of the hot tier when it is `s3:<bucket>`: JSON,
    /// `{"access_key_id": "...", "secret_access_key": "..."}`, or the access key id on the first
    /// line and the secret access key on the second. Only its owner may read it (mode 0600).
    pub hot_credentials_file: Option<PathBuf>,
    /// The endpoint of the cold tier when it is `s3:<bucket>`: `http://host[:port]`, with no
    /// path. TLS is not built yet.
    pub cold_endpoint: Option<String>,
    /// The region in the credential scope of the requests to the cold tier when it is
    /// `s3:<bucket>`; `us-east-1` if not given.
    pub cold_region: Option<String>,
    /// The file that holds the access key of the cold tier when it is `s3:<bucket>`, in the
    /// form of `hot_store.credentials_file`. Only its owner may read it (mode 0600).
    pub cold_credentials_file: Option<PathBuf>,
    /// The limit on a request to an `s3:<bucket>` tier: a number and `ms`, `s`, `m` or `h`. A
    /// request with a small body, or none, may take this long in all; a ranged read may take
    /// longer while bytes keep arriving; a write may take this long plus the time its body
    /// takes at 1 MiB a second; and the part of a multipart upload, which is as slow as the
    /// client that sends it, is cut only if it makes no progress for this long. The completion
    /// of a multipart upload has a small body, so it may take this long in all, and a store may
    /// take minutes to put a large object together: set this longer than the longest.
    pub s3_request_timeout: Duration,
    /// The bytes the hot tier is meant to hold; the water marks are shares of it.
    pub hot_bytes: u64,
    /// The percent of `hot_store.budget` above which the oldest copied objects are evicted from the
    /// hot tier.
    pub high_water: u8,
    /// The percent of `hot_store.budget` down to which the eviction goes on.
    pub low_water: u8,
    /// The percent of `hot_store.budget` above which a read of a cold object does not copy
    /// it back into the hot tier.
    pub critical: u8,
    /// The name of the bucket served.
    pub bucket: String,
    /// The region in the credential scope of a request's signature.
    pub region: String,
    /// The file that holds the full access key, which may do everything the server serves:
    /// JSON, `{"access_key_id": "...", "secret_access_key": "..."}` (both fields and no other),
    /// or the access key id on the first line and the secret access key on the second. Only its
    /// owner may read it (mode 0600).
    pub access_key_file: PathBuf,
    /// A file that holds a read-only access key, in the form of `server.access_key_file`: a key that
    /// may get and head objects, list the bucket and head the bucket, and is refused every
    /// write, delete and multipart call with 403 `AccessDenied`. Give the flag once for each
    /// such key (the daemons' key, say); the ids of all keys must differ, or the start is
    /// refused with status 2. Only its owner may read it (mode 0600).
    pub read_only_key_file: Vec<PathBuf>,
    /// A file that holds a presigning key and its ceiling, JSON of `access_key_id`,
    /// `secret_access_key` and `ceiling` (`prefix`, `max_expires_secs`, `max_object_bytes`; all
    /// required, so a key without a ceiling is refused at start): the key may sign presigned PUT
    /// URLs for one object each under the prefix, and nothing else; a request signed with it in
    /// the header is refused. Give the flag once for each such key; all key ids must differ. Only
    /// its owner may read it (mode 0600).
    pub presign_key_file: Vec<PathBuf>,
    /// A file that holds a GC key, JSON of exactly `access_key_id`, `secret_access_key` and
    /// `prefix` (ending in `/`): a scoped key with a list, a delete and an abort right on the
    /// prefix, that lists (ListObjectsV2 and ListMultipartUploads, each with a prefix inside its
    /// own), heads, deletes and aborts uploads there and does nothing else; it is refused on a
    /// presigned URL. List each such key once; all key ids must differ. Only its owner may read
    /// it (mode 0600).
    pub gc_key_file: Vec<PathBuf>,
    /// A file that holds a scoped key, JSON of exactly `access_key_id`, `secret_access_key`,
    /// `read_prefixes`, `write_prefixes` and `delete_prefixes` (lists of prefixes, each ending in
    /// `/`; all five required, the lists may be empty but not all three): a header-signed key that
    /// gets, heads and lists under its read prefixes, creates objects and multipart uploads under
    /// its write prefixes (create-only: no `If-Match`, no copy; it aborts uploads there) and deletes objects under its
    /// delete prefixes; it also signs presigned GET URLs for objects under its read prefixes. List
    /// each such key once; all key ids must differ. Only its owner may read it (mode 0600).
    pub scoped_key_file: Vec<PathBuf>,
    /// The longest `X-Amz-Expires` that a presigned URL may have, whatever the ceiling of its key
    /// says: a number and `ms`, `s`, `m` or `h`.
    pub presign_max_expires: Duration,
    /// Deprecated and ignored: the read grant was removed (use a presigned GET URL). The key
    /// `server.grant_key_files` is accepted and its files are not read, so that a file that sets it
    /// still starts; stderr says it is ignored.
    pub grant_key_file: Vec<PathBuf>,
    /// Deprecated, and ignored, like `server.grant_key_files`: `server.grant_max_lifetime`.
    pub grant_max_lifetime: Option<Duration>,
    /// How long, after the drain began, the server goes on serving everything, so that the
    /// 503 of `/readyz` is seen: a number and `ms`, `s`, `m` or `h`.
    pub drain_lb_wait: Duration,
    /// How long the drain waits for requests in flight, for the answers being sent, and for the
    /// connections it closes, once the settle is over.
    pub drain_deadline: Duration,
    /// How long a batch of journal entries waits for more before it is written.
    pub journal_linger: Duration,
    /// How long an applied journal object is kept before it is deleted.
    pub journal_retention: Duration,
    /// The most objects the reconciler copies to the cold tier at once.
    pub copy_parallel: u32,
    /// How long the reconciler's lease lasts without a renewal.
    pub lease_ttl: Duration,
    /// How often the reconciler works: shorter than `hot_store.lease_ttl`.
    pub reconcile_interval: Duration,
    /// The heartbeat of the proxy and the registration retention: `hot_store.heartbeat`,
    /// `hot_store.departure`, `hot_store.deadline_margin` and `hot_store.registration_retention`.
    pub heartbeat: HeartbeatFlags,
    /// The lock file of this node: an absolute path on a local file system, in a directory that
    /// nothing removes (not under `/run`), the same for every process of one node. The reconciler
    /// works only while this process holds an exclusive lock on the file; a process that finds
    /// it held serves reads and writes through its own proxy and tries the lock again every
    /// second. Without it the reconciler works as soon as the process starts.
    pub lock_file: Option<PathBuf>,
    /// How old the usage the reconciler publishes may be before `/usagez` says 503 with the
    /// reason (`/readyz` does not look at it): a number and `ms`, `s`, `m` or `h`, longer than
    /// `hot_store.lease_ttl`, so that a reconciler that takes over the lease does not show. A usage that
    /// is older than 30 seconds is ignored by the puts as if the hot tier were not full, so a
    /// reconciler that has stopped would otherwise show nowhere; the puts are not refused. A
    /// process that has run this long without any usage is judged the same way.
    pub usage_stale_after: Duration,
    /// The longest body of a request, in bytes: the longest object, and the longest part of a
    /// multipart upload. An upper bound on the length of what is streamed, not a bound on
    /// memory: the body of a put or of a part is passed on to the tiers as it arrives and is
    /// never held whole. A presigning key with a larger `max_object_bytes` is held to this too: the smaller of the two
    /// applies, so raise this with the key's cap. A body over it is refused with 413 and nothing is stored; a request
    /// that says in its `Content-Length` that its body is longer is refused before any byte of
    /// it is read.
    pub max_request_bytes: u64,
    /// The most put bodies and upload parts taken at once. One more waits for a slot, at most
    /// `server.put_permit_wait`, and is then answered 503 `SlowDown` with `Retry-After: 1`; its body
    /// is not read, while it waits or after. A body in flight holds a few pieces of
    /// at most the size the connection delivers (tens of KiB), and an S3 tier up to 16 MiB
    /// more, so 32 bodies hold about half a GiB at most, with S3 tiers, and a few MiB with
    /// directory tiers.
    pub max_concurrent_puts: u32,
    /// How long a put body or upload part that arrives when `server.max_concurrent_puts` bodies are
    /// already being taken waits for one of them to end: a number and `ms`, `s`, `m` or `h`. If a
    /// slot is free in that time the body is taken; if not, it is answered 503 `SlowDown` with
    /// `Retry-After: 1`. The request's body is not read while it waits, so a slow tier makes
    /// clients queue here, up to this long, instead of retrying at once. 0 answers 503 at once.
    pub put_permit_wait: Duration,
    /// The least average rate, in bytes a second, at which a request body may come in, counted
    /// over the time the server spends waiting for the client to send the next piece, once that
    /// time has passed `server.put_rate_grace`. Time the server spends not waiting for the client (a
    /// tier that is slow to answer or to take the body) is not counted. A slower body is
    /// answered 400 `RequestTimeout`, nothing of it is stored and the upload it began at an S3
    /// tier is aborted. It ends senders that send a byte inside every idle timeout. 0 turns
    /// it off.
    pub put_min_bytes_per_sec: u64,
    /// How much time spent waiting for the client a body is let to go before
    /// `server.put_min_bytes_per_sec` is held to it: a number and `ms`, `s`, `m` or `h`.
    pub put_rate_grace: Duration,
    /// How long a request body may go without a byte arriving, and how long a client may go
    /// without taking a byte of an answer, before the request ends: a number and `ms`, `s`,
    /// `m` or `h`. A body that arrives slowly but steadily is not cut by this timeout, however long
    /// the whole takes (`server.put_min_bytes_per_sec` sets the slowest rate it is let to have). A request whose body stalls is answered 400 `RequestTimeout` and its connection
    /// is closed; a connection whose client stops reading an answer is closed. Both are
    /// logged on stderr, and a drain waits no longer than this for a stalled client.
    pub body_idle_timeout: Duration,
    /// The fewest bytes a secret access key may have, whichever kind of key it is (full,
    /// read-only, presigning, GC or scoped): a server whose key file holds a shorter
    /// secret does not start.
    pub min_secret_bytes: u32,
    /// Leaves the multipart-completion check of the start-up self-test out, for a reason:
    /// `multipart-conditional=REASON`, where REASON says why the store cannot pass it (for
    /// example `multipart-conditional=store-ignores-if-none-match-on-completion`). It is the
    /// only check that can be left out: the others test the guarantees the proxy, the lease and
    /// the journal stand on, and any other name is refused as a usage error. The reason is
    /// written to stderr at every start. Without the check, a multipart upload may replace an
    /// object that exists: leave it out only for a store you have shown to be safe another way.
    /// None is skipped by default.
    pub skip_backend_check: Vec<SkippedCheck>,
    /// How long the start-up self-test asks again of a tier that does not answer (down, a
    /// timeout, a 503 or another 5xx but 501 and 507), with pauses that double from one second
    /// to thirty, before the start fails with status 1 (and the unit starts it again): a number
    /// and `ms`, `s`, `m` or `h`. Each tier has the whole of it. `0s` asks once. A tier that
    /// answers wrongly is not asked again.
    pub backend_check_wait: Duration,
    /// Serves a read-only OCI distribution front at this address, from the same tiers (see the
    /// [`oci`](super::oci) docs): `GET` and `HEAD` of `/v2/`, of `/v2/<name>/manifests/<digest>`
    /// and of `/v2/<name>/blobs/<digest>`, so that podman and other registry clients pull an
    /// image by digest. Anonymous: no token is ever asked for. Objects are read at
    /// `<params.prefix>blobs/sha256/<hex>`. With `params.tls_cert_file` and `params.tls_key_file` it is
    /// HTTPS, and clients dial an IP address, so the certificate must carry an IP subjectAltName
    /// for this address (any IP, if it is unspecified), which the start checks. With neither it
    /// is plain HTTP, served at any address: on loopback (127/8 or `::1`) a node's own engine
    /// pulls from its own store and the traffic never leaves the host; off loopback the start
    /// prints one warning line, since a plain connection can be read and changed on its path,
    /// and the certificate and key are advised. The addresses of every `oci` interface, as
    /// for the `s3` interfaces; the certificate must name each of them. Empty: no such listener is
    /// opened.
    pub oci_listen: Vec<SocketAddr>,
    /// The certificate chain (PEM) of an `oci` interface, leaf first, issued by the CA the clients
    /// trust. Read once at start: a new certificate takes a restart. Given with
    /// `params.tls_key_file` or not at all.
    pub oci_tls_cert: Option<PathBuf>,
    /// The private key (PEM) of `params.tls_cert_file`. A file any user can read (a mode with the
    /// "other" bits set) is refused at start; a group may read it.
    pub oci_tls_key: Option<PathBuf>,
    /// The prefix of the keys the OCI front reads, ending in `/` (`oci/` if not given): an
    /// object is at `<prefix>blobs/sha256/<64 lowercase hex digits>`. A prefix inside
    /// `_journal/` or `_tiering/` is refused at start (exit 2).
    pub oci_prefix: Option<String>,
    /// The most connections the OCI front has open at once (64 if not given); the others wait
    /// in the kernel's backlog. Memory is bounded by connections times the buffer a connection
    /// holds (up to 4 MiB: a blob chunk or a manifest), so the default is about 256 MiB at worst.
    pub oci_max_connections: Option<u32>,
    /// The longest an OCI connection lives (1h if not given): a number and `ms`, `s`, `m` or
    /// `h`. A request being answered when it ends is finished first. The time a client has to
    /// send a request, and the time it may leave an answer unread, are those of the S3 front's
    /// timeouts (a minute, and `server.body_idle_timeout`).
    pub oci_connection_lifetime: Option<Duration>,
}

/// A check of the self-test that the operator leaves out, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedCheck {
    /// The check left out.
    pub check: Check,
    /// The reason given, never empty.
    pub reason: String,
}

/// A skipped check: `multipart-conditional`, `=`, and a reason that is not empty. Any other
/// check is refused.
pub(super) fn parse_skip(text: &str) -> Result<SkippedCheck, String> {
    let names: Vec<&str> = Check::ALL.iter().map(|check| check.name()).collect();
    let usage = format!(
        "{text:?} is not CHECK=REASON; the checks are {}",
        names.join(", ")
    );
    let (name, reason) = text.split_once('=').ok_or_else(|| usage.clone())?;
    let check = Check::parse(name).ok_or(usage)?;
    if check != Check::MultipartConditional {
        return Err(format!(
            "the {name} check cannot be skipped: only multipart-conditional can"
        ));
    }
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(format!(
            "the skipped check {name} needs a reason after the ="
        ));
    }
    Ok(SkippedCheck {
        check,
        reason: reason.to_owned(),
    })
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

/// The share of a budget that a percent is.
fn share(percent: u8) -> f64 {
    f64::from(percent) / 100.0
}

/// What the settings make.
pub(super) struct Built {
    pub(super) server: ServerConfig,
    pub(super) proxy: ProxyConfig,
    pub(super) reconciler: ReconcilerConfig,
    pub(super) hot: Arc<dyn Backend>,
    pub(super) cold: Arc<dyn Backend>,
    pub(super) oci: Option<OciSetup>,
}

/// The OCI front, as far as the flags and files go: the proxy it reads is not open yet.
pub(super) struct OciSetup {
    /// The addresses, which are at least one.
    pub(super) listen: Vec<SocketAddr>,
    /// `None` serves plain HTTP.
    pub(super) acceptor: Option<tokio_rustls::TlsAcceptor>,
    pub(super) prefix: OciPrefix,
    pub(super) metrics: Arc<OciMetrics>,
    pub(super) limits: OciLimits,
}

/// The flags that say how to reach a tier that is an S3 bucket.
pub(super) struct S3Flags<'a> {
    pub(super) endpoint: Option<&'a str>,
    pub(super) region: Option<&'a str>,
    pub(super) credentials: Option<&'a Path>,
}

impl S3Flags<'_> {
    /// Whether any of the flags was given.
    fn given(&self) -> bool {
        let given = [
            self.endpoint.is_some(),
            self.region.is_some(),
            self.credentials.is_some(),
        ];
        given.contains(&true)
    }
}

impl Cli {
    /// The flags of the hot tier that are for an S3 bucket.
    fn hot_s3(&self) -> S3Flags<'_> {
        S3Flags {
            endpoint: self.hot_endpoint.as_deref(),
            region: self.hot_region.as_deref(),
            credentials: self.hot_credentials_file.as_deref(),
        }
    }

    /// The flags of the cold tier that are for an S3 bucket.
    fn cold_s3(&self) -> S3Flags<'_> {
        S3Flags {
            endpoint: self.cold_endpoint.as_deref(),
            region: self.cold_region.as_deref(),
            credentials: self.cold_credentials_file.as_deref(),
        }
    }
}

/// The tier a flag names, with the flags that go with it: a bucket of an S3-compatible store,
/// or one of the kinds that `tier` opens, which have no use for those flags.
pub(super) fn open_tier(
    flag: &str,
    spec: &str,
    s3: &S3Flags<'_>,
    timeout: Duration,
) -> Result<Arc<dyn Backend>, String> {
    match spec.strip_prefix("s3:") {
        Some(bucket) => s3_tier(flag, spec, bucket, s3, timeout),
        None if s3.given() => {
            let why = "its endpoint, region and credentials flags are for an `s3:<bucket>` tier";
            Err(format!("{flag} {spec:?}: {why}"))
        }
        None => tier(flag, spec),
    }
}

/// The bucket `bucket` of the S3-compatible store that `s3` says how to reach, as the tier
/// that `flag` names with `spec`.
fn s3_tier(
    flag: &str,
    spec: &str,
    bucket: &str,
    s3: &S3Flags<'_>,
    timeout: Duration,
) -> Result<Arc<dyn Backend>, String> {
    let fail = |why: String| format!("{flag} {spec:?}: {why}");
    let Some(endpoint) = s3.endpoint else {
        return Err(fail(format!("it needs {flag}-endpoint")));
    };
    let Some(credentials) = s3.credentials else {
        return Err(fail(format!("it needs {flag}-credentials-file")));
    };
    let credentials_flag = format!("{flag}-credentials-file");
    let (key_id, secret) = read_credentials(&credentials_flag, credentials)?;
    let config = S3Config {
        endpoint: endpoint.to_owned(),
        region: s3.region.unwrap_or(DEFAULT_REGION).to_owned(),
        bucket: bucket.to_owned(),
        credentials: Credentials::new(key_id, secret),
        capabilities: Capabilities::default(),
        connect_timeout: S3_CONNECT_TIMEOUT,
        request_timeout: timeout,
        min_body_rate: S3_MIN_BODY_RATE,
    };
    let built = S3Backend::new(config);
    let backend = built.map_err(|why| fail(why.to_string()))?;
    Ok(Arc::new(backend))
}

/// The tier a flag names, of the kinds that need no flags besides the tier's own.
fn tier(flag: &str, spec: &str) -> Result<Arc<dyn Backend>, String> {
    match (spec, spec.strip_prefix("dir:")) {
        ("memory", _) => Ok(Arc::new(MemoryBackend::new())),
        (_, Some(path)) => match DirBackend::open(path) {
            Ok(dir) => Ok(Arc::new(dir)),
            Err(why) => Err(format!("{flag} {spec:?}: {why}")),
        },
        _ => {
            let why = "only `memory`, `dir:<path>` and `s3:<bucket>` are tiers";
            Err(format!("{flag} {spec:?}: {why}"))
        }
    }
}

/// Checks the flags that depend on each other, and makes the settings they stand for.
pub(super) fn build(cli: &Cli) -> Result<Built, Error> {
    if cli.low_water >= cli.high_water {
        return Err("--low-water must be below --high-water".into());
    }
    if cli.high_water > cli.critical {
        return Err("--high-water must not be above --critical".into());
    }
    if cli.reconcile_interval >= cli.lease_ttl {
        return Err("--reconcile-interval must be shorter than --lease-ttl".into());
    }
    if cli.usage_stale_after <= cli.lease_ttl {
        return Err("--usage-stale-after must be longer than --lease-ttl".into());
    }
    if cli.s3_request_timeout.is_zero() {
        return Err("--s3-request-timeout must be longer than zero".into());
    }
    cli.heartbeat.check()?;
    if let Some(path) = &cli.lock_file {
        check_lock_path(path).map_err(|why| format!("--lock-file {path:?} {why}"))?;
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut server = server_config(&cli.front(), Arc::clone(&clock), true)?;
    server.usage_stale_after = cli.usage_stale_after;
    let timeout = cli.s3_request_timeout;
    let hot = open_tier("--hot", &cli.hot, &cli.hot_s3(), timeout)?;
    let cold = open_tier("--cold", &cli.cold, &cli.cold_s3(), timeout)?;
    let mut proxy = ProxyConfig::new(&cli.proxy_id, cli.hot_bytes, Arc::clone(&clock));
    proxy.critical = share(cli.critical);
    proxy.linger = cli.journal_linger;
    let mut reconciler = ReconcilerConfig::new(&cli.proxy_id, cli.hot_bytes, clock);
    reconciler.high_water = share(cli.high_water);
    reconciler.low_water = share(cli.low_water);
    reconciler.parallel = cli.copy_parallel as usize;
    reconciler.lease_ttl = cli.lease_ttl;
    reconciler.retention = cli.journal_retention;
    cli.heartbeat.apply(&mut proxy, &mut reconciler);
    let oci = oci_setup(cli, &server, true)?;
    server.oci_metrics = oci.as_ref().map(|setup| Arc::clone(&setup.metrics));
    Ok(Built {
        server,
        proxy,
        reconciler,
        hot,
        cold,
        oci,
    })
}

/// The default of `params.prefix`.
pub(super) const DEFAULT_OCI_PREFIX: &str = "oci/";

/// The default of `params.max_connections`.
const DEFAULT_OCI_CONNECTIONS: u32 = 64;

/// The default of `params.connection_lifetime`.
const DEFAULT_OCI_LIFETIME: Duration = Duration::from_secs(3600);

/// How long a client has to finish the TLS handshake with the OCI front.
const OCI_HANDSHAKE: Duration = Duration::from_secs(10);

/// Checks the OCI front's flags and files, and makes its TLS, if an `oci` interface is given. A prefix,
/// certificate or key that cannot work is an `InvalidFlags` (exit 2, as a restart cannot mend it);
/// a file that cannot be read is exit 1. `warn` says whether the warning for a plain-text listener
/// is printed; `--check-config` leaves it out, as it prints nothing when the file is fine.
fn oci_setup(cli: &Cli, server: &ServerConfig, warn: bool) -> Result<Option<OciSetup>, Error> {
    let flags = OciFlags {
        listen: cli.oci_listen.clone(),
        tls_cert: cli.oci_tls_cert.clone(),
        tls_key: cli.oci_tls_key.clone(),
        prefix: cli.oci_prefix.clone(),
        max_connections: cli.oci_max_connections,
        connection_lifetime: cli.oci_connection_lifetime,
    };
    oci_setup_of(&flags, server, warn)
}

/// [`oci_setup`] for the values the flags or the `[oci]` table of a file make: the one place the
/// front's rules are applied, for a storage node and for a build node.
pub(super) fn oci_setup_of(
    flags: &OciFlags,
    server: &ServerConfig,
    warn: bool,
) -> Result<Option<OciSetup>, Error> {
    if flags.listen.is_empty() {
        return Ok(None);
    }
    let listen = flags.listen.clone();
    let lifetime = flags.connection_lifetime.unwrap_or(DEFAULT_OCI_LIFETIME);
    if lifetime.is_zero() {
        return Err(
            InvalidFlags("--oci-connection-lifetime must be longer than zero".into()).into(),
        );
    }
    let prefix = flags.prefix.as_deref().unwrap_or(DEFAULT_OCI_PREFIX);
    let prefix = OciPrefix::new(prefix).map_err(refuse)?;
    let acceptor = match (&flags.tls_cert, &flags.tls_key) {
        (Some(cert), Some(key)) => Some(oci::acceptor_for(cert, key, &listen).map_err(refuse)?),
        (None, None) => {
            let warnings = listen.iter().filter_map(|addr| oci::plain_warning(*addr));
            for warning in warnings.filter(|_| warn) {
                tracing::warn!("{warning}");
            }
            None
        }
        _ => {
            let why = "--oci-tls-cert and --oci-tls-key go together: give both, or neither";
            return Err(refuse(OciError::Missing(why.to_owned())));
        }
    };
    let mode = if acceptor.is_some() {
        OciMode::Https
    } else {
        OciMode::Http
    };
    let connections = flags.max_connections.unwrap_or(DEFAULT_OCI_CONNECTIONS);
    let limits = OciLimits {
        max_connections: connections as usize,
        handshake_timeout: OCI_HANDSHAKE,
        header_timeout: server.header_timeout,
        write_idle: server.body_idle_timeout,
        lifetime,
        drain_deadline: server.drain_deadline,
    };
    Ok(Some(OciSetup {
        listen,
        acceptor,
        prefix,
        metrics: Arc::new(OciMetrics::with_mode(mode)),
        limits,
    }))
}

/// Resolves on SIGTERM or SIGINT. The handlers are in place when this returns.
pub(super) fn signals() -> std::io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    })
}

async fn serve_until_signalled(cli: Cli) -> Result<(), Error> {
    let stop = signals()?;
    let announce = |bound: Addresses| {
        let mut line = format!("tierstore {VERSION} listen={}", joined(&bound.s3_all));
        if bound.oci.is_some() {
            let mode = bound.oci_mode.unwrap_or_default().as_str();
            line.push_str(&format!(" oci-listen={} ({mode})", joined(&bound.oci_all)));
        }
        super::build_info::stderr_log::answer(&format!("{line} commit={COMMIT}\n"));
    };
    run_serving(&cli, stop, announce).await?;
    Ok(())
}

fn blocking(start: impl Future<Output = Result<(), Error>>) -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(start)
}

/// `--capabilities`: the capabilities of this build, one to a line, on stdout and status 0 (see
/// [`CAPABILITIES`]); or, when `args` (the program name first) hold anything else, the first of
/// it named on stderr and status 2, with nothing on stdout.
fn capabilities(args: &[OsString]) -> ExitCode {
    match args.iter().skip(1).find(|arg| *arg != "--capabilities") {
        Some(other) => {
            let other = other.to_string_lossy();
            tracing::error!("tierstore: --capabilities takes no other flag, got {other}");
            ExitCode::from(2)
        }
        None => {
            super::build_info::stderr_log::answer(&capabilities_text(CAPABILITIES));
            ExitCode::SUCCESS
        }
    }
}

/// The binary: flags in, exit status out.
#[must_use]
pub fn main(args: Vec<OsString>) -> ExitCode {
    // Before anything can say something: the operator messages go through `tracing`.
    super::build_info::stderr_log::install();
    // The verbs (the host updater's hook, the operator's `format-epoch` and `entries`): each is
    // answered before any flag is parsed.
    if let Some(status) = super::verbs::answer(&args) {
        return status;
    }
    // Before the flags are parsed: it needs none of the flags a start requires.
    if args.iter().skip(1).any(|arg| arg == "--capabilities") {
        return capabilities(&args);
    }
    let launch = match resolve(args) {
        Ok(launch) => launch,
        Err(Refused::Usage(usage)) => {
            let _ = usage.print();
            return ExitCode::from(if usage.use_stderr() { 2 } else { 0 });
        }
        Err(Refused::Config(why)) => {
            tracing::error!("tierstore: {why}");
            return ExitCode::from(2);
        }
    };
    let Launch {
        settings,
        check_only,
    } = launch;
    if check_only {
        return check_config(&settings);
    }
    let started = match settings {
        Settings::Storage(cli) => blocking(serve_until_signalled(*cli)),
        Settings::Cache(cache) => blocking(serve_cache_until_signalled(*cache)),
        Settings::Combined(node) => blocking(serve_combined_until_signalled(*node)),
    };
    match started {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            // The messages of the checks name flags; the operator wrote keys.
            tracing::error!("tierstore: {}", keys_for_flags(&why.to_string()));
            ExitCode::from(exit_status(&why))
        }
    }
}

/// `--check-config`: what a start refuses before it opens anything, asked of the settings of a
/// file: the front's settings, which reads the key files it names, and the limits a start
/// checks first. Status 0 and no output if there is nothing to refuse; else the start's message
/// on stderr and status 2. It binds nothing and asks nothing of a tier. It does not make the
/// checks that a start makes of the disk and the stores: the budget of a cache against the size
/// of its file system (`free_floor`), the cache directory, the credentials of a tier, the
/// self-test.
fn check_config(settings: &Settings) -> ExitCode {
    match checked(settings) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            tracing::error!("tierstore: {}", keys_for_flags(&why.to_string()));
            ExitCode::from(2)
        }
    }
}

/// What `--check-config` finds wrong with the settings, if anything.
pub(super) fn checked(settings: &Settings) -> Result<(), Error> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    match settings {
        Settings::Storage(cli) => {
            let server = server_config(&cli.front(), clock, false)?;
            // The `oci_*` fields: the prefix, the lifetime and the TLS files, as a start checks them.
            oci_setup(cli, &server, false).map(drop)
        }
        Settings::Cache(cache) if cache.s3_request_timeout.is_zero() => {
            Err("--s3-request-timeout must be longer than zero".into())
        }
        Settings::Cache(cache) => {
            let server = server_config(&cache.front, clock, false)?;
            // The `[oci]` table, with the checks a start makes of it.
            oci_setup_of(&cache.oci, &server, false).map(drop)
        }
        Settings::Combined(combined) => {
            let server = server_config(&combined.node.front(), clock, false)?;
            oci_setup(&combined.node, &server, false).map(drop)
        }
    }
}

/// The exit status of a start that failed: 2 for flags that cannot go together, 3 for a tier
/// or a cache directory that answered the self-test wrongly, 1 for anything else (a tier that could not be reached
/// among it: a restart may mend that).
pub(super) fn exit_status(why: &Error) -> u8 {
    if why.downcast_ref::<InvalidFlags>().is_some() {
        return 2;
    }
    if why.downcast_ref::<LocalTierFailure>().is_some() {
        return 3;
    }
    match why.downcast_ref::<SelfTestError>() {
        Some(SelfTestError::Violation(_)) => 3,
        Some(SelfTestError::Unreachable { .. }) | None => 1,
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "oci_cli_tests.rs"]
mod oci_tests;

#[cfg(test)]
#[path = "grant_cli_tests.rs"]
mod grant_tests;

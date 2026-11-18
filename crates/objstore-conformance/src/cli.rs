//! The runner: the suite against an S3 endpoint named by flags, through the `S3Store` client
//! (the reference run of 17.2 item 6, or any S3 service a person wants to check).
//!
//! ```text
//! objstore-conformance --endpoint http://127.0.0.1:8333 --bucket conformance \
//!     --access-key-id <id> --secret-access-key-file <file> \
//!     --prefix runs/<run id>/ --identical refused [--create-bucket]
//! ```
//!
//! It prints the report (one `PASS` or `FAIL` line per case), then one `ETAG` line with the
//! entity tag the store gave an object beside the MD5 of its body (S3 gives that MD5; the storage
//! tier gives the generation, a blake3 hash, 10.9), and exits 0 if every case passed, 1 if one did not, and 2 if it could not
//! run (a flag, the secret file, the client or the bucket). The secret is read from a file, not a
//! flag or the environment, so it is in no process listing.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bytes::Bytes;
use clap::Parser;
use md5::{Digest as _, Md5};
use objstore_s3::s3::{Credentials, S3Config, S3ConfigError, S3Store};
use objstore_s3::{Capabilities, KeyError, KeyPrefix, ObjectStore, ObjectStoreError};

use crate::IdenticalPut;

/// The runner's flags.
#[derive(Debug, Parser)]
#[command(
    name = "objstore-conformance",
    about = "Runs the ObjectStore conformance suite against an S3 endpoint."
)]
pub struct Flags {
    /// `http://host[:port]`, with no path.
    #[arg(long)]
    pub endpoint: String,
    /// The signing region.
    #[arg(long, default_value = "us-east-1")]
    pub region: String,
    /// The bucket.
    #[arg(long)]
    pub bucket: String,
    /// The access key id.
    #[arg(long)]
    pub access_key_id: String,
    /// A file holding the secret access key (a trailing newline is dropped).
    #[arg(long)]
    pub secret_access_key_file: PathBuf,
    /// The root the cases write under, each under `<prefix><case>/`; it should hold nothing.
    #[arg(long)]
    pub prefix: String,
    /// What the store answers to a write of the bytes a key holds.
    #[arg(long, value_enum)]
    pub identical: Identical,
    /// Create the bucket first (an existing bucket is an error).
    #[arg(long)]
    pub create_bucket: bool,
    /// Limit on each request, in seconds.
    #[arg(long, default_value_t = 30)]
    pub timeout_secs: u64,
}

/// [`IdenticalPut`] as a flag value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Identical {
    /// `200`: the storage tier's S3 front (10.9).
    Accepted,
    /// `412`: an S3 bucket with conditional writes.
    Refused,
}

impl From<Identical> for IdenticalPut {
    fn from(value: Identical) -> Self {
        match value {
            Identical::Accepted => IdenticalPut::Accepted,
            Identical::Refused => IdenticalPut::Refused,
        }
    }
}

/// Why the runner could not run the suite.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The secret file could not be read.
    #[error("reading the secret access key from {path:?}: {source}")]
    Secret {
        /// The file.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The prefix is not one.
    #[error("--prefix: {0}")]
    Prefix(KeyError),
    /// The client refused its configuration.
    #[error("the S3 client: {0}")]
    Client(S3ConfigError),
    /// The bucket could not be created.
    #[error("creating the bucket: {0}")]
    Bucket(ObjectStoreError),
    /// The runtime could not be started.
    #[error("starting the runtime: {0}")]
    Runtime(io::Error),
    /// The report could not be written.
    #[error("writing the report: {0}")]
    Output(io::Error),
}

/// Runs the binary on `args` (the program name first); the exit code says how it went.
pub fn main(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let mut err = io::stderr().lock();
    let flags = match Flags::try_parse_from(args) {
        Ok(flags) => flags,
        Err(e) => {
            // `--help` is not an error: clap's exit code says which it is.
            let _ = write!(err, "{}", e.render());
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let answer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(CliError::Runtime)
        .and_then(|runtime| runtime.block_on(run(flags, &mut io::stdout().lock())));
    match answer {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            let _ = writeln!(err, "objstore-conformance: {e}");
            ExitCode::from(2)
        }
    }
}

/// Runs the suite as `flags` say and writes the report and the `ETAG` line to `out`; `true` if
/// every case passed.
///
/// # Errors
/// [`CliError`] when the suite could not run.
pub async fn run(flags: Flags, out: &mut impl Write) -> Result<bool, CliError> {
    let path = flags.secret_access_key_file.clone();
    let secret =
        std::fs::read_to_string(&path).map_err(|source| CliError::Secret { path, source })?;
    let root = KeyPrefix::new(flags.prefix.clone()).map_err(CliError::Prefix)?;
    let create_bucket = flags.create_bucket;
    let identical = flags.identical.into();
    let config = s3_config(flags, &secret);
    let store = S3Store::new(config).map_err(CliError::Client)?;
    if create_bucket {
        store.create_bucket().await.map_err(CliError::Bucket)?;
    }
    let report = crate::run(&store, &root, identical).await;
    write!(out, "{report}").map_err(CliError::Output)?;
    let tag = etag(&store, &root).await;
    writeln!(out, "{tag}").map_err(CliError::Output)?;
    Ok(report.passed())
}

/// The client's configuration from `flags` and the secret read from its file (a trailing newline
/// dropped): conditional writes claimed, Object Lock not, and `--timeout-secs` as the limit on
/// opening a connection and on each request.
fn s3_config(flags: Flags, secret: &str) -> S3Config {
    let timeout = Duration::from_secs(flags.timeout_secs);
    S3Config {
        endpoint: flags.endpoint,
        region: flags.region,
        bucket: flags.bucket,
        credentials: Credentials::new(flags.access_key_id, secret.trim_end_matches(['\r', '\n'])),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: timeout,
        request_timeout: timeout,
        min_body_rate: None,
    }
}

/// The `ETAG` line: writes an object under `<root>etag`, reads its entity tag, deletes it, and
/// says what the tag was beside the MD5 of the body (S3 gives that MD5 for a single PUT), or why
/// it is unknown (the object could not be written, read by HEAD or deleted).
async fn etag(store: &S3Store, root: &KeyPrefix) -> String {
    let looked = async {
        let key = root
            .key("etag")
            .map_err(|e| ObjectStoreError::Protocol(e.to_string()))?;
        let body = Bytes::from_static(b"the entity tag of this body");
        store.put_new(&key, body.clone(), None).await?;
        let head = store.head(&key).await;
        store.delete(&key).await?;
        Ok::<_, ObjectStoreError>((head?.etag, body))
    };
    match looked.await {
        Ok((tag, body)) => {
            let md5 = hex::encode(Md5::digest(&body));
            format!("ETAG {tag} for a body whose MD5 is \"{md5}\"")
        }
        Err(e) => format!("ETAG unknown: {e}"),
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;

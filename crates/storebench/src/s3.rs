//! Reaching silo: the client of one bucket, from an endpoint flag and silo's credentials file.
//!
//! The credentials are read from the file silo's unit reads them from (`MINIO_ROOT_USER=` and
//! `MINIO_ROOT_PASSWORD=` lines, values quoted or not), named by a flag; nothing is read from the
//! environment (AGENTS.md S2) and nothing prints the secret.

use std::path::Path;
use std::time::Duration;

use clap::Args;
use objstore_s3::Capabilities;
use objstore_s3::s3::{Addressing, Credentials, S3Config, S3Store};

use crate::Error;

/// The flags that name the S3 store that is read.
#[derive(Clone, Debug, Args)]
pub struct Endpoint {
    /// The store's S3 endpoint, `http://host:port` or `https://host:port` (no default: it names
    /// the store read).
    #[arg(long)]
    pub endpoint: String,
    /// The file with `MINIO_ROOT_USER=` and `MINIO_ROOT_PASSWORD=` lines.
    #[arg(long)]
    pub credentials: std::path::PathBuf,
    /// The longest a request may wait for its answer, or for the next piece of a body, in
    /// seconds: silo builds a listing of a large bucket before it answers the first page.
    #[arg(long, default_value_t = 600)]
    pub request_timeout_secs: u64,
}

/// The access key pair in silo's credentials file at `path`.
///
/// # Errors
/// [`Error::Io`] when the file cannot be read; [`Error::Parse`] when it lacks either line.
pub fn credentials(path: &Path) -> Result<Credentials, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io("read", path, e))?;
    let value = |name: &str| {
        text.lines().find_map(|line| {
            let rest = line.trim().strip_prefix(name)?.strip_prefix('=')?;
            Some(rest.trim().trim_matches('"').trim_matches('\'').to_owned())
        })
    };
    match (value("MINIO_ROOT_USER"), value("MINIO_ROOT_PASSWORD")) {
        (Some(user), Some(password)) => Ok(Credentials::new(user, password)),
        _ => Err(Error::parse(
            path,
            0,
            "needs a MINIO_ROOT_USER= and a MINIO_ROOT_PASSWORD= line",
        )),
    }
}

/// The client of `bucket` at `endpoint`, signed with `credentials`, whose requests wait at
/// most `timeout` for an answer. Does no I/O.
///
/// # Errors
/// [`Error::Usage`] when the endpoint or the bucket name is refused.
pub fn store(
    endpoint: &str,
    bucket: &str,
    credentials: Credentials,
    timeout: Duration,
) -> Result<S3Store, Error> {
    S3Store::new(S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: bucket.to_owned(),
        addressing: Addressing::Auto,
        ca_pem: None,
        credentials,
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: timeout,
        min_body_rate: None,
    })
    .map_err(|e| Error::Usage(format!("silo at {endpoint}, bucket {bucket}: {e}")))
}

#[cfg(test)]
pub(crate) mod tests {
    use tierstore_testkit::dir::TestDir;

    use super::*;

    /// The runtime of a test that talks to an S3 service on loopback: two worker threads, so
    /// the tests that run at once do not start one thread per CPU each.
    pub(crate) fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Both lines, quoted or not, give the key pair; the secret is never shown.
    #[test]
    fn the_credentials_file_gives_the_key_pair_and_debug_hides_the_secret() {
        let dir = TestDir::under(std::env::temp_dir(), "storebench-creds");
        let path = dir.path().join("cold.secret.env");
        std::fs::write(
            &path,
            "# silo\nMINIO_ROOT_USER=\"admin\"\n MINIO_ROOT_PASSWORD='s3cret'\n",
        )
        .unwrap();
        let got = credentials(&path).unwrap();
        assert_eq!(
            format!("{got:?}"),
            "Credentials { access_key_id: \"admin\", secret_access_key: \"<redacted>\" }"
        );
    }

    /// A file without the password line, and a file that is not there, are errors that name it.
    #[test]
    fn a_credentials_file_without_both_lines_or_absent_is_an_error() {
        let dir = TestDir::under(std::env::temp_dir(), "storebench-creds-bad");
        let path = dir.path().join("cold.secret.env");
        std::fs::write(&path, "MINIO_ROOT_USER=admin\nMINIO_ROOT_PASSWORDX=no\n").unwrap();
        let err = credentials(&path).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "{}:0: needs a MINIO_ROOT_USER= and a MINIO_ROOT_PASSWORD= line",
                path.display()
            )
        );
        let missing = dir.path().join("absent");
        assert!(matches!(
            credentials(&missing),
            Err(Error::Io { what: "read", .. })
        ));
    }

    /// An endpoint that is not a URL is refused before any request.
    #[test]
    fn a_bad_endpoint_is_a_usage_error() {
        let err = store(
            "not a url",
            "cas-store",
            Credentials::new("a", "b"),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err}");
        assert!(
            store(
                "http://127.0.0.1:0",
                "cas-store",
                Credentials::new("a", "b"),
                Duration::from_secs(1)
            )
            .is_ok()
        );
    }
}

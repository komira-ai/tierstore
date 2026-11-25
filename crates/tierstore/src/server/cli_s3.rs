//! A tier that is a bucket of an S3-compatible store: the flags that say how to reach it, and
//! the backend they make. Its credentials are the key of its credentials file, or come from a
//! source of AWS credentials (`credential_source`, see `config_tier`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::Backend;
use crate::S3Backend;
use crate::s3::{Addressing, Capabilities, Credentials, S3Config};

use super::super::config_tier::SourceSetting;
use super::super::config_tuning::S3Client;
use super::super::keys::read_credentials;
use super::DEFAULT_REGION;

/// The flags that say how to reach a tier that is an S3 bucket.
pub(in crate::server) struct S3Flags<'a> {
    pub(in crate::server) endpoint: Option<&'a str>,
    /// The endpoints after `endpoint`, if the tier has several.
    pub(in crate::server) more_endpoints: &'a [String],
    pub(in crate::server) region: Option<&'a str>,
    pub(in crate::server) credentials: Option<&'a Path>,
    /// The source of the tier's credentials when they are not the key of `credentials`.
    pub(in crate::server) source: Option<&'a SourceSetting>,
    /// The tier's `conditional_delete` setting; `None` is the default of its kind.
    pub(in crate::server) conditional_delete: Option<bool>,
}

impl S3Flags<'_> {
    /// Whether any of the flags was given.
    pub(super) fn given(&self) -> bool {
        let given = [
            self.endpoint.is_some(),
            self.region.is_some(),
            self.credentials.is_some(),
            self.source.is_some(),
        ];
        given.contains(&true)
    }
}

/// The bucket `bucket` of the S3-compatible store that `s3` says how to reach, as the tier
/// that `flag` names with `spec`.
pub(super) fn s3_tier(
    flag: &str,
    spec: &str,
    bucket: &str,
    s3: &S3Flags<'_>,
    limits: (Duration, &S3Client),
) -> Result<Arc<dyn Backend>, String> {
    let backend = s3_backend(flag, spec, bucket, s3, limits)?;
    Ok(Arc::new(backend))
}

/// The backend of [`s3_tier`], before it is shared.
pub(in crate::server) fn s3_backend(
    flag: &str,
    spec: &str,
    bucket: &str,
    s3: &S3Flags<'_>,
    limits: (Duration, &S3Client),
) -> Result<S3Backend, String> {
    let (timeout, client) = limits;
    let fail = |why: String| format!("{flag} {spec:?}: {why}");
    let Some(endpoint) = s3.endpoint else {
        return Err(fail(format!("it needs {flag}-endpoint")));
    };
    let credentials = match (s3.source, s3.credentials) {
        (None, None) => return Err(fail(format!("it needs {flag}-credentials-file"))),
        (None, Some(path)) => {
            let (key_id, secret) = read_credentials(&format!("{flag}-credentials-file"), path)?;
            Credentials::new(key_id, secret)
        }
        (Some(setting), None) => setting.credentials(),
        (Some(setting), Some(_)) => {
            let source = setting.source;
            let why =
                format!("{flag}-credentials-file is not for {flag}-credential-source {source}");
            return Err(fail(why));
        }
    };
    let config = S3Config {
        endpoint: endpoint.to_owned(),
        region: s3.region.unwrap_or(DEFAULT_REGION).to_owned(),
        bucket: bucket.to_owned(),
        addressing: Addressing::Auto,
        ca_pem: None,
        credentials,
        capabilities: Capabilities::default(),
        connect_timeout: client.connect_timeout,
        request_timeout: timeout,
        min_body_rate: client.min_body_rate,
    };
    let built = S3Backend::new_rotating(config, client.tuning, s3.more_endpoints);
    let backend = built.map_err(|why| fail(why.to_string()))?;
    // Off unless set: a read of the tag before the DELETE is right on every store. The store is
    // not asked whether it enforces `If-Match` on a DELETE; the setting says.
    let backend = backend
        .with_conditional_delete(s3.conditional_delete.unwrap_or(false))
        .with_chunks_waiting(client.chunks_waiting);
    Ok(backend)
}

//! The credential sources against fakes on loopback ([`fake::Fake`]) for STS, the container
//! endpoint and the instance metadata service, with the environment given by each test
//! ([`AwsEnv::from_vars`]), the shared files in a scratch directory of its own, and the clock of
//! the renewals a test's ([`TestClock`]): no test reads the host's environment or files, or waits.

mod container;
mod fake;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::answer::Http;
use super::live::{Clock, Live};
use super::{AwsEnv, CredentialSource, Credentials};
use crate::s3::client_builder;
use crate::s3::endpoint::tls_config;
use crate::s3::sigv4::Utc;

pub(crate) use fake::{Fake, Seen};

/// A time of the tests, in seconds since the epoch: 2027-01-15T08:00:00Z.
pub(crate) const T0: u64 = 1_800_000_000;

/// A clock a test sets, in whole seconds since the epoch.
#[derive(Clone)]
pub(crate) struct TestClock(Arc<AtomicU64>);

impl TestClock {
    /// A clock that reads `secs`.
    pub(crate) fn at(secs: u64) -> Self {
        Self(Arc::new(AtomicU64::new(secs)))
    }

    /// Sets the clock to `secs`.
    pub(crate) fn set(&self, secs: u64) {
        self.0.store(secs, Ordering::SeqCst);
    }

    /// The clock, for [`Live::new`].
    pub(crate) fn clock(&self) -> Clock {
        let secs = Arc::clone(&self.0);
        Arc::new(move || UNIX_EPOCH + Duration::from_secs(secs.load(Ordering::SeqCst)))
    }
}

/// The client the credential endpoints are asked through: plain HTTP to loopback.
pub(crate) fn http() -> Http {
    let tls = tls_config(None, Vec::new()).unwrap();
    let client = client_builder(Duration::from_secs(5), Duration::from_secs(5), tls)
        .no_proxy()
        .build()
        .unwrap();
    Http {
        client,
        timeout: Duration::from_secs(30),
    }
}

/// The environment of `vars` alone.
pub(crate) fn env(vars: &[(&str, &str)]) -> AwsEnv {
    AwsEnv::from_vars(
        vars.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    )
}

/// The live credentials of `source` in `env`, renewed by `clock`, for a store of the region
/// `us-east-1`.
pub(crate) fn live(source: CredentialSource, env: AwsEnv, clock: &TestClock) -> Live {
    let credentials = Credentials::from_source(source, env);
    let made: Result<Live, ()> = Live::new(&credentials, || Ok(http()), "us-east-1", clock.clock());
    made.unwrap()
}

/// `YYYY-MM-DDTHH:MM:SSZ` of `secs` since the epoch.
pub(crate) fn iso(secs: u64) -> String {
    Utc::of(UNIX_EPOCH + Duration::from_secs(secs), false).iso8601()
}

/// The JSON of temporary credentials that the container endpoint and the instance metadata
/// service answer, expiring at `expires` seconds since the epoch.
pub(crate) fn json_credentials(id: &str, secret: &str, token: &str, expires: u64) -> String {
    format!(
        r#"{{"Code": "Success", "LastUpdated": "{}", "Type": "AWS-HMAC", "AccessKeyId": "{id}", "SecretAccessKey": "{secret}", "Token": "{token}", "Expiration": "{}"}}"#,
        iso(T0),
        iso(expires)
    )
}

/// The answer of STS `AssumeRoleWithWebIdentity` with these credentials.
pub(crate) fn sts_answer(id: &str, secret: &str, token: &str, expires: u64) -> String {
    format!(
        "<AssumeRoleWithWebIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">\
         <AssumeRoleWithWebIdentityResult><Credentials>\
         <AccessKeyId>{id}</AccessKeyId><SecretAccessKey>{secret}</SecretAccessKey>\
         <SessionToken>{token}</SessionToken><Expiration>{}</Expiration>\
         </Credentials></AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>",
        iso(expires)
    )
}

/// A time `secs` after the epoch.
pub(crate) fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

//! The credentials requests are signed with: a fixed key pair ([`Credentials::new`],
//! [`Credentials::session`]), or a source of AWS credentials found the way the AWS SDKs find them
//! ([`Credentials::from_source`]).
//!
//! The sources, in the order [`CredentialSource::Auto`] tries them (the AWS SDKs' order):
//!
//! 1. [`CredentialSource::Environment`]: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
//!    `AWS_SESSION_TOKEN` (`env.rs`).
//! 2. [`CredentialSource::Profile`]: the profile `AWS_PROFILE` names (or `default`) of the shared
//!    credentials file (`AWS_SHARED_CREDENTIALS_FILE`, or `~/.aws/credentials`) and the shared
//!    config file (`AWS_CONFIG_FILE`, or `~/.aws/config`): static keys, or a role taken with a
//!    web identity token (`role_arn` and `web_identity_token_file`) (`profile.rs`).
//! 3. [`CredentialSource::WebIdentity`]: the token of `AWS_WEB_IDENTITY_TOKEN_FILE` exchanged for
//!    the credentials of the role `AWS_ROLE_ARN` with STS `AssumeRoleWithWebIdentity`, as EKS
//!    (IRSA) sets it up (`sts.rs`).
//! 4. [`CredentialSource::Container`]: the endpoint of `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`
//!    (ECS) or `AWS_CONTAINER_CREDENTIALS_FULL_URI` (EKS Pod Identity), with the authorization
//!    token of `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE` or `AWS_CONTAINER_AUTHORIZATION_TOKEN`
//!    (`container.rs`).
//! 5. [`CredentialSource::InstanceMetadata`]: the role of the EC2 instance profile, from the
//!    instance metadata service with an IMDSv2 session token (`imds.rs`).
//!
//! A source that is not set up (none of its variables is set, no file has the profile) is passed
//! over by `Auto`; one that is set up and fails stops it with that source's error, so a broken
//! setup is never answered with another identity. The source `Auto` settles on is the one every
//! later refresh asks.
//!
//! Temporary credentials (all but static keys) are fetched when a request is first signed and
//! kept until 5 minutes before they expire (half their lifetime, for credentials that last under
//! 10 minutes); the next request then fetches new ones. A fetch that fails while the kept ones
//! have more than a minute left signs with those and logs a warning; a request is never signed with
//! credentials that have a minute or less left: it fails with [`CredentialsError::Expired`]
//! instead (`live.rs`).
//!
//! No secret is logged or put in an error: not a secret key, a session token, a web identity
//! token or an authorization token, and not an answer's body, which may hold one.

mod answer;
mod container;
mod env;
mod imds;
mod live;
mod profile;
mod sts;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

pub(crate) use answer::Http;
pub use env::AwsEnv;
pub(crate) use live::{Keys, Live, system_clock};

/// The credentials requests are signed with. `Debug` never shows a secret.
#[derive(Clone)]
pub struct Credentials {
    kind: Kind,
}

/// What [`Credentials`] holds.
#[derive(Clone)]
enum Kind {
    /// Keys that never change.
    Fixed(Arc<Keys>),
    /// A source of credentials, and the environment it is found in.
    Source(CredentialSource, Arc<AwsEnv>),
}

impl Credentials {
    /// A key pair.
    #[must_use]
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self::fixed(access_key_id.into(), secret_access_key.into(), None)
    }

    /// A key pair of temporary credentials with its session token, which is sent with each
    /// request (`x-amz-security-token`, signed). Nothing renews them: a store signs with them
    /// until they are refused.
    #[must_use]
    pub fn session(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: impl Into<String>,
    ) -> Self {
        let token = Some(session_token.into());
        Self::fixed(access_key_id.into(), secret_access_key.into(), token)
    }

    /// The credentials `source` gives, found in `env`. Nothing is read or asked here: the files
    /// and endpoints of the source are read when a request is first signed, and again each time
    /// temporary credentials are renewed.
    #[must_use]
    pub fn from_source(source: CredentialSource, env: AwsEnv) -> Self {
        Self {
            kind: Kind::Source(source, Arc::new(env)),
        }
    }

    fn fixed(access_key_id: String, secret_access_key: String, token: Option<String>) -> Self {
        let keys = Keys {
            access_key_id,
            secret_access_key,
            session_token: token,
        };
        Self {
            kind: Kind::Fixed(Arc::new(keys)),
        }
    }
}

impl Credentials {
    /// Whether the credentials come from a source, and are fetched and renewed.
    pub(crate) fn renews(&self) -> bool {
        matches!(self.kind, Kind::Source(..))
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = f.debug_struct("Credentials");
        match &self.kind {
            Kind::Fixed(keys) => {
                out.field("access_key_id", &keys.access_key_id)
                    .field("secret_access_key", &"<redacted>");
                if keys.session_token.is_some() {
                    out.field("session_token", &"<redacted>");
                }
            }
            Kind::Source(source, _) => {
                out.field("source", source);
            }
        }
        out.finish()
    }
}

/// Where credentials come from (see the module docs for what each reads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    /// The first of the others, in their order here, that is set up.
    Auto,
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
    Environment,
    /// A profile of the shared config and credentials files.
    Profile,
    /// A web identity token exchanged with STS for a role's credentials.
    WebIdentity,
    /// The credentials endpoint of an ECS task or an EKS pod.
    Container,
    /// The role of the EC2 instance profile, from the instance metadata service (IMDSv2).
    InstanceMetadata,
}

impl fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Environment => "environment",
            Self::Profile => "profile",
            Self::WebIdentity => "web-identity",
            Self::Container => "container",
            Self::InstanceMetadata => "instance-metadata",
        })
    }
}

/// Why no credentials could be had to sign a request with. The text never holds a secret.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CredentialsError {
    /// The source was chosen by name and is not set up: what it needs is not there.
    #[error("credential source {from}: {why}")]
    NotSet {
        /// The source.
        from: CredentialSource,
        /// What is missing.
        why: String,
    },
    /// [`CredentialSource::Auto`] found no source set up; the text says, for each, why not.
    #[error("no credential source is set up: {0}")]
    NoSource(String),
    /// The source is set up and failed: a file could not be read, an endpoint could not be
    /// reached or refused, or answered what are not credentials.
    #[error("credential source {from}: {why}")]
    Failed {
        /// The source.
        from: CredentialSource,
        /// What failed.
        why: String,
    },
    /// The profile asks for a way of getting credentials this crate does not have.
    #[error(
        "credential source profile: profile {profile:?} uses {feature}, which is not supported"
    )]
    Unsupported {
        /// The profile.
        profile: String,
        /// The setting it uses (`source_profile`, `credential_process`, ...).
        feature: &'static str,
    },
    /// The credentials kept have a minute or less left, and new ones could not be had.
    #[error(
        "credential source {from}: the credentials expire at {at} and could not be renewed: {why}"
    )]
    Expired {
        /// The source.
        from: CredentialSource,
        /// When they expire, `YYYY-MM-DDTHH:MM:SSZ`.
        at: String,
        /// Why the renewal failed.
        why: String,
    },
}

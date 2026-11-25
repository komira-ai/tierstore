//! What a tier table of the configuration file says about its tier: the spec of `--hot` or
//! `--cold`, the S3 keys, the `conditional_delete` key and the marks bucket, checked against the
//! kind of the tier.
//!
//! The credentials of an `s3` tier are the key of `credentials_file` (`credential_source =
//! "file"`, the default), or come from a source of AWS credentials, found as the AWS SDKs find
//! them (`objstore_s3::s3::Credentials::from_source`): `auto` (the first of the others that is
//! set up), `environment`, `profile`, `web-identity`, `container` or `instance-metadata`. A source
//! takes no `credentials_file`, and `credential_profile` (the profile of the shared AWS files, in
//! place of the one `AWS_PROFILE` names) is only for `profile` and `auto`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::config::{ConfigError, Kind, TierTable, invalid, refuse, require};
use crate::s3::{AwsEnv, CredentialSource, Credentials};

/// The values of `credential_source` (and of the verbs' `--hot-credential-source`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SourceName {
    /// The key of `credentials_file`.
    File,
    /// The first of the sources below that is set up, in this order.
    Auto,
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
    Environment,
    /// A profile of the shared AWS config and credentials files.
    Profile,
    /// `AWS_WEB_IDENTITY_TOKEN_FILE` and `AWS_ROLE_ARN`, exchanged with STS.
    WebIdentity,
    /// The credentials endpoint of an ECS task or an EKS pod.
    Container,
    /// The EC2 instance profile, from the instance metadata service (IMDSv2).
    InstanceMetadata,
}

impl SourceName {
    /// The source, or `None` for [`SourceName::File`].
    pub(crate) fn source(self) -> Option<CredentialSource> {
        Some(match self {
            Self::File => return None,
            Self::Auto => CredentialSource::Auto,
            Self::Environment => CredentialSource::Environment,
            Self::Profile => CredentialSource::Profile,
            Self::WebIdentity => CredentialSource::WebIdentity,
            Self::Container => CredentialSource::Container,
            Self::InstanceMetadata => CredentialSource::InstanceMetadata,
        })
    }
}

/// The credentials of an `s3` tier that come from a source of AWS credentials, not a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSetting {
    pub source: CredentialSource,
    /// The profile in place of the one `AWS_PROFILE` names; only with `profile` and `auto`.
    pub profile: Option<String>,
}

impl SourceSetting {
    /// The setting of `name` with the profile `profile`, if `name` is a source: `None` for
    /// `file`.
    ///
    /// # Errors
    /// The profile, for a source that reads no profile; the text says why, for the key or flag
    /// the caller names.
    pub(crate) fn of(name: SourceName, profile: Option<&str>) -> Result<Option<Self>, String> {
        let source = name.source();
        let reads_profile = matches!(
            source,
            Some(CredentialSource::Profile | CredentialSource::Auto)
        );
        if profile.is_some() && !reads_profile {
            return Err("only for the credential sources \"profile\" and \"auto\"".to_owned());
        }
        Ok(source.map(|source| Self {
            source,
            profile: profile.map(str::to_owned),
        }))
    }

    /// The credentials of the source, found in the provider-standard environment of this
    /// process (the `AWS_*` variables and `HOME`).
    pub(crate) fn credentials(&self) -> Credentials {
        self.credentials_in(AwsEnv::from_process())
    }

    /// The credentials of the source, found in `env`, with the profile of the setting.
    fn credentials_in(&self, env: AwsEnv) -> Credentials {
        let env = match &self.profile {
            Some(profile) => env.with_profile(profile),
            None => env,
        };
        Credentials::from_source(self.source, env)
    }
}

/// What a tier table says about the tier: the spec of `--hot` or `--cold`, and the endpoint,
/// region and credentials file of its S3 flags.
#[derive(Debug)]
pub(super) struct TierParts {
    pub spec: String,
    /// The spec of the tier of the markers (`docs/design/put-without-lock.md`, 4.1): the same
    /// forms as `spec`. An `s3` tier keeps them in `<bucket>-marks` unless `marks_bucket` names
    /// another bucket of the store, a `dir` tier in the directory of that name beside its own,
    /// and a `memory` tier in a map of its own.
    pub marks: String,
    /// The endpoint, or the first of `endpoints`.
    pub endpoint: Option<String>,
    /// The rest of `endpoints`, in order; each request of the tier goes to the next of
    /// `endpoint` and these in turn.
    pub more_endpoints: Vec<String>,
    pub region: Option<String>,
    pub credentials_file: Option<PathBuf>,
    /// The credentials of an `s3` tier with a `credential_source` other than `file`.
    pub credential_source: Option<SourceSetting>,
    /// The `conditional_delete` key; `None` is the default of the kind.
    pub conditional_delete: Option<bool>,
}

/// The tier `table` names, if its keys fit its kind.
pub(super) fn tier(name: &str, table: &TierTable) -> Result<TierParts, ConfigError> {
    let s3_only = [
        ("endpoint", table.endpoint.is_some()),
        ("endpoints", table.endpoints.is_some()),
        ("region", table.region.is_some()),
        ("credentials_file", table.credentials_file.is_some()),
        ("credential_source", table.credential_source.is_some()),
        ("credential_profile", table.credential_profile.is_some()),
    ];
    let bucket = ("bucket", table.bucket.is_some());
    let path = ("path", table.path.is_some());
    let delete_off = (
        "conditional_delete",
        table.conditional_delete == Some(false),
    );
    let marks_named = ("marks_bucket", table.marks_bucket.is_some());
    let (spec, marks, endpoints, credential_source) = match (table.kind, &table.path, &table.bucket)
    {
        (Kind::Memory, ..) => {
            refuse(name, "not for kind \"memory\"", &[path, bucket])?;
            refuse(name, "not for kind \"memory\"", &s3_only)?;
            refuse(name, DELETE_OFF, &[delete_off])?;
            refuse(name, MARKS_ONLY_S3, &[marks_named])?;
            ("memory".to_owned(), "memory".to_owned(), Vec::new(), None)
        }
        (Kind::Dir, path_value, _) => {
            refuse(name, "not for kind \"dir\"", &[bucket])?;
            refuse(name, "not for kind \"dir\"", &s3_only)?;
            refuse(name, DELETE_OFF, &[delete_off])?;
            refuse(name, MARKS_ONLY_S3, &[marks_named])?;
            let path = require(name, "path", path_value.as_ref())?;
            let marks = marks_directory(path);
            (
                format!("dir:{}", path.display()),
                format!("dir:{}", marks.display()),
                Vec::new(),
                None,
            )
        }
        (Kind::S3, _, bucket_value) => {
            refuse(name, "not for kind \"s3\"", &[path])?;
            let bucket = require(name, "bucket", bucket_value.as_ref())?;
            let endpoints = endpoints(name, table)?;
            let source = credential_source(name, table)?;
            let marks = marks_bucket(name, bucket, table.marks_bucket.as_deref())?;
            (
                format!("s3:{bucket}"),
                format!("s3:{marks}"),
                endpoints,
                source,
            )
        }
    };
    let mut endpoints = endpoints.into_iter();
    Ok(TierParts {
        spec,
        marks,
        endpoint: endpoints.next(),
        more_endpoints: endpoints.collect(),
        region: table.region.clone(),
        credentials_file: table.credentials_file.clone(),
        credential_source,
        conditional_delete: table.conditional_delete,
    })
}

/// The credential source of the `s3` tier `name`: `None` for the key of `credentials_file`, which
/// is then needed; a source takes no `credentials_file`.
fn credential_source(name: &str, table: &TierTable) -> Result<Option<SourceSetting>, ConfigError> {
    let source_name = table.credential_source.unwrap_or(SourceName::File);
    let profile = table.credential_profile.as_deref();
    let setting = SourceSetting::of(source_name, profile)
        .map_err(|why| invalid(&format!("{name}.credential_profile"), why))?;
    match (&setting, &table.credentials_file) {
        (None, file) => {
            require(name, "credentials_file", file.as_ref())?;
        }
        (Some(setting), Some(_)) => {
            let why = format!(
                "not with credential_source = \"{}\": the credentials come from the one or the other",
                setting.source
            );
            return Err(invalid(&format!("{name}.credentials_file"), why));
        }
        (Some(_), None) => {}
    }
    Ok(setting)
}

/// The endpoints of an `s3` tier: the one `endpoint`, or the entries of `endpoints`, which is not
/// empty and has no entry twice. Exactly one of the two keys is given.
fn endpoints(name: &str, table: &TierTable) -> Result<Vec<String>, ConfigError> {
    let list = match (&table.endpoint, &table.endpoints) {
        (Some(one), None) => return Ok(vec![one.clone()]),
        (Some(_), Some(_)) => {
            let why = "give endpoint or endpoints, not both";
            return Err(invalid(&format!("{name}.endpoints"), why));
        }
        (None, Some(list)) => list,
        (None, None) => return Err(invalid(&format!("{name}.endpoint"), "it is needed")),
    };
    let key = format!("{name}.endpoints");
    if list.is_empty() {
        return Err(invalid(&key, "is empty; give at least one endpoint"));
    }
    if let Some(twice) = list
        .iter()
        .enumerate()
        .find_map(|(at, one)| list[..at].contains(one).then_some(one))
    {
        return Err(invalid(&key, format!("{twice:?} is given twice")));
    }
    Ok(list.clone())
}

/// Why `conditional_delete = false` is refused for a `memory` or `dir` tier.
const DELETE_OFF: &str =
    "false is only for kind \"s3\": a memory or dir tier deletes conditionally in one atomic step";

/// Why `marks_bucket` is refused for a `memory` or `dir` tier.
const MARKS_ONLY_S3: &str = "only for kind \"s3\": a dir tier keeps its markers in the directory \
     of its path with \"-marks\" added, and a memory tier in a map of its own";

/// The bucket of the markers of the tier `name` whose bucket is `bucket`: the one the key names, or
/// `<bucket>-marks`. It is not the bucket of the tier itself, which would put the markers among the
/// objects they describe; no other check is made of the name, and the store is not asked (the
/// activation of the markers checks once that the bucket exists).
fn marks_bucket<'a>(
    name: &str,
    bucket: &'a str,
    named: Option<&'a str>,
) -> Result<String, ConfigError> {
    let Some(marks) = named else {
        return Ok(format!("{bucket}-marks"));
    };
    let key = format!("{name}.marks_bucket");
    if marks.is_empty() {
        return Err(invalid(&key, "is empty"));
    }
    if marks == bucket {
        let why = format!(
            "is the bucket of the tier, {bucket:?}; the markers need a bucket of their own"
        );
        return Err(invalid(&key, why));
    }
    Ok(marks.to_owned())
}

/// The directory beside `path` that holds the markers of a `dir` tier at `path`: its name with
/// `-marks` added, in the same parent. A trailing separator of `path` is not part of the name.
fn marks_directory(path: &Path) -> PathBuf {
    let whole: PathBuf = path.components().collect();
    let mut name: OsString = whole.into_os_string();
    name.push("-marks");
    PathBuf::from(name)
}

#[cfg(test)]
#[path = "config_tier_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "config_credentials_tests.rs"]
mod credentials_tests;

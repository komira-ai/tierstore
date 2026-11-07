//! What a tier table of the configuration file says about its tier: the spec of `--hot` or
//! `--cold`, the S3 keys, the `conditional_delete` key and the marks bucket, checked against the
//! kind of the tier.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::config::{ConfigError, Kind, TierTable, invalid, refuse, require};

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
    ];
    let bucket = ("bucket", table.bucket.is_some());
    let path = ("path", table.path.is_some());
    let delete_off = (
        "conditional_delete",
        table.conditional_delete == Some(false),
    );
    let marks_named = ("marks_bucket", table.marks_bucket.is_some());
    let (spec, marks, endpoints) = match (table.kind, &table.path, &table.bucket) {
        (Kind::Memory, ..) => {
            let keys = [path, bucket, s3_only[0], s3_only[1], s3_only[2], s3_only[3]];
            refuse(name, "not for kind \"memory\"", &keys)?;
            refuse(name, DELETE_OFF, &[delete_off])?;
            refuse(name, MARKS_ONLY_S3, &[marks_named])?;
            ("memory".to_owned(), "memory".to_owned(), Vec::new())
        }
        (Kind::Dir, path_value, _) => {
            let keys = [bucket, s3_only[0], s3_only[1], s3_only[2], s3_only[3]];
            refuse(name, "not for kind \"dir\"", &keys)?;
            refuse(name, DELETE_OFF, &[delete_off])?;
            refuse(name, MARKS_ONLY_S3, &[marks_named])?;
            let path = require(name, "path", path_value.as_ref())?;
            let marks = marks_directory(path);
            (
                format!("dir:{}", path.display()),
                format!("dir:{}", marks.display()),
                Vec::new(),
            )
        }
        (Kind::S3, _, bucket_value) => {
            refuse(name, "not for kind \"s3\"", &[path])?;
            let bucket = require(name, "bucket", bucket_value.as_ref())?;
            let endpoints = endpoints(name, table)?;
            require(name, "credentials_file", table.credentials_file.as_ref())?;
            let marks = marks_bucket(name, bucket, table.marks_bucket.as_deref())?;
            (format!("s3:{bucket}"), format!("s3:{marks}"), endpoints)
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
        conditional_delete: table.conditional_delete,
    })
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

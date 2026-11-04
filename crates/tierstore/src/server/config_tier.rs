//! What a tier table of the configuration file says about its tier: the spec of `--hot` or
//! `--cold`, the S3 keys, and the `conditional_delete` key, checked against the kind of the tier.

use std::path::PathBuf;

use super::config::{ConfigError, Kind, TierTable, refuse, require};

/// What a tier table says about the tier: the spec of `--hot` or `--cold`, and the endpoint,
/// region and credentials file of its S3 flags.
#[derive(Debug)]
pub(super) struct TierParts {
    pub spec: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub credentials_file: Option<PathBuf>,
    /// The `conditional_delete` key; `None` is the default of the kind.
    pub conditional_delete: Option<bool>,
}

/// The tier `table` names, if its keys fit its kind.
pub(super) fn tier(name: &str, table: &TierTable) -> Result<TierParts, ConfigError> {
    let s3_only = [
        ("endpoint", table.endpoint.is_some()),
        ("region", table.region.is_some()),
        ("credentials_file", table.credentials_file.is_some()),
    ];
    let bucket = ("bucket", table.bucket.is_some());
    let path = ("path", table.path.is_some());
    let delete_off = (
        "conditional_delete",
        table.conditional_delete == Some(false),
    );
    let spec = match (table.kind, &table.path, &table.bucket) {
        (Kind::Memory, ..) => {
            let keys = [path, bucket, s3_only[0], s3_only[1], s3_only[2]];
            refuse(name, "not for kind \"memory\"", &keys)?;
            refuse(name, DELETE_OFF, &[delete_off])?;
            "memory".to_owned()
        }
        (Kind::Dir, path_value, _) => {
            let keys = [bucket, s3_only[0], s3_only[1], s3_only[2]];
            refuse(name, "not for kind \"dir\"", &keys)?;
            refuse(name, DELETE_OFF, &[delete_off])?;
            format!(
                "dir:{}",
                require(name, "path", path_value.as_ref())?.display()
            )
        }
        (Kind::S3, _, bucket_value) => {
            refuse(name, "not for kind \"s3\"", &[path])?;
            let bucket = require(name, "bucket", bucket_value.as_ref())?;
            require(name, "endpoint", table.endpoint.as_ref())?;
            require(name, "credentials_file", table.credentials_file.as_ref())?;
            format!("s3:{bucket}")
        }
    };
    Ok(TierParts {
        spec,
        endpoint: table.endpoint.clone(),
        region: table.region.clone(),
        credentials_file: table.credentials_file.clone(),
        conditional_delete: table.conditional_delete,
    })
}

/// Why `conditional_delete = false` is refused for a `memory` or `dir` tier.
const DELETE_OFF: &str =
    "false is only for kind \"s3\": a memory or dir tier deletes conditionally in one atomic step";

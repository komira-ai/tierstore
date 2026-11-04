//! The `Content-Type` of a manifest, taken from the manifest itself.
//!
//! A registry client picks the parser by this header, so it must be the media type the manifest
//! was written with. The object is a blob of the store with no metadata, so the type is read from
//! the JSON's `mediaType`.

use serde_json::Value;

/// The OCI image manifest.
pub(super) const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// The OCI image index.
pub(super) const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// The Docker schema 2 manifest and manifest list.
const DOCKER_TYPES: [&str; 2] = [
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

/// The media type to serve `body` with.
///
/// A `mediaType` that is one of the four manifest types (OCI manifest and index, Docker schema 2
/// manifest and list) is served as it is. A manifest with no `mediaType`, or with another, is
/// served only if it is plainly one: `schemaVersion` 2 and a `config` object with a `layers`
/// array is an OCI image manifest, and `schemaVersion` 2 and a `manifests` array is an OCI image
/// index (the index may leave `mediaType` out). Anything else is refused.
///
/// # Errors
/// What is wrong with `body`, for the client: it is not JSON, or not a manifest.
pub(super) fn media_type(body: &[u8]) -> Result<&'static str, String> {
    let json: Value =
        serde_json::from_slice(body).map_err(|why| format!("the object is not JSON: {why}"))?;
    let declared = json.get("mediaType").and_then(Value::as_str);
    let known = [OCI_MANIFEST, OCI_INDEX, DOCKER_TYPES[0], DOCKER_TYPES[1]];
    if let Some(found) = known.into_iter().find(|&known| Some(known) == declared) {
        return Ok(found);
    }
    let version_two = json.get("schemaVersion") == Some(&Value::from(2));
    let has = |field: &str, kind: fn(&Value) -> bool| json.get(field).is_some_and(kind);
    if version_two && has("config", Value::is_object) && has("layers", Value::is_array) {
        return Ok(OCI_MANIFEST);
    }
    if version_two && has("manifests", Value::is_array) {
        return Ok(OCI_INDEX);
    }
    Err(format!(
        "the object is not a manifest: its mediaType is {declared:?}, and it is not \
         schemaVersion 2 with a config and layers (or manifests)"
    ))
}

/// The digests a manifest or an index names: its config, its layers and its manifests, as the
/// text they are written with. A manifest names no more than it lists, so a `config` that is not
/// an object with a `digest` text, a `layers` or `manifests` that is not an array, or an entry of
/// one that has no `digest` text, is refused.
///
/// # Errors
/// What is wrong with `body`, for the client: it is not JSON, or a descriptor has no digest.
pub(super) fn references(body: &[u8]) -> Result<Vec<String>, String> {
    let json: Value =
        serde_json::from_slice(body).map_err(|why| format!("the object is not JSON: {why}"))?;
    let mut found = Vec::new();
    if let Some(config) = json.get("config") {
        found.push(digest_of(config, "config")?);
    }
    for field in ["layers", "manifests"] {
        let Some(list) = json.get(field) else {
            continue;
        };
        let entries = list
            .as_array()
            .ok_or_else(|| format!("the manifest's {field} is not a list"))?;
        for entry in entries {
            found.push(digest_of(entry, field)?);
        }
    }
    Ok(found)
}

/// The `digest` text of the descriptor `entry`, of the `field` of a manifest.
fn digest_of(entry: &Value, field: &str) -> Result<String, String> {
    entry
        .get("digest")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("an entry of the manifest's {field} has no digest text"))
}

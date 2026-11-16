//! What a key may ask of the trees interface, as far as the request alone says (design 3.2,
//! "the key may read every source object and write every target location ... no new permission
//! exists").
//!
//! The core judges what only it knows: the locations of an existing placement.

use sha2::{Digest as _, Sha256};
use tierstore_trees_api::v1::{CreateTreeRequest, entry, location};

use super::{Caller, Reason, Refusal};
use crate::server::auth;
use crate::server::keys::Role;

/// The object a file with no `from` is the whole of: `cas/l/<sha256 in hex>`.
const WHOLE_OBJECT: &str = "cas/l/";

/// The refusal for a key that may not make a call at all.
fn denied(why: &str) -> Refusal {
    Refusal::about(Reason::AccessDenied, why)
}

/// Whether `caller` may ask `GetPlacement`: every key of the server may; which placement it may
/// see is the core's to say.
///
/// # Errors
/// [`Reason::AccessDenied`] for a key that may not read: a presigning key.
pub(super) fn permit_placement(caller: &Caller) -> Result<(), Refusal> {
    match caller.role {
        Role::PresignPut => Err(denied("this key may not read")),
        _ => Ok(()),
    }
}

/// Whether `caller` may ask `DeleteTree`: a key that may change what is stored. Whether the
/// placement is the key's to release is the core's to say.
///
/// # Errors
/// [`Reason::AccessDenied`] for a read-only key and a presigning key.
pub(super) fn permit_delete(caller: &Caller) -> Result<(), Refusal> {
    match caller.role {
        Role::ReadOnly | Role::PresignPut => Err(denied(auth::READ_ONLY.message)),
        _ => Ok(()),
    }
}

/// Whether `caller` may ask `CreateTree` for this request: the full key may; a read-only and a
/// presigning key may not; a scoped key may when it may read every source object and write every
/// location, each judged on its own, and it names the first that it may not.
///
/// # Errors
/// [`Reason::AccessDenied`] naming the key or the location, or [`Reason::MalformedDigest`] naming
/// the path of a file with no `from` whose `sha256` is not 32 bytes, since it names no object.
pub(super) fn permit_create(caller: &Caller, request: &CreateTreeRequest) -> Result<(), Refusal> {
    let scopes = match &caller.role {
        Role::Full => return Ok(()),
        Role::ReadOnly | Role::PresignPut => {
            return Err(denied(auth::READ_ONLY.message));
        }
        Role::Scoped(scopes) => scopes,
    };
    for entry in &request.entries {
        let Some(entry::Kind::File(file)) = &entry.kind else {
            continue;
        };
        let key = match &file.from {
            Some(from) => from.key.clone(),
            None if file.sha256.len() == Sha256::output_size() => {
                format!("{WHOLE_OBJECT}{}", hex::encode(&file.sha256))
            }
            None => return Err(Refusal::about(Reason::MalformedDigest, &entry.path)),
        };
        if !scopes.may_read(&key) {
            return Err(denied(&format!("this key may not read {key}")));
        }
    }
    for at in &request.at {
        match &at.r#where {
            Some(location::Where::S3(s3)) if scopes.writes_under(&s3.prefix) => {}
            Some(location::Where::S3(s3)) => {
                return Err(denied(&format!("this key may not write {}", s3.prefix)));
            }
            Some(location::Where::Fs(fs)) => {
                let why = format!("only the full key may place a tree at the path {}", fs.path);
                return Err(denied(&why));
            }
            None => {}
        }
    }
    Ok(())
}

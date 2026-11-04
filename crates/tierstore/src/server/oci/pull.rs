//! Pull-through: a miss is filled from the upstream registry, stored, and then served.
//!
//! With `params.upstream` (see `upstream`) the front no longer answers 404 for an object its store
//! lacks. The first request for it is paused while the front asks the upstream for it, stores it
//! as a push does, and then answers from the store as it would for any object it has; every later
//! request is answered from the store and never reaches the upstream.
//!
//! - A manifest by digest: `GET /v2/<name>/manifests/<digest>` upstream. The bytes are read whole
//!   (at most [`MAX_MANIFEST_BYTES`]), must be a manifest (see `manifest`) and must hash to the
//!   digest, and are stored by it.
//! - A manifest by tag: the same request with the tag. The manifest is stored by the digest its
//!   bytes hash to, and the tag's first version is created (see `tags`) with the create-only
//!   write and nothing else; a tag that was made in the meantime is left as it is, and the answer
//!   is the manifest that tag points at, read back after the write. A tag that is in the store is
//!   never asked of the upstream again: it keeps pointing at the manifest it first pointed at,
//!   however the upstream moves it. A tag that is moved by a push is a push's to move.
//! - A blob: `GET /v2/<name>/blobs/<digest>` upstream, handed to the store's streamed put as the
//!   bytes arrive, hashed in the same pass, and refused (nothing is stored) if the hash is not
//!   the digest. There is no limit on its size and no copy of it in memory. The client's own
//!   request waits for the blob to be stored, then it is served from the store, with its range if
//!   it asked for one; a `HEAD` of a blob that is not there fills it the same way. The store's
//!   put runs on a task of its own, so a client that goes away does not stop the blob being stored.
//!
//! What goes to the upstream is the `<name>` of the request, checked against its grammar, and the
//! digest or tag. The store is still one content-addressed namespace: a manifest or blob that is in
//! it is served for any name, and a name is asked of the upstream only for what the store lacks.
//! Two requests for one missing object at once each pull it (the second's create-only write finds
//! it stored and is taken as done); nothing joins them.
//!
//! What the client is told: 404 as before when the upstream has no such object, and 502 with the
//! code `UNAVAILABLE` for any other failure of the upstream (it cannot be reached, it refuses the
//! login, it sends something that is not what was asked for). What the upstream said goes to
//! stderr and not to the client. Listings (`tags/list`) are the store's own and are not pulled.

use std::sync::Arc;

use bytes::Bytes;
use futures::stream::StreamExt as _;
use hyper::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::metrics::Outcome;
use super::names::Digest;
use super::push::{Answer, Refusal, Tally, counted};
use super::upstream::{Upstream, UpstreamError};
use super::{MAX_MANIFEST_BYTES, OciFront, echoed, error, manifest, store_failed, tags};
use crate::Error;
use crate::server::ops::detached;

/// The media types a manifest is asked of the upstream as.
const ACCEPT_MANIFESTS: &str = "application/vnd.oci.image.manifest.v1+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.docker.distribution.manifest.list.v2+json";

const MANIFEST_UNKNOWN: &str = "MANIFEST_UNKNOWN";
const BLOB_UNKNOWN: &str = "BLOB_UNKNOWN";

/// 502: the upstream could not give what was asked for.
fn unavailable(message: &str, detail: Value) -> Answer {
    let response = error(StatusCode::BAD_GATEWAY, "UNAVAILABLE", message, detail);
    (Outcome::Failed, response)
}

/// The answer for a pull that failed: 404 if the upstream has no such object, else 502.
fn pull_failed(failure: &UpstreamError, key: &str, unknown: &str, detail: Value) -> Answer {
    match failure {
        UpstreamError::NotFound => {
            let absent = Error::NotFound(key.to_owned());
            store_failed(&absent, key, unknown, detail)
        }
        other => {
            tracing::warn!("tierstore: oci: pulling {key} from the upstream: {other}");
            let message = "the upstream registry could not give the object";
            unavailable(message, detail)
        }
    }
}

impl OciFront {
    /// The manifest the upstream has at `reference` of `name`, and the digest its bytes hash to.
    async fn fetch_manifest(
        &self,
        upstream: &Upstream,
        name: &str,
        reference: &str,
        detail: &Value,
    ) -> Result<(Bytes, Digest), Answer> {
        let path = format!("/v2/{name}/manifests/{reference}");
        let failed =
            |why: UpstreamError| pull_failed(&why, &path, MANIFEST_UNKNOWN, detail.clone());
        let reply = upstream
            .get(&path, ACCEPT_MANIFESTS)
            .await
            .map_err(failed)?;
        let failed =
            |why: UpstreamError| pull_failed(&why, &path, MANIFEST_UNKNOWN, detail.clone());
        let body = reply.bytes(MAX_MANIFEST_BYTES).await.map_err(failed)?;
        if let Err(why) = manifest::media_type(&body) {
            tracing::warn!("tierstore: oci: the upstream's {path} is not a manifest: {why}");
            return Err(unavailable(
                "the upstream's answer is not a manifest",
                detail.clone(),
            ));
        }
        let digest = Digest::of_hex(hex::encode(Sha256::digest(&body)));
        Ok((body, digest))
    }

    /// Stores the manifest `body` at `key`; a manifest that is already stored is as good.
    async fn keep_manifest(&self, key: &str, body: Bytes, detail: Value) -> Result<(), Answer> {
        match self.backend.put(key, body).await {
            Ok(()) | Err(Error::AlreadyExists(_)) => Ok(()),
            Err(failure) => Err(store_failed(&failure, key, MANIFEST_UNKNOWN, detail)),
        }
    }

    /// Fills the manifest `digest` from the upstream.
    ///
    /// # Errors
    /// The answer to send: 404 if the upstream has none, 502 if it fails or sends bytes that are
    /// not that manifest, and the store's if it cannot store it.
    pub(super) async fn pull_manifest(
        &self,
        upstream: &Upstream,
        name: &str,
        digest: &Digest,
    ) -> Result<(), Answer> {
        let detail = json!({ "digest": digest.text() });
        let (body, found) = self
            .fetch_manifest(upstream, name, &digest.text(), &detail)
            .await?;
        if found != *digest {
            self.metrics.mismatch();
            tracing::warn!(
                "tierstore: oci: the upstream's {} hashes to {}",
                digest.text(),
                found.text()
            );
            let message = "the upstream's manifest does not match its digest and is not stored";
            return Err(unavailable(message, detail));
        }
        self.keep_manifest(&self.prefix.key(digest), body, detail)
            .await
    }

    /// Fills the tag `tag` of `name` from the upstream, and gives the digest it points at.
    ///
    /// # Errors
    /// As [`pull_manifest`](Self::pull_manifest).
    pub(super) async fn pull_tag(
        &self,
        upstream: &Upstream,
        name: &str,
        tag: &str,
    ) -> Result<Digest, Answer> {
        let detail = json!({ "tag": echoed(tag) });
        let (body, digest) = self.fetch_manifest(upstream, name, tag, &detail).await?;
        let key = self.prefix.key(&digest);
        self.keep_manifest(&key, body, detail.clone()).await?;
        let prefix = self.tag_prefix(name, tag);
        let failed =
            |failure: Error| store_failed(&failure, &prefix, MANIFEST_UNKNOWN, detail.clone());
        tags::create_first(&*self.backend, &prefix, &digest)
            .await
            .map_err(failed)?;
        // The tag now holds what its newest version says: this pull's digest, or that of a tag
        // made in the meantime, which is the one answered.
        self.tagged(name, tag).await.map_err(failed)
    }

    /// Fills the blob `digest` from the upstream, in one pass: the bytes are hashed as they are
    /// handed to the store, and nothing is stored if the hash is not the digest.
    ///
    /// # Errors
    /// As [`pull_manifest`](Self::pull_manifest).
    pub(super) async fn pull_blob(
        &self,
        upstream: &Upstream,
        name: &str,
        digest: &Digest,
    ) -> Result<(), Answer> {
        let key = self.prefix.key(digest);
        let detail = json!({ "digest": digest.text() });
        let path = format!("/v2/{name}/blobs/{}", digest.text());
        let reply = upstream.get(&path, "application/octet-stream").await;
        let reply = reply.map_err(|why| pull_failed(&why, &path, BLOB_UNKNOWN, detail.clone()))?;
        let length = reply.length();
        let tally = Arc::new(Tally::default());
        let noted = Arc::clone(&tally);
        let source = reply
            .pieces()
            .inspect(move |piece| {
                if let Err(why) = piece {
                    noted.refuse(Refusal::Body(why.to_string()));
                }
            })
            .boxed();
        let body = counted(source, Some(digest.clone()), Arc::clone(&tally));
        let backend = Arc::clone(&self.backend);
        let stored_key = key.clone();
        let stored = detached(async move { backend.put_stream(&stored_key, length, body).await });
        match stored.await {
            Ok(()) | Err(Error::AlreadyExists(_)) => Ok(()),
            Err(failure) => Err(match tally.take() {
                Some(Refusal::Digest { found }) => {
                    self.metrics.mismatch();
                    tracing::warn!(
                        "tierstore: oci: the upstream's {} hashes to sha256:{found}",
                        digest.text()
                    );
                    let message = "the upstream's blob does not match its digest and is not stored";
                    unavailable(message, detail)
                }
                Some(Refusal::Body(why)) => {
                    tracing::warn!("tierstore: oci: the upstream's {path} was cut: {why}");
                    unavailable("the upstream's blob could not be read in full", detail)
                }
                // A range refusal is made only by a push's chunk body, never by a pull's.
                Some(Refusal::Range) | None => store_failed(&failure, &key, BLOB_UNKNOWN, detail),
            }),
        }
    }
}

#[cfg(test)]
#[path = "pull_tests.rs"]
mod tests;

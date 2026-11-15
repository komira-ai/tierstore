//! What the front calls: [`TreeCore`], the node-local tree store, and who is calling.

use async_trait::async_trait;
use tierstore_trees_api::v1::{
    CreateTreeRequest, CreateTreeResponse, DeleteTreeRequest, DeleteTreeResponse,
    GetPlacementRequest, GetPlacementResponse,
};

use super::Refusal;
use crate::server::keys::Role;

/// The key that made a call, after the front has checked its signature and the rights that can be
/// told from the request alone (see `rights`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Caller {
    /// The id of the access key.
    pub(crate) key_id: String,
    /// What the key may do. A scoped key's prefixes are what the core judges a placement's
    /// locations against, for `GetPlacement` and `DeleteTree`.
    pub(crate) role: Role,
}

/// The work behind the three calls of `TreesService`: the node's tree store. The front answers a
/// [`Refusal`] a core returns with the status of its reason, and sends a core nothing but a
/// request whose key it has checked.
///
/// A core is made when the node has one; the interface is served without one until then, and
/// answers `NotServing` (see [`Reason::NotServing`](super::Reason)).
#[async_trait]
pub(crate) trait TreeCore: Send + Sync {
    /// Creates a tree and places it (`CreateTree`).
    ///
    /// # Errors
    /// The refusal of the design that stands for what is wrong, or what failed.
    async fn create(
        &self,
        caller: &Caller,
        request: CreateTreeRequest,
    ) -> Result<CreateTreeResponse, Refusal>;

    /// The state of a placement, and the children of a directory of it if asked (`GetPlacement`).
    ///
    /// # Errors
    /// [`Reason::UnknownPlacement`](super::Reason) for an id no placement has, or another
    /// refusal.
    async fn placement(
        &self,
        caller: &Caller,
        request: GetPlacementRequest,
    ) -> Result<GetPlacementResponse, Refusal>;

    /// Releases a placement (`DeleteTree`).
    ///
    /// # Errors
    /// [`Reason::UnknownPlacement`](super::Reason) for an id no placement has, or another
    /// refusal.
    async fn delete(
        &self,
        caller: &Caller,
        request: DeleteTreeRequest,
    ) -> Result<DeleteTreeResponse, Refusal>;
}

//! The fake cluster: the fake storage nodes by id, as one [`PeerClient`].

use std::collections::BTreeMap;
use std::sync::Arc;

use tierstore_fetch_api::clock::ManualClock;
use tierstore_format::ids::NodeId;

use super::node::FakeStorageNode;
use crate::hot::ReplicaBatch;
use crate::peer::{PeerBody, PeerClient, PeerError, PeerGet};
use crate::proto::{
    CommitAnswer, CommitRecord, Envelope, KeyCheck, KeyFacts, LocateRequest, LocateResponse,
    PingAnswer, ReplicaAnswer, SealAnswer, SealMessage, TombPage, TombPull,
};

/// A set of fake storage nodes on one injected clock. Each call goes to the node it names; a node
/// the cluster does not hold is `Unreachable`.
#[derive(Debug)]
pub struct FakeCluster {
    clock: ManualClock,
    nodes: BTreeMap<NodeId, Arc<FakeStorageNode>>,
}

impl FakeCluster {
    /// A cluster with no node, on `clock`.
    #[must_use]
    pub fn new(clock: ManualClock) -> Self {
        Self {
            clock,
            nodes: BTreeMap::new(),
        }
    }

    /// The injected clock.
    #[must_use]
    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }

    /// Adds `node` (replacing a node of the same id).
    pub fn add(&mut self, node: Arc<FakeStorageNode>) {
        self.nodes.insert(node.id(), node);
    }

    /// Makes a node `id` on the cluster's clock, adds it and returns it.
    pub fn spawn(&mut self, id: NodeId) -> Arc<FakeStorageNode> {
        let node = Arc::new(FakeStorageNode::new(id, self.clock.clone()));
        self.add(Arc::clone(&node));
        node
    }

    /// The node `id`, if the cluster holds it.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<Arc<FakeStorageNode>> {
        self.nodes.get(&id).cloned()
    }

    fn route(&self, id: NodeId) -> Result<&Arc<FakeStorageNode>, PeerError> {
        self.nodes.get(&id).ok_or_else(|| PeerError::Unreachable {
            node: id,
            reason: "no such node in the fake cluster".to_owned(),
        })
    }
}

#[async_trait::async_trait]
impl PeerClient for FakeCluster {
    async fn locate(
        &self,
        node: NodeId,
        req: LocateRequest,
    ) -> Result<Envelope<LocateResponse>, PeerError> {
        self.route(node)?.locate(req).await
    }

    async fn get(&self, node: NodeId, req: PeerGet) -> Result<PeerBody, PeerError> {
        self.route(node)?.get(req).await
    }

    async fn replicate(
        &self,
        node: NodeId,
        batch: Envelope<ReplicaBatch>,
    ) -> Result<Envelope<ReplicaAnswer>, PeerError> {
        self.route(node)?.replicate(batch).await
    }

    async fn seal(
        &self,
        node: NodeId,
        msg: Envelope<SealMessage>,
    ) -> Result<Envelope<SealAnswer>, PeerError> {
        self.route(node)?.seal(msg).await
    }

    async fn key_check(
        &self,
        node: NodeId,
        check: Envelope<KeyCheck>,
    ) -> Result<Envelope<KeyFacts>, PeerError> {
        self.route(node)?.key_check(check).await
    }

    async fn commit(
        &self,
        node: NodeId,
        record: Envelope<CommitRecord>,
    ) -> Result<Envelope<CommitAnswer>, PeerError> {
        self.route(node)?.commit(record).await
    }

    async fn tomb_pull(
        &self,
        node: NodeId,
        pull: TombPull,
    ) -> Result<Envelope<TombPage>, PeerError> {
        self.route(node)?.tomb_pull(pull).await
    }

    async fn ping(&self, node: NodeId) -> Result<Envelope<PingAnswer>, PeerError> {
        self.route(node)?.ping().await
    }
}

//! The shape of the configuration file of a node over the cold tier alone: `[cold_store]` with no
//! `[hot_store]` and no `[local_cache]`, with `[server]` and `[[interface]]` as every file has them.
//!
//! ```toml
//! [[interface]]
//! type   = "s3"
//! params = { addresses = ["127.0.0.1:9300"] }
//!
//! [server]
//! proxy_id        = "tierstore-front-1"
//! bucket          = "tierstore"
//! access_key_file = "/etc/tierstore/access.key"
//!
//! [cold_store]
//! kind             = "s3"
//! bucket           = "tierstore-cold"
//! endpoints        = ["http://storage-1.example:9000", "http://storage-2.example:9000"]
//! credentials_file = "/etc/tierstore/cold.credentials"
//! ```
//!
//! The fronts read and write the cold store directly: a write is in it when it is acknowledged,
//! and nothing is kept on the node. The node has no registration, format object, lease or
//! reconciler, and owes no tier anything when it stops. It is the shape of a front that must not
//! store data of its own, over a cold store that other nodes write too.
//!
//! The file is read into a [`ColdSettings`]. What it refuses, besides what every file does: a
//! `[[policy]]` with `cache = true` (there is no cache), a `[trees]` table or a `trees`
//! interface (there is no tree store), the keys only `[hot_store]` has, `cold_store.skip_checks`
//! and `server.backend_check_wait`.

use std::time::Duration;

use crate::server::config_handoff::handoff_of;
use crate::server::config_hot_only::refuse_hot_keys;
use crate::server::config_interface::{Interfaces, no_trees};
use crate::server::config_policy::{Policy, check_policies};
use crate::server::config_tuning::S3Client;
use crate::server::front::FrontSettings;
use crate::server::handoff::Handoff;

use super::{
    ConfigError, OciFlags, PerRun, ServerTable, TierParts, TierTable, front_of, refuse, tier,
};

/// The settings of a node over the cold tier alone.
#[derive(Debug)]
pub(in crate::server) struct ColdSettings {
    /// The S3 front.
    pub front: FrontSettings,
    /// The limit on a request to the cold store.
    pub s3_request_timeout: Duration,
    /// The connect timeout, body rate, idle-connection timeout and read retries of the client of
    /// the cold store.
    pub s3_client: S3Client,
    /// The cold tier: the spec and the flags of its store.
    pub cold: TierParts,
    /// The OCI front, as the `oci` interfaces make it: `listen` is empty for none.
    pub oci: OciFlags,
    /// The hand-off of the listening sockets (`server.handoff_socket` and `server.handoff_wait`),
    /// or none without `server.handoff_socket`.
    pub handoff: Option<Handoff>,
}

impl ColdSettings {
    /// The settings of the tables of a file with `[cold_store]` alone.
    pub(in crate::server) fn new(
        server: ServerTable,
        cold: &TierTable,
        fronts: Interfaces,
        policy: &[Policy],
        per_run: PerRun,
    ) -> Result<Self, ConfigError> {
        check_policies(policy, false)?;
        no_trees(&fronts, "a node over the cold tier alone")?;
        let unused = [("backend_check_wait", server.backend_check_wait.is_some())];
        refuse(
            "server",
            "not used by a node over the cold tier alone",
            &unused,
        )?;
        refuse_hot_keys(cold)?;
        let skipping = [("skip_checks", cold.skip_checks.is_some())];
        refuse(
            "cold_store",
            "not for a node over the cold tier alone",
            &skipping,
        )?;
        let parts = tier("cold_store", cold)?;
        let handoff = handoff_of(&server, None, per_run.state_dir.as_deref())?;
        let oci = fronts.oci;
        let shared = front_of(server, fronts.s3, per_run)?;
        Ok(Self {
            front: shared.front,
            s3_request_timeout: shared.s3_request_timeout,
            s3_client: shared.s3_client,
            cold: parts,
            oci,
            handoff,
        })
    }
}

#[cfg(test)]
#[path = "config_cold_tests.rs"]
mod tests;

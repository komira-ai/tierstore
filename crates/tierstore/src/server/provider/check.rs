//! The trees of the update check: two small trees made for the purpose, which the update hook's
//! `verify` has the node's mount provider prepare, mount and release, as jobscheduler's agent does
//! for a job, and whose every file it reads back through the mount (`update_hook/mount_check.rs`).
//!
//! The two trees are stored as the build service writes a tree today, both forms a build farm
//! holds: [`WITH_SIDECAR`] has its version 2 list at `cas/l/<id>` and its L2 sidecar at
//! `cas/l2/<id>`, and [`WITHOUT_SIDECAR`] has its version 2 list alone (a tree written while its
//! pack's cells were not known, or before the sidecar). The provider places the first from its
//! sidecar and the second from its list, so a release that refuses either form of tree fails the
//! check. Their files lie in one pack, [`pack`], at [`pack_key`].
//!
//! The objects are this module's, never the upstream's: [`CheckUpstream`] answers the reads of
//! exactly their keys from memory (and that the second tree has no sidecar), and hands every other
//! call to the upstream unchanged. So the check reads nothing from the network and writes nothing
//! to the store. The lists and the sidecar are kept as the bytes the writers made
//! (`check/*.list`, `check/*.l2`); a test makes them again with the writers of today
//! (`TreeList::encode_v2`, `tierstore_list::sidecar_from_v2`) and compares.
//!
//! The check's sources are [`AGENT`]'s, a name no agent of jobscheduler has (an agent is named by
//! its host's id), so a check never touches a job's sources, and nothing sweeps them.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::server::update_hook::TreeNode;
use crate::tree_list_key;

#[path = "check_upstream.rs"]
mod upstream;

pub(in crate::server) use upstream::CheckUpstream;

/// The agent of the check's sources.
pub(in crate::server) const AGENT: &str = "tierstore-verify";

/// The run of the check's sources.
pub(in crate::server) const RUN: &str = "js.tierstore-verify";

/// The first part of the key of the check's pack, before the hex of its SHA-256.
pub(in crate::server) const PACK_PREFIX: &str = "cas/check/";

/// The bytes of the executable file of both trees.
const TOOL: &[u8] = b"#!/bin/sh\n# tierstore update check: read back through the mount\nexit 0\n";

/// The bytes of the plain file of both trees.
const DATA: &[u8] = b"tierstore update check: the bytes of a file the provider placed\n";

/// The bytes of `variant` in the tree placed from its sidecar.
const FROM_SIDECAR: &[u8] = b"placed from its L2 sidecar\n";

/// The bytes of `variant` in the tree placed from its list.
const FROM_LIST: &[u8] = b"placed from its tree list\n";

/// The files of the pack, in its order.
const BLOBS: [&[u8]; 4] = [TOOL, DATA, FROM_SIDECAR, FROM_LIST];

/// One tree of the check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) struct CheckTree {
    /// What it is, for the details: `with its L2 sidecar` or `without an L2 sidecar`.
    pub form: &'static str,
    /// Its source's place among the check's sources.
    pub index: u32,
    /// The bytes of its version 2 list.
    pub list: &'static [u8],
    /// The bytes of its L2 sidecar, if it has one.
    pub sidecar: Option<&'static [u8]>,
    /// The bytes of its file `variant`.
    variant: &'static [u8],
}

/// The tree with its L2 sidecar.
pub(in crate::server) const WITH_SIDECAR: CheckTree = CheckTree {
    form: "with its L2 sidecar",
    index: 0,
    list: include_bytes!("check/with_sidecar.list"),
    sidecar: Some(include_bytes!("check/with_sidecar.l2")),
    variant: FROM_SIDECAR,
};

/// The tree without an L2 sidecar.
pub(in crate::server) const WITHOUT_SIDECAR: CheckTree = CheckTree {
    form: "without an L2 sidecar",
    index: 1,
    list: include_bytes!("check/without_sidecar.list"),
    sidecar: None,
    variant: FROM_LIST,
};

/// Both trees, in the order the check takes them.
pub(in crate::server) const TREES: [CheckTree; 2] = [WITH_SIDECAR, WITHOUT_SIDECAR];

impl CheckTree {
    /// The key of its list, `cas/l/<id>`: the ref a `Prepare` names.
    pub(in crate::server) fn key(&self) -> String {
        tree_list_key(self.list)
    }

    /// The key of its L2 sidecar, `cas/l2/<id>`, whether it has one or not.
    pub(in crate::server) fn sidecar_key(&self) -> String {
        tierstore_list::sidecar_key(&Sha256::digest(self.list).into())
    }

    /// Every path of the tree below its directory, with what it is: the files with their bytes,
    /// the directories (those the list names and those its paths imply) and the link.
    pub(in crate::server) fn nodes(&self) -> BTreeMap<String, TreeNode> {
        let mut nodes = BTreeMap::new();
        for dir in ["bin", "data", "data/dir"] {
            nodes.insert(dir.to_owned(), TreeNode::Dir);
        }
        nodes.insert("bin/tool".to_owned(), TreeNode::File(TOOL.to_vec()));
        nodes.insert("data/one".to_owned(), TreeNode::File(DATA.to_vec()));
        nodes.insert("data/empty".to_owned(), TreeNode::File(Vec::new()));
        nodes.insert("tool".to_owned(), TreeNode::Link("bin/tool".to_owned()));
        nodes.insert("variant".to_owned(), TreeNode::File(self.variant.to_vec()));
        nodes
    }
}

/// The bytes of the pack both trees' files lie in.
pub(in crate::server) fn pack() -> Vec<u8> {
    BLOBS.concat()
}

/// The key of the pack: [`PACK_PREFIX`] and the hex of its SHA-256.
pub(in crate::server) fn pack_key() -> String {
    format!("{PACK_PREFIX}{}", hex::encode(Sha256::digest(pack())))
}

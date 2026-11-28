//! A mount provider for the tests of the steps: it places the trees of the update check as the
//! provider does (the tree a `Prepare` names, at the source's directory below the mount), answers
//! a `Mount` of a source it holds nothing of as the provider does, and can be told to refuse a
//! call, to place a tree that reads back wrong, or to keep a placement past its release.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::ports::{MountProvider, ProviderCode, ProviderFault, ReadFault, TreeAt, TreeNode};
use crate::server::provider::NOT_PREPARED_HERE;
use crate::server::provider::check::TREES;

/// The mount the fake places its trees under, the provider's default.
pub(crate) const FAKE_MOUNT: &str = "/run/tierstore/fs";

/// The file of a node whose `mount-provider` interface is on this socket, under [`FAKE_MOUNT`].
pub(crate) const PROVIDER_ENTRY: &str = "\n[[interface]]\ntype = \"mount-provider\"\nparams = { socket = \"/var/lib/tierstore/provider.sock\" }\n";

/// The socket of [`PROVIDER_ENTRY`].
pub(crate) const FAKE_SOCKET: &str = "/var/lib/tierstore/provider.sock";

/// What the fake has and has been told.
#[derive(Default)]
struct State {
    /// The ref each source was prepared with, while it is placed.
    placed: BTreeMap<u32, String>,
    /// The calls, as `<call> <index>`, `read <dir>` for a read through the mount and
    /// `read <path under the mount> from <front>` for a read from an NFS front.
    calls: Vec<String>,
    /// What a call of a source is answered with instead, by `<call> <index>`.
    faults: BTreeMap<String, ProviderFault>,
    /// What a read of a source's tree finds instead of its tree, by index.
    reads: BTreeMap<u32, Result<BTreeMap<String, TreeNode>, ReadFault>>,
    /// The directory a `Mount` of a source answers instead of its own, by index.
    dirs: BTreeMap<u32, PathBuf>,
    /// Whether a release leaves the placement.
    keeps: bool,
}

/// The fake provider.
#[derive(Default)]
pub(crate) struct FakeProvider {
    state: RefCell<State>,
    /// The mount its trees are under; [`FAKE_MOUNT`] when none is given.
    mount: Option<PathBuf>,
}

impl FakeProvider {
    /// A fake that places its trees under `mount`.
    pub(crate) fn under(mount: &str) -> FakeProvider {
        FakeProvider {
            mount: Some(PathBuf::from(mount)),
            ..FakeProvider::default()
        }
    }

    /// The directory of the source `index` under this fake's mount.
    fn dir(&self, index: u32) -> PathBuf {
        self.mount_dir()
            .join(format!("mount-provider/check/{index}"))
    }

    /// Answers `call` (`Prepare`, `Mount` or `Release`) of the source `index` with `fault`, every
    /// time.
    pub(crate) fn fail(&self, call: &str, index: u32, fault: ProviderFault) {
        let key = format!("{call} {index}");
        self.state.borrow_mut().faults.insert(key, fault);
    }

    /// Answers `call` of `index` with a status `code` and `reason`.
    pub(crate) fn answer(&self, call: &str, index: u32, code: ProviderCode, reason: &str) {
        let reason = reason.to_owned();
        self.fail(call, index, ProviderFault::Answered { code, reason });
    }

    /// A read of the tree of `index` finds `read` instead.
    pub(crate) fn read_as(&self, index: u32, read: Result<BTreeMap<String, TreeNode>, ReadFault>) {
        self.state.borrow_mut().reads.insert(index, read);
    }

    /// A mount of `index` answers `dir`.
    pub(crate) fn mount_at(&self, index: u32, dir: &str) {
        self.state
            .borrow_mut()
            .dirs
            .insert(index, PathBuf::from(dir));
    }

    /// A release leaves the placement.
    pub(crate) fn keep_placements(&self) {
        self.state.borrow_mut().keeps = true;
    }

    /// The calls so far.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.state.borrow().calls.clone()
    }

    /// The sources placed now.
    pub(crate) fn placed(&self) -> Vec<u32> {
        self.state.borrow().placed.keys().copied().collect()
    }

    /// The directory of the source `index`.
    pub(crate) fn dir_of(index: u32) -> PathBuf {
        Path::new(FAKE_MOUNT).join(format!("mount-provider/check/{index}"))
    }

    /// Records `call` of `index`, and the fault it is to be answered with.
    fn called(&self, call: &str, index: u32) -> Result<(), ProviderFault> {
        let key = format!("{call} {index}");
        let mut state = self.state.borrow_mut();
        state.calls.push(key.clone());
        state.faults.get(&key).cloned().map_or(Ok(()), Err)
    }
}

impl MountProvider for FakeProvider {
    fn prepare(&self, _socket: &Path, index: u32, reference: &str) -> Result<(), ProviderFault> {
        self.called("Prepare", index)?;
        let mut state = self.state.borrow_mut();
        state.placed.insert(index, reference.to_owned());
        Ok(())
    }

    fn mount(&self, _socket: &Path, index: u32) -> Result<PathBuf, ProviderFault> {
        self.called("Mount", index)?;
        let state = self.state.borrow();
        if !state.placed.contains_key(&index) {
            return Err(ProviderFault::Answered {
                code: ProviderCode::Unavailable,
                reason: NOT_PREPARED_HERE.to_owned(),
            });
        }
        Ok(state
            .dirs
            .get(&index)
            .cloned()
            .unwrap_or_else(|| self.dir(index)))
    }

    fn release(&self, _socket: &Path, index: u32) -> Result<(), ProviderFault> {
        self.called("Release", index)?;
        let mut state = self.state.borrow_mut();
        if !state.keeps {
            state.placed.remove(&index);
        }
        Ok(())
    }

    fn read_tree(&self, at: TreeAt<'_>) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
        let (call, dir) = match at {
            TreeAt::Mount(dir) => (format!("read {}", dir.display()), dir.to_path_buf()),
            TreeAt::Front { front, below } => (
                format!("read {} from {front}", below.display()),
                self.mount_dir().join(below),
            ),
        };
        self.found(call, &dir)
    }
}

impl FakeProvider {
    /// The mount its trees are under.
    fn mount_dir(&self) -> &Path {
        self.mount.as_deref().unwrap_or(Path::new(FAKE_MOUNT))
    }

    /// What a read, recorded as `call`, finds at `dir`: the tree placed there, or what the fake
    /// was told a read of it finds.
    fn found(&self, call: String, dir: &Path) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
        let mut state = self.state.borrow_mut();
        state.calls.push(call);
        let index = (0..2)
            .find(|index| *dir == self.dir(*index))
            .ok_or_else(|| {
                ReadFault::NotTheTree(format!("{} is no source's directory", dir.display()))
            })?;
        if let Some(read) = state.reads.get(&index) {
            return read.clone();
        }
        let reference = state
            .placed
            .get(&index)
            .ok_or_else(|| ReadFault::Failed(format!("{} is not placed", dir.display())))?;
        let tree = TREES
            .iter()
            .find(|tree| tree.key() == *reference)
            .ok_or_else(|| ReadFault::NotTheTree(format!("{reference} is no tree of the check")))?;
        Ok(tree.nodes())
    }
}

//! The mount check of `verify`, against a fake provider: the calls in their order, and every
//! answer that fails, defers or passes it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::mount_check::{Read as ReadVia, check, on_node};
use super::params::Params;
use super::ports::{ProviderCode, ProviderFault, ReadFault, TreeAt, TreeNode};
use super::provider_fake::{FAKE_MOUNT, FAKE_SOCKET, FakeProvider, PROVIDER_ENTRY};
use super::report::{Done, Outcome, Reason};
use super::shape::ProviderShape;
use super::systemd_tests::fake::SystemdFake;
use crate::server::provider::check::{TREES, WITH_SIDECAR, WITHOUT_SIDECAR};

/// The provider of [`PROVIDER_ENTRY`].
fn shape() -> ProviderShape {
    ProviderShape {
        socket: PathBuf::from(FAKE_SOCKET),
        mountpoint: PathBuf::from(FAKE_MOUNT),
        nfs: None,
    }
}

/// The check against `provider`, retried after 5 s when deferred.
fn checked(provider: &FakeProvider) -> Result<String, Done> {
    check(provider, &shape(), 5)
}

/// The failure `answer` is, with its reason and detail.
fn failed(answer: Result<String, Done>) -> (Outcome, Option<Reason>, Option<u64>, String) {
    let done = answer.expect_err("the check does not pass");
    let detail = done.report.detail.clone().unwrap_or_default();
    (
        done.outcome,
        done.report.reason,
        done.report.retry_after_secs,
        detail,
    )
}

/// The directories of the two sources, as a read names them.
fn read(index: u32) -> String {
    format!("read {}", FakeProvider::dir_of(index).display())
}

/// The calls of a check that passes.
fn every_call() -> Vec<String> {
    let mut calls: Vec<String> = [
        "Release 0",
        "Release 1",
        "Prepare 0",
        "Prepare 1",
        "Mount 0",
        "Mount 1",
    ]
    .map(str::to_owned)
    .to_vec();
    calls.extend([read(0), read(1)]);
    calls.extend(["Release 0", "Release 1", "Mount 0", "Mount 1"].map(str::to_owned));
    calls
}

/// Asserts that a check releases both sources first, prepares both with their trees' lists,
/// mounts both, reads both trees, releases both and asks for each again, in that order, and passes
/// with a detail that names the socket and both forms of tree, leaving nothing placed. Catches a
/// check that skips a call, reads a tree before both are mounted (so that a refusal of the second
/// is hidden behind a read of the first), or leaves a placement behind.
#[test]
fn a_check_that_passes_makes_every_call_in_order_and_leaves_nothing() {
    let provider = FakeProvider::default();
    let said = checked(&provider).expect("the check passes");
    assert_eq!(
        said,
        format!(
            "the mount provider at {FAKE_SOCKET} placed, served and released a tree with its L2 \
             sidecar and one without an L2 sidecar"
        )
    );
    assert_eq!(provider.calls(), every_call());
    assert!(provider.placed().is_empty());
}

/// Asserts that a provider that refuses the tree without an L2 sidecar (`FAILED_PRECONDITION` at
/// its `Mount`, as a release that places only trees with a sidecar answers) fails the check with
/// `provider_refused`, the detail naming the call, the tree, its key, the status and the
/// provider's reason in full, and that both sources are released before the check ends. Catches a
/// verify that passes a release whose provider refuses every job's tree of that form.
#[test]
fn a_refusal_of_the_tree_without_a_sidecar_fails_the_check_and_releases_both() {
    let provider = FakeProvider::default();
    let reason = format!("no tree list at {}", WITHOUT_SIDECAR.key());
    provider.answer("Mount", 1, ProviderCode::FailedPrecondition, &reason);
    let (outcome, why, retry, detail) = failed(checked(&provider));
    assert_eq!(
        (outcome, why, retry),
        (Outcome::Failure, Some(Reason::ProviderRefused), None)
    );
    assert_eq!(
        detail,
        format!(
            "the mount provider at {FAKE_SOCKET} refused Mount of the tree without an L2 sidecar \
             ({}) with FailedPrecondition: {reason}",
            WITHOUT_SIDECAR.key()
        )
    );
    let calls = provider.calls();
    assert_eq!(
        calls[calls.len() - 2..],
        ["Release 0".to_owned(), "Release 1".to_owned()]
    );
    assert!(!calls.iter().any(|call| call.starts_with("read")));
    assert!(provider.placed().is_empty());
}

/// Asserts that every status but `UNAVAILABLE`, at any call of either tree, fails the check with
/// `provider_refused` and the status named: `INVALID_ARGUMENT` and another status at a `Prepare`,
/// `FAILED_PRECONDITION` at the tree with a sidecar's `Mount`, and a refusal of the first
/// `Release`. Catches a status the check takes for a pass, or for a provider that is not there.
#[test]
fn every_other_refusal_fails_the_check_with_its_status() {
    let cases = [
        (
            "Prepare",
            0,
            ProviderCode::InvalidArgument,
            "InvalidArgument",
        ),
        ("Prepare", 1, ProviderCode::Other(12), "Other(12)"),
        (
            "Mount",
            0,
            ProviderCode::FailedPrecondition,
            "FailedPrecondition",
        ),
        (
            "Release",
            0,
            ProviderCode::FailedPrecondition,
            "FailedPrecondition",
        ),
    ];
    for (call, index, code, named) in cases {
        let provider = FakeProvider::default();
        provider.answer(call, index, code, "said no");
        let (outcome, why, _, detail) = failed(checked(&provider));
        assert_eq!(
            (outcome, why),
            (Outcome::Failure, Some(Reason::ProviderRefused))
        );
        let tree = TREES[index as usize];
        assert_eq!(
            detail,
            format!(
                "the mount provider at {FAKE_SOCKET} refused {call} of the tree {} ({}) with \
                 {named}: said no",
                tree.form,
                tree.key()
            )
        );
        assert!(provider.placed().is_empty(), "{call} {index}");
    }
}

/// Asserts that a provider that does not answer, or answers `UNAVAILABLE`, defers the check with
/// `not_ready` and the retry it is given, the detail naming the call and the provider's words; a
/// deferral after a `Prepare` releases both sources first, one at the first `Release` prepares
/// nothing. Catches a provider that is still binding its socket taken for a broken release, and
/// a deferral that leaves a placement.
#[test]
fn silence_and_unavailable_defer_the_check() {
    let provider = FakeProvider::default();
    provider.fail(
        "Release",
        0,
        ProviderFault::Silent("connection refused".to_owned()),
    );
    let (outcome, why, retry, detail) = failed(checked(&provider));
    assert_eq!(
        (outcome, why, retry),
        (Outcome::Deferred, Some(Reason::NotReady), Some(5))
    );
    assert_eq!(
        detail,
        format!(
            "the mount provider at {FAKE_SOCKET} does not answer Release yet: connection refused"
        )
    );
    assert_eq!(provider.calls(), ["Release 0", "Release 1"]);

    let provider = FakeProvider::default();
    provider.answer("Mount", 0, ProviderCode::Unavailable, "not ready");
    let (outcome, why, retry, detail) = failed(check(&provider, &shape(), 9));
    assert_eq!(
        (outcome, why, retry),
        (Outcome::Deferred, Some(Reason::ProviderBusy), Some(9))
    );
    assert_eq!(
        detail,
        format!(
            "the mount provider at {FAKE_SOCKET} answered Mount of the tree with its L2 sidecar \
             with UNAVAILABLE: not ready"
        )
    );
    assert!(provider.placed().is_empty());

    let provider = FakeProvider::default();
    provider.fail("Prepare", 1, ProviderFault::Silent("timed out".to_owned()));
    let (outcome, ..) = failed(checked(&provider));
    assert_eq!(outcome, Outcome::Deferred);
    assert!(provider.placed().is_empty());
}

/// What a read of a tree finds.
type Read = Result<BTreeMap<String, TreeNode>, ReadFault>;

/// The tree with its sidecar, changed by `change`.
fn changed(change: impl FnOnce(&mut BTreeMap<String, TreeNode>)) -> BTreeMap<String, TreeNode> {
    let mut nodes = WITH_SIDECAR.nodes();
    change(&mut nodes);
    nodes
}

/// Asserts that a tree read back wrong fails the check with `mounted_tree_wrong`, the detail
/// naming the tree, its directory and the first path that differs, by size and never by its bytes:
/// a file of other bytes, a path missing, one of another kind, a link to another target, a path
/// not in the tree, and a read that fails; and that both sources are released. Catches a check
/// that passes a mount that serves wrong bytes, and a detail that copies a file's bytes into the
/// log.
#[test]
fn a_tree_read_back_wrong_fails_the_check_by_its_first_difference() {
    let dir = FakeProvider::dir_of(0);
    let cases: Vec<(Read, String)> = vec![
        (
            Ok(changed(|nodes| {
                nodes.insert("data/one".to_owned(), TreeNode::File(b"secret".to_vec()));
            })),
            "is not the tree prepared: data/one has other bytes (6 bytes, 64 in the tree)"
                .to_owned(),
        ),
        (
            Ok(changed(|nodes| {
                nodes.remove("variant");
            })),
            "is not the tree prepared: variant is missing".to_owned(),
        ),
        (
            Ok(changed(|nodes| {
                nodes.insert("bin".to_owned(), TreeNode::File(Vec::new()));
            })),
            "is not the tree prepared: bin is a file, a directory in the tree".to_owned(),
        ),
        (
            Ok(changed(|nodes| {
                nodes.insert("tool".to_owned(), TreeNode::Link("/etc".to_owned()));
            })),
            "is not the tree prepared: tool links to \"/etc\", \"bin/tool\" in the tree".to_owned(),
        ),
        (
            Ok(changed(|nodes| {
                nodes.insert("data/empty".to_owned(), TreeNode::Dir);
            })),
            "is not the tree prepared: data/empty is a directory, a file in the tree".to_owned(),
        ),
        (
            Ok(changed(|nodes| {
                nodes.insert("tool".to_owned(), TreeNode::File(Vec::new()));
            })),
            "is not the tree prepared: tool is a file, a link in the tree".to_owned(),
        ),
        (
            Ok(changed(|nodes| {
                nodes.insert("zz".to_owned(), TreeNode::Dir);
            })),
            "is not the tree prepared: zz is not in the tree".to_owned(),
        ),
        (
            Err(ReadFault::NotTheTree("more than 64 paths".to_owned())),
            "is not the tree prepared: more than 64 paths".to_owned(),
        ),
    ];
    for (read, said) in cases {
        let provider = FakeProvider::default();
        provider.read_as(0, read);
        let (outcome, why, _, detail) = failed(checked(&provider));
        assert_eq!(
            (outcome, why),
            (Outcome::Failure, Some(Reason::MountedTreeWrong))
        );
        assert_eq!(
            detail,
            format!("the tree with its L2 sidecar at {} {said}", dir.display())
        );
        assert!(!detail.contains("secret"));
        assert!(provider.placed().is_empty());
    }
}

/// Asserts that a read through the mount that does not end in its bound (the mount fills a file
/// as it is opened, slowly on a node under load) defers the check with `not_ready`, its detail
/// naming the tree, its directory and what was waited for, and releases both sources. Catches a
/// check that fails, and so rolls back, an update of a node whose mount is only slow.
#[test]
fn a_read_that_does_not_end_defers_the_check() {
    let dir = FakeProvider::dir_of(1);
    let why = "the read did not end within 30000 ms";
    let provider = FakeProvider::default();
    provider.read_as(1, Err(ReadFault::Unread(why.to_owned())));
    let (outcome, reason, retry, detail) = failed(checked(&provider));
    assert_eq!(
        (outcome, reason, retry),
        (Outcome::Deferred, Some(Reason::NotReady), Some(5))
    );
    assert_eq!(
        detail,
        format!(
            "the tree without an L2 sidecar at {} could not be read yet: {why}",
            dir.display()
        )
    );
    assert!(provider.placed().is_empty());
}

/// Asserts that a read through the mount that the system answers with an error (a handle the
/// mount no longer knows, a permission, a path that is not there, an I/O error) fails the check at
/// once with `mounted_tree_wrong`, not deferred: its detail names the tree, its directory, the
/// path and what the system said, and both sources are released. The trees' objects are the
/// release's own, read from memory, so asking again would only meet the same answer: a Mac's
/// update looped on such a read, deferred every few seconds, until hostkeeper's deadline. Catches
/// a check that defers, and so holds the update for `verify_deadline_secs`, on a mount that will
/// not serve the tree.
#[test]
fn a_read_the_system_refuses_fails_the_check_at_once_naming_the_path() {
    for (index, form, why) in [
        (
            0,
            "with its L2 sidecar",
            "the directory could not be listed: Stale NFS file handle (os error 70)",
        ),
        (
            1,
            "without an L2 sidecar",
            "data could not be listed: Permission denied (os error 13)",
        ),
        (
            1,
            "without an L2 sidecar",
            "data/one could not be read: Input/output error (os error 5)",
        ),
    ] {
        let provider = FakeProvider::default();
        provider.read_as(index, Err(ReadFault::Failed(why.to_owned())));
        let (outcome, reason, retry, detail) = failed(checked(&provider));
        assert_eq!(
            (outcome, reason, retry),
            (Outcome::Failure, Some(Reason::MountedTreeWrong), None)
        );
        assert_eq!(
            detail,
            format!(
                "the tree {form} at {} could not be read through the mount: {why}",
                FakeProvider::dir_of(index).display()
            )
        );
        assert!(provider.placed().is_empty());
    }
}

/// Asserts that a `Mount` that answers a directory outside the provider's mount, the mount
/// itself, or a path below it that leads out of it by `..`, fails the check with `mounted_tree_wrong` before anything is read, and releases both.
/// Catches a check that has root read whatever directory the provider names.
#[test]
fn a_directory_outside_the_mount_fails_the_check_unread() {
    for dir in [
        "/etc",
        FAKE_MOUNT,
        "/run/tierstore/fsx/a",
        "/run/tierstore/fs/../../etc",
    ] {
        let provider = FakeProvider::default();
        provider.mount_at(1, dir);
        let (outcome, why, _, detail) = failed(checked(&provider));
        assert_eq!(
            (outcome, why),
            (Outcome::Failure, Some(Reason::MountedTreeWrong))
        );
        assert_eq!(
            detail,
            format!(
                "Mount of the tree without an L2 sidecar answered {dir}, which is not below the \
                 mount {FAKE_MOUNT}"
            )
        );
        assert!(!provider.calls().iter().any(|call| call.starts_with("read")));
        assert!(provider.placed().is_empty());
    }
}

/// Asserts that a placement that outlives its release (a `Mount` after it still answers the
/// directory) fails the check with `mounted_tree_wrong`, says so, and asks for the release of both
/// again before it ends; and that any other answer of that `Mount` but `UNAVAILABLE` with
/// `the source is not prepared here` is judged as at any call: `UNAVAILABLE` with another reason
/// and silence defer, a refusal fails with `provider_refused`. Catches a release that leaves the
/// placement to count against `trees.max_placements`, and a provider that dies at the last call
/// taken for a wrong tree.
#[test]
fn a_placement_that_outlives_its_release_fails_the_check() {
    let provider = FakeProvider::default();
    provider.keep_placements();
    let (outcome, why, _, detail) = failed(checked(&provider));
    assert_eq!(
        (outcome, why),
        (Outcome::Failure, Some(Reason::MountedTreeWrong))
    );
    assert_eq!(
        detail,
        format!(
            "the placement of the tree with its L2 sidecar outlived its Release: a Mount after it \
             answered its directory {}",
            FakeProvider::dir_of(0).display()
        )
    );
    let calls = provider.calls();
    assert_eq!(
        calls[calls.len() - 3..],
        [
            "Mount 0".to_owned(),
            "Release 0".to_owned(),
            "Release 1".to_owned()
        ]
    );
    let unavailable = ProviderFault::Answered {
        code: ProviderCode::Unavailable,
        reason: "not ready".to_owned(),
    };
    let refused = ProviderFault::Answered {
        code: ProviderCode::FailedPrecondition,
        reason: "the source was released".to_owned(),
    };
    let silent = ProviderFault::Silent("timed out".to_owned());
    let socket = FAKE_SOCKET;
    let cases = [
        (
            unavailable,
            Outcome::Deferred,
            Reason::ProviderBusy,
            format!(
                "the mount provider at {socket} answered Mount of the tree with its L2 sidecar \
                 with UNAVAILABLE: not ready"
            ),
        ),
        (
            refused,
            Outcome::Failure,
            Reason::ProviderRefused,
            format!(
                "the mount provider at {socket} refused Mount of the tree with its L2 sidecar ({}) \
                 with FailedPrecondition: the source was released",
                WITH_SIDECAR.key()
            ),
        ),
        (
            silent,
            Outcome::Deferred,
            Reason::NotReady,
            format!("the mount provider at {socket} does not answer Mount yet: timed out"),
        ),
    ];
    for (fault, outcome, reason, said) in cases {
        let provider = FakeProvider::default();
        // The first Mount of the source places it; the fault is the answer of the one after the
        // release.
        let gone = GoneFault {
            inner: &provider,
            fault,
        };
        let (got, why, _, detail) = failed(check(&gone, &shape(), 5));
        assert_eq!((got, why), (outcome, Some(reason)));
        assert_eq!(detail, said);
        assert!(provider.placed().is_empty());
    }
}

/// Asserts that a release of the first source that is not answered still asks for the release
/// of the second, at the check's start and after a failure, and that the first failure is the
/// check's answer. Catches a check that leaves the second tree placed behind a first release
/// that failed.
#[test]
fn a_failed_release_of_one_source_still_releases_the_other() {
    let provider = FakeProvider::default();
    provider.answer("Mount", 1, ProviderCode::FailedPrecondition, "no");
    let refusing = FirstReleaseFailsAfterPrepare { inner: &provider };
    let (outcome, why, _, detail) = failed(check(&refusing, &shape(), 5));
    assert_eq!(
        (outcome, why),
        (Outcome::Failure, Some(Reason::ProviderRefused))
    );
    assert!(
        detail.contains("refused Mount of the tree without"),
        "{detail}"
    );
    let calls = provider.calls();
    assert_eq!(calls.last().map(String::as_str), Some("Release 1"));
    assert_eq!(provider.placed(), [0]);
}

/// A provider whose `Release` of source 0 is not answered once a `Prepare` was.
struct FirstReleaseFailsAfterPrepare<'a> {
    inner: &'a FakeProvider,
}

impl super::ports::MountProvider for FirstReleaseFailsAfterPrepare<'_> {
    fn prepare(
        &self,
        socket: &std::path::Path,
        index: u32,
        reference: &str,
    ) -> Result<(), ProviderFault> {
        self.inner.prepare(socket, index, reference)
    }

    fn mount(&self, socket: &std::path::Path, index: u32) -> Result<PathBuf, ProviderFault> {
        self.inner.mount(socket, index)
    }

    fn release(&self, socket: &std::path::Path, index: u32) -> Result<(), ProviderFault> {
        let prepared = self
            .inner
            .calls()
            .iter()
            .any(|call| call.starts_with("Prepare"));
        if prepared && index == 0 {
            return Err(ProviderFault::Silent("reset".to_owned()));
        }
        self.inner.release(socket, index)
    }

    fn read_tree(&self, at: TreeAt<'_>) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
        self.inner.read_tree(at)
    }
}

/// A provider whose second `Mount` of source 0 (the one after the release) is answered with
/// `fault`.
struct GoneFault<'a> {
    inner: &'a FakeProvider,
    fault: ProviderFault,
}

impl super::ports::MountProvider for GoneFault<'_> {
    fn prepare(
        &self,
        socket: &std::path::Path,
        index: u32,
        reference: &str,
    ) -> Result<(), ProviderFault> {
        self.inner.prepare(socket, index, reference)
    }

    fn mount(&self, socket: &std::path::Path, index: u32) -> Result<PathBuf, ProviderFault> {
        let mounts = self
            .inner
            .calls()
            .iter()
            .filter(|call| **call == format!("Mount {index}"))
            .count();
        if index == 0 && mounts == 1 {
            return Err(self.fault.clone());
        }
        self.inner.mount(socket, index)
    }

    fn release(&self, socket: &std::path::Path, index: u32) -> Result<(), ProviderFault> {
        self.inner.release(socket, index)
    }

    fn read_tree(&self, at: TreeAt<'_>) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
        self.inner.read_tree(at)
    }
}

/// Asserts that a refusal of the last `Release` fails the check with `provider_refused`.
#[test]
fn a_refused_last_release_fails_the_check() {
    let provider = FakeProvider::default();
    let refusing = LastReleaseRefused { inner: &provider };
    let (outcome, why, _, detail) = failed(check(&refusing, &shape(), 5));
    assert_eq!(
        (outcome, why),
        (Outcome::Failure, Some(Reason::ProviderRefused))
    );
    assert!(
        detail.contains("refused Release of the tree with its L2 sidecar"),
        "{detail}"
    );
}

/// A provider that refuses a `Release` once both sources were read.
struct LastReleaseRefused<'a> {
    inner: &'a FakeProvider,
}

impl super::ports::MountProvider for LastReleaseRefused<'_> {
    fn prepare(
        &self,
        socket: &std::path::Path,
        index: u32,
        reference: &str,
    ) -> Result<(), ProviderFault> {
        self.inner.prepare(socket, index, reference)
    }

    fn mount(&self, socket: &std::path::Path, index: u32) -> Result<PathBuf, ProviderFault> {
        self.inner.mount(socket, index)
    }

    fn release(&self, socket: &std::path::Path, index: u32) -> Result<(), ProviderFault> {
        let read = self
            .inner
            .calls()
            .iter()
            .any(|call| call.starts_with("read"));
        if read {
            return Err(ProviderFault::Answered {
                code: ProviderCode::FailedPrecondition,
                reason: "no".to_owned(),
            });
        }
        self.inner.release(socket, index)
    }

    fn read_tree(&self, at: TreeAt<'_>) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
        self.inner.read_tree(at)
    }
}

/// The params with the node's file at `config`, or none.
fn params_naming(config: Option<&str>) -> Params {
    let mut text =
        "\"updater.staged_release\" = \"/r\"\n\"updater.target_version\" = \"1\"\n".to_owned();
    if let Some(config) = config {
        text.push_str(&format!("config_file = \"{config}\"\n"));
    }
    Params::parse(&text).expect("params")
}

/// Asserts that the check runs on a node whose file has a `mount-provider` interface, and asks
/// nothing of the provider, and says nothing, on a node whose params name no file, whose file is
/// not there, or whose file has no such interface; that an entry with no socket is
/// `config_refused`; and that a file that cannot be read is the host's failure. Catches a storage
/// node failed for a provider it does not have, and a build node whose check is skipped.
#[test]
fn the_check_runs_on_a_node_with_a_mount_provider_alone() {
    let host = SystemdFake::installed();
    let provider = FakeProvider::default();
    let named = params_naming(Some("/etc/tierstore/config.toml"));
    host.set_config(Some(&format!("[server]\n{PROVIDER_ENTRY}")));
    let said = on_node(&host, &provider, &named, ReadVia::Mount).expect("the check passes");
    assert!(said.starts_with("the mount provider at "), "{said}");
    assert_eq!(provider.calls(), every_call());

    let provider = FakeProvider::default();
    assert_eq!(
        on_node(&host, &provider, &params_naming(None), ReadVia::Mount),
        Ok(String::new())
    );
    for text in [None, Some("[server]\n")] {
        host.set_config(text);
        assert_eq!(
            on_node(&host, &provider, &named, ReadVia::Mount),
            Ok(String::new())
        );
    }
    assert!(provider.calls().is_empty());

    host.set_config(Some("[[interface]]\ntype = \"mount-provider\"\n"));
    let done = on_node(&host, &provider, &named, ReadVia::Mount).expect_err("refused");
    assert_eq!(done.report.reason, Some(Reason::ConfigRefused));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the mount-provider interface has no absolute socket")
    );

    host.fail_on(Some("host.config"));
    let done = on_node(&host, &provider, &named, ReadVia::Mount).expect_err("the host failed");
    assert_eq!(done.report.reason, Some(Reason::HostFailed));
}

/// The calls of a check that passes on a node whose trees are read from the NFS front at `front`.
fn every_call_from(front: &str) -> Vec<String> {
    every_call()
        .into_iter()
        .map(
            |call| match call.strip_prefix(&format!("read {FAKE_MOUNT}/")) {
                Some(below) => format!("read {below} from {front}"),
                None => call,
            },
        )
        .collect()
}

/// Asserts that a Mac's check (`ReadVia::NfsFront`) reads both trees from the NFS front of the node's
/// `fs` interface, at their paths under the mount, and never through the mount: the front its file
/// names (`fs.nfs.listen`), or the loader's default for a file that names none, its detail saying
/// so; that a node with no `fs` interface, and a node checked through the mount (`ReadVia::Mount`, a
/// Linux node, whose `fs` interface serves no NFS), reads through the mount; and that a `listen`
/// that is not an address and a port, or names port 0, is `config_refused`, with nothing asked of
/// the provider. Catches a Mac whose check reads the volume, which macOS refuses the hook's
/// process, and a front the hook cannot reach taken for one it can.
#[test]
fn a_mac_reads_the_trees_from_the_nfs_front_its_file_names() {
    let host = SystemdFake::installed();
    let named = params_naming(Some("/usr/local/tierstore/etc/tierstore.toml"));
    let fs = |params: &str| {
        format!("[server]\n{PROVIDER_ENTRY}\n[[interface]]\ntype = \"fs\"\n{params}")
    };
    for (file, front) in [
        (
            fs("params = { nfs = { listen = \"127.0.0.1:12050\" } }\n"),
            "127.0.0.1:12050",
        ),
        (fs(""), "127.0.0.1:12049"),
        (
            fs("params = { mountpoint = \"/run/tierstore/fs\" }\n"),
            "127.0.0.1:12049",
        ),
    ] {
        host.set_config(Some(&file));
        let provider = FakeProvider::default();
        let said = on_node(&host, &provider, &named, ReadVia::NfsFront).expect("the check passes");
        assert_eq!(
            said,
            format!(
                "the mount provider at {FAKE_SOCKET} placed, served and released a tree with its \
                 L2 sidecar and one without an L2 sidecar, read from the NFS front at {front}"
            )
        );
        assert_eq!(provider.calls(), every_call_from(front));
    }

    for (file, read) in [
        (format!("[server]\n{PROVIDER_ENTRY}"), ReadVia::NfsFront),
        (fs(""), ReadVia::Mount),
    ] {
        host.set_config(Some(&file));
        let provider = FakeProvider::default();
        on_node(&host, &provider, &named, read).expect("the check passes");
        assert_eq!(provider.calls(), every_call());
    }

    for (listen, shown) in [
        ("\"nowhere\"", "\"nowhere\""),
        ("\"127.0.0.1:0\"", "\"127.0.0.1:0\""),
        ("5", "5"),
    ] {
        host.set_config(Some(&fs(&format!(
            "params = {{ nfs = {{ listen = {listen} }} }}\n"
        ))));
        let provider = FakeProvider::default();
        let done = on_node(&host, &provider, &named, ReadVia::NfsFront).expect_err("refused");
        assert_eq!(done.report.reason, Some(Reason::ConfigRefused));
        assert_eq!(
            done.report.detail,
            Some(format!(
                "the fs interface's nfs.listen, {shown}, is no address and port the hook can reach"
            ))
        );
        assert!(provider.calls().is_empty());
    }
}

/// Asserts that a read from the NFS front that the front answers with an error fails the check at
/// once with `mounted_tree_wrong`, its detail naming the tree, its directory, the front and what
/// the front said, and that a read from it that does not end defers, as through the mount.
/// Catches a Mac's check that defers until hostkeeper's deadline on a front that will not serve
/// the tree.
#[test]
fn a_read_from_the_front_fails_at_once_or_defers_as_through_the_mount() {
    let mut shape = shape();
    let front: std::net::SocketAddr = "127.0.0.1:12049".parse().unwrap();
    shape.nfs = Some(front);
    let why = "mount-provider/check/0 could not be looked up: the NFS front at 127.0.0.1:12049 \
               answered NFS3ERR_STALE (70)";
    let provider = FakeProvider::default();
    provider.read_as(0, Err(ReadFault::Failed(why.to_owned())));
    let (outcome, reason, retry, detail) = failed(check(&provider, &shape, 5));
    assert_eq!(
        (outcome, reason, retry),
        (Outcome::Failure, Some(Reason::MountedTreeWrong), None)
    );
    assert_eq!(
        detail,
        format!(
            "the tree with its L2 sidecar at {} could not be read from the NFS front at {front}: \
             {why}",
            FakeProvider::dir_of(0).display()
        )
    );
    assert!(provider.placed().is_empty());

    let provider = FakeProvider::default();
    let slow = "the NFS front at 127.0.0.1:12049 did not answer READ of data/one: timed out";
    provider.read_as(1, Err(ReadFault::Unread(slow.to_owned())));
    let (outcome, reason, retry, _) = failed(check(&provider, &shape, 5));
    assert_eq!(
        (outcome, reason, retry),
        (Outcome::Deferred, Some(Reason::NotReady), Some(5))
    );
}

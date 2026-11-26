//! The update of a node of the one-process layout, seen from its jobs (komira-ai/tierstore#479):
//! while the first process still drains a request, the second is already the cache's writer and
//! answers the mount provider, so that the node's jobs neither wait for that drain to fill their
//! trees nor find no provider. Over the rig of `interface_tests.rs`.
//!
//! The first process's drain is held by a HEAD of the S3 front that waits at its upstream: a
//! request that writes nothing to the cache, so the first process's `written` does not wait for it,
//! and a drain that does not end until the test lets it go.

use std::os::fd::AsRawFd as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use objstore_s3::{ObjectKey, ObjectStore as _};
use tierstore_trees_api::provider::v1::{
    MountRequest, MountResponse, PrepareRequest, PrepareResponse,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{FsNode, Held, Started, assert_answers, left_state, memory, platform, start_on, stop};
use crate::server::Drained;
use crate::server::cache_cli_tests::{QUICK, client, probe};
use crate::server::cache_handoff_lock_tests::{Pin, pin};
use crate::server::cli::Addresses;
use crate::server::config::{PerRun, settings_from_text};
use crate::server::config_cache::CacheSettings;
use crate::server::handoff::testing::within;
use crate::server::local_cache::pack_testing::Pack;
use crate::server::provider::testing::{Client, READ_ONLY, Rig, list_of, source};
use crate::server::treefs::fakedev::{self, FakeDevice};
use crate::server::treefs::testing::FakeManager;
use crate::{Backend, MemoryBackend, tree_list_key};

/// The slots of the two processes, which tell their answers apart on the sockets they share.
const FIRST: u16 = 9711;
const SECOND: u16 = 9712;

/// The knobs of a run whose drain waits for its requests in flight as long as a test lasts: the
/// first process ends only once the test lets its request go.
const HELD: PerRun = PerRun {
    drain_deadline: Some(Duration::from_secs(3600)),
    ..QUICK
};

/// The settings of a process of `node` with `slot`, handing over on `control`, whose S3 front
/// listens on `listen`, with a `mount-provider` interface on `provider` (its mount point left out:
/// the `fs` interface's) when one is given.
fn settings(
    node: &FsNode,
    (control, slot): (&std::path::Path, u16),
    listen: &str,
    provider: Option<&std::path::Path>,
) -> CacheSettings {
    let server = format!(
        "handoff_socket = {:?}\nhandoff_wait = \"30s\"\nslot = {slot}",
        control.display().to_string()
    );
    let interface = provider.map_or_else(String::new, |socket| {
        format!(
            "\n[[interface]]\ntype   = \"mount-provider\"\nparams = {{ socket = {:?} }}\n",
            socket.display().to_string()
        )
    });
    let text = node.text_with(&server, "", listen, &interface);
    let mut settings = settings_from_text(&text, HELD)
        .unwrap()
        .into_cache()
        .unwrap();
    // A mount the test makes is its user's alone: `allow_other` needs the system's consent.
    if let Some(fs) = &mut settings.fs {
        fs.allow_other = false;
    }
    settings
}

/// Says on standard output which step a test begins: a test that hangs shows where.
fn step(what: &str) {
    println!("step: {what}");
}

/// The `/readyz` answer of the process behind `pin`, once it is `want`: the pin's connections go
/// to that process alone, and each probe is a connection of its own.
async fn readyz_of(pin: &Pin, want: &str) {
    loop {
        let (status, text) = probe(pin.at, "/readyz").await;
        if (status, text.as_str()) == (200, want) {
            return;
        }
        tokio::task::yield_now().await;
    }
}

/// The first process of an update, serving the mount and its S3 front, with a HEAD on it held at
/// its upstream: its drain goes on until [`Draining::let_go`].
struct Draining {
    node: FsNode,
    control: PathBuf,
    first: Started,
    first_bound: Addresses,
    kernel: FakeDevice,
    stored: std::os::fd::OwnedFd,
    held: Arc<Held>,
    to_first: Pin,
    heading: JoinHandle<Result<Option<u64>, objstore_s3::ObjectStoreError>>,
    _manager: FakeManager,
}

impl Draining {
    /// Starts the first process of `node` (with the provider at `provider`, if any), and its HEAD of
    /// `cas/held`, which waits at the upstream.
    async fn start(name: &str, provider: Option<&std::path::Path>) -> Self {
        let node = FsNode::new(name);
        let control = node.dir.path().join("control.sock");
        let provider = provider.map(|file| node.dir.path().join(file));
        let first_settings = settings(&node, (&control, FIRST), "127.0.0.1:0", provider.as_deref());
        left_state(first_settings.fs.as_ref().unwrap());
        let held = Arc::new(Held {
            inner: MemoryBackend::new(),
            armed: AtomicBool::new(false),
            reached: Notify::new(),
            let_go: Notify::new(),
        });
        held.inner
            .put("cas/held", Bytes::from_static(b"held"))
            .await
            .unwrap();
        let (stored, kernel) = fakedev::pair();
        let manager = FakeManager::start(&node.dir.path().join("first.sock"));
        let on = platform(&manager, Some(stored.as_raw_fd()), node.kernel(false));
        let upstream: Arc<dyn Backend> = held.clone();
        let mut first = start_on(first_settings, upstream, on);
        step("the first serves");
        let first_bound = within("the first serves", &mut first.bound).await.unwrap();
        step("the first answers the kernel");
        assert_answers(&kernel, 1);
        step("the pin to the first");
        let to_first = pin(first_bound.s3, FIRST).await;
        held.armed.store(true, Ordering::SeqCst);
        let at = to_first.at;
        let heading =
            tokio::spawn(async move { client(at).len(&ObjectKey::new("cas/held").unwrap()).await });
        step("the head reaches the upstream");
        within("the head reaches the upstream", held.reached.notified()).await;
        Self {
            node,
            control,
            first,
            first_bound,
            kernel,
            stored,
            held,
            to_first,
            heading,
            _manager: manager,
        }
    }

    /// Starts the second process on the same file over `upstream`, which takes over from the first,
    /// and gives it once it serves, with a pin to it.
    async fn take_over(
        &self,
        upstream: Arc<dyn Backend>,
        provider: Option<&std::path::Path>,
    ) -> (Started, Pin, FakeManager) {
        let provider = provider.map(|file| self.node.dir.path().join(file));
        let listen = self.first_bound.s3.to_string();
        let second_settings = settings(
            &self.node,
            (&self.control, SECOND),
            &listen,
            provider.as_deref(),
        );
        let manager = FakeManager::start(&self.node.dir.path().join("second.sock"));
        let on = platform(&manager, None, self.node.kernel(false));
        let mut second = start_on(second_settings, upstream, on);
        step("the second serves");
        let bound = within("the second serves", &mut second.bound)
            .await
            .unwrap();
        assert_eq!(bound.s3, self.first_bound.s3);
        step("the pin to the second");
        let to_second = pin(self.first_bound.s3, SECOND).await;
        (second, to_second, manager)
    }

    /// Asserts that the first process still drains: it has not ended, and its HEAD is in flight.
    fn still_draining(&self) {
        assert!(!self.first.ended.is_finished(), "the first still drains");
        assert!(!self.heading.is_finished(), "its request is in flight");
    }

    /// Lets the HEAD go, and asserts that it is answered and that the first process then ends: the
    /// node's directory and the descriptor its first store kept, which the second process still
    /// uses.
    async fn let_go(self) -> (FsNode, std::os::fd::OwnedFd) {
        self.held.let_go.notify_one();
        step("the head ends");
        let answered = within("the head ends", self.heading).await.unwrap();
        assert_eq!(answered.unwrap(), Some(4));
        drop(self.to_first);
        step("the first ends");
        let drained = within("the first ends", self.first.ended)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(drained, Drained { abandoned: 0 });
        (self.node, self.stored)
    }
}

/// Asserts that the second process of an update is the cache's writer while the first still
/// drains a request: its `/readyz` says `ready read-write` while the first has not ended and its
/// request is in flight, and the mount is served meanwhile (by the second). Catches the update in
/// which the second process asks for the sockets alone and stays read-only until the first has
/// drained and exited, so that the jobs' fills through the mount wait for that drain and fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_second_process_is_the_writer_while_the_first_still_drains() {
    let draining = Draining::start("tsfs-one-writer", None).await;
    let (second, to_second, _manager) = draining.take_over(memory(), None).await;
    step("the second takes the write role");
    within(
        "the second takes the write role",
        readyz_of(&to_second, "ready read-write"),
    )
    .await;
    draining.still_draining();
    step("the second answers the kernel");
    assert_answers(&draining.kernel, 2);
    let _kept = draining.let_go().await;
    within(
        "the second is still the writer",
        readyz_of(&to_second, "ready read-write"),
    )
    .await;
    drop(to_second);
    step("the second stops");
    stop(second).await.unwrap();
}

/// Asserts that a job agent's Prepare and Mount, made on the mount provider's socket while the first
/// process of an update still drains a request, are answered by the second process: the source's
/// tree is placed from the list in the second's upstream (the first's has no such list, so only the
/// second can answer it so) and the Mount names its directory under the mount, while the first has
/// not ended and its request is in flight. Calls that meet no socket, or the first process's, are
/// asked again, as the agent does. Catches the update in which the provider's socket is bound by no
/// one from the first process's hand-over until its exit, so that the agent's Mount waits out its
/// `--mount-provider-wait` and the job fails `input-unavailable`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_mount_during_the_hand_over_is_answered_by_the_second_process() {
    let draining = Draining::start("tsfs-one-provider", Some("provider.sock".as_ref())).await;
    let socket = draining.node.dir.path().join("provider.sock");
    step("the first binds the provider");
    crate::server::local_cache::testing::until("the first binds the provider", || socket.exists())
        .await;
    let upstream = MemoryBackend::new();
    let pack = Pack::new("cas/pack/one", &[1000, 2000, 3000]);
    upstream
        .put(&pack.key, Bytes::copy_from_slice(pack.bytes()))
        .await
        .unwrap();
    let list = list_of(&pack).encode();
    let key = tree_list_key(&list);
    upstream.put(&key, Bytes::from(list)).await.unwrap();
    let (second, to_second, _manager) = draining
        .take_over(Arc::new(upstream), Some("provider.sock".as_ref()))
        .await;
    step("the provider mounts the source");
    let mounted = within("the provider mounts the source", mount(&socket, &key)).await;
    let place = Rig::place_of("js.1", 0);
    assert_eq!(
        mounted,
        draining.node.mountpoint().join(place).to_string_lossy()
    );
    draining.still_draining();
    let _kept = draining.let_go().await;
    drop(to_second);
    step("the second stops");
    stop(second).await.unwrap();
    assert!(!socket.exists());
}

/// Prepares and mounts the source `js.1` of the list `key` on the provider's socket at `socket`
/// until a Mount answers it, as a job agent asks again when it finds no socket or the call fails:
/// the directory the Mount names.
async fn mount(socket: &std::path::Path, key: &str) -> String {
    loop {
        if let Some(path) = mount_once(socket, key).await {
            return path;
        }
        tokio::task::yield_now().await;
    }
}

/// One Prepare and Mount of `js.1` on a connection of its own: the directory, or none when either
/// is not answered or not done.
async fn mount_once(socket: &std::path::Path, key: &str) -> Option<String> {
    let mut client = Client::try_connect(socket).await?;
    let prepare = PrepareRequest {
        source: source("js.1", 0),
        r#ref: key.to_owned(),
        mode: READ_ONLY,
        host: String::new(),
    };
    let _: PrepareResponse = client.try_call("Prepare", &prepare).await?.ok()?;
    let mount = MountRequest {
        source: source("js.1", 0),
    };
    let mounted: MountResponse = client.try_call("Mount", &mount).await?.ok()?;
    Some(mounted.path)
}

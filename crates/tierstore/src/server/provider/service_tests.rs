//! The provider on its socket: the four calls and a refusal over gRPC, the codes and the bound of a
//! refusal's reason, and when the socket is bound, rebound and removed.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tierstore_trees_api::provider::v1::{
    MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest, ReleaseResponse,
    SweepRequest, SweepResponse,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tonic::Code;

use super::service::status_of;
use super::testing::{AGENT, Client, READ_ONLY, Rig, source};
use super::{MAX_REASON_BYTES, Provider, ProviderError, serve};
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::Tick;
use crate::server::local_cache::testing::{until, wait_advancing};

/// The directory of a socket, and the path of the socket in it.
struct Place {
    _dir: TestDir,
    path: PathBuf,
}

impl Place {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let path = dir.path().join("provider.sock");
        Self { _dir: dir, path }
    }
}

/// The provider served on a socket in a task, and the means to stop it.
struct Served {
    halt: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Served {
    fn start(provider: &Provider, cache: &LocalCache, path: &Path) -> Self {
        let halt = Arc::new(Notify::new());
        let (provider, cache, path) = (provider.clone(), cache.clone(), path.to_owned());
        let notify = Arc::clone(&halt);
        let task = tokio::spawn(async move { serve(provider, &cache, &path, notify).await });
        Self { halt, task }
    }

    async fn stop(self) {
        self.halt.notify_one();
        self.task.await.unwrap();
    }
}

/// Asserts the code of each answer that is not a success, and that a reason over the contract's
/// bound is cut to it at a character boundary.
#[test]
fn a_refusal_is_its_code_and_a_bounded_reason() {
    let invalid = status_of(ProviderError::Invalid("bad".into()));
    assert_eq!(
        (invalid.code(), invalid.message()),
        (Code::InvalidArgument, "bad")
    );
    let refused = status_of(ProviderError::Refused("no".into()));
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "no")
    );
    let later = status_of(ProviderError::Unavailable("later".into()));
    assert_eq!(
        (later.code(), later.message()),
        (Code::Unavailable, "later")
    );
    assert_eq!(MAX_REASON_BYTES, 256);
    let exact = "x".repeat(MAX_REASON_BYTES);
    assert_eq!(
        status_of(ProviderError::Refused(exact.clone())).message(),
        exact
    );
    let long = format!("a{}", "é".repeat(200));
    let cut = status_of(ProviderError::Refused(long.clone()));
    assert_eq!(cut.message().len(), 255);
    assert!(long.starts_with(cut.message()));
}

/// Asserts the four calls over the socket, as the agent makes them: a prepare, a mount that answers
/// the directory, a sweep that keeps the run, and a release; and a refusal with its code and
/// reason. The socket is mode 0600, and is gone when the provider stops.
#[tokio::test]
async fn the_four_calls_are_answered_over_the_socket() {
    let rig = Rig::new("prov-socket").await;
    let place = Place::new("prov-socket");
    let served = Served::start(&rig.provider, &rig.bench.rig.cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    let mode = std::fs::metadata(&place.path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let prepare = PrepareRequest {
        source: source("js.1", 0),
        r#ref: rig.key.clone(),
        mode: READ_ONLY,
        host: "host-1".into(),
    };
    let prepared: PrepareResponse = client.call("Prepare", &prepare).await.unwrap();
    assert_eq!(prepared, PrepareResponse {});
    let mount = MountRequest {
        source: source("js.1", 0),
    };
    let mounted: MountResponse = client.call("Mount", &mount).await.unwrap();
    assert_eq!(
        mounted.path,
        Rig::path_of("js.1", 0).to_string_lossy().into_owned()
    );
    let sweep = SweepRequest {
        agent: AGENT.into(),
        keep: vec!["js.1".into()],
    };
    let swept: SweepResponse = client.call("Sweep", &sweep).await.unwrap();
    assert_eq!(swept.released, 0);
    let release = ReleaseRequest {
        source: source("js.1", 0),
    };
    let released: ReleaseResponse = client.call("Release", &release).await.unwrap();
    assert_eq!(released, ReleaseResponse {});
    assert!(rig.bench.store.mount_paths().is_empty());
    let bad = PrepareRequest {
        r#ref: "cas/l/short".into(),
        ..prepare
    };
    let refused = client
        .call::<PrepareResponse>("Prepare", &bad)
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(
        refused.message(),
        "the ref is not the key of a tree list: cas/l/ and 64 lowercase hex digits"
    );
    let mount_unprepared = MountRequest {
        source: source("js.9", 0),
    };
    let refused = client
        .call::<MountResponse>("Mount", &mount_unprepared)
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::Unavailable, "the source is not prepared here")
    );
    let release_none = ReleaseRequest { source: None };
    let refused = client
        .call::<ReleaseResponse>("Release", &release_none)
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "no source")
    );
    let sweep_none = SweepRequest {
        agent: String::new(),
        keep: Vec::new(),
    };
    let refused = client
        .call::<SweepResponse>("Sweep", &sweep_none)
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "the agent is 1 to 120 bytes")
    );
    // A connection that closes is reaped, and the next is served.
    drop(client);
    let mut next = Client::connect(&place.path).await;
    let swept: SweepResponse = next.call("Sweep", &sweep).await.unwrap();
    assert_eq!(swept.released, 0);
    served.stop().await;
    assert!(!place.path.exists());
}

/// Asserts that an instance whose cache does not hold the lock of its directory binds no socket
/// (the instance that holds it answers), and binds it in the tick in which it takes the lock.
#[tokio::test]
async fn a_read_only_instance_binds_nothing_until_it_takes_the_lock() {
    let rig = Rig::new("prov-socket-wait").await;
    let place = Place::new("prov-socket-wait");
    let holder = rig.bench.rig.locked();
    let waiting = rig.bench.rig.locked();
    let served = Served::start(&rig.provider, &waiting, &place.path);
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(!place.path.exists());
    drop(holder);
    assert_eq!(waiting.tick().unwrap(), Tick::Took);
    let mut client = Client::connect(&place.path).await;
    let sweep = SweepRequest {
        agent: AGENT.into(),
        keep: Vec::new(),
    };
    let swept: SweepResponse = client.call("Sweep", &sweep).await.unwrap();
    assert_eq!(swept.released, 0);
    served.stop().await;
}

/// Asserts that a socket that cannot be bound (its directory is not there) is tried again after a
/// pause, and is bound when the directory is made; and that a halt stops the reads of files the
/// provider had not begun. The clock is paused and moves when every task waits.
#[tokio::test(start_paused = true)]
async fn a_socket_that_cannot_be_bound_is_tried_again() {
    let rig = Rig::new("prov-socket-rebind").await;
    let place = Place::new("prov-socket-rebind");
    let dir = place.path.with_file_name("later");
    let path = dir.join("provider.sock");
    let served = Served::start(&rig.provider, &rig.bench.rig.cache, &path);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(!path.exists());
    std::fs::create_dir(&dir).unwrap();
    wait_advancing(
        "the socket was tried again and bound",
        Duration::from_secs(1),
        || path.exists(),
    )
    .await;
    served.stop().await;
    assert!(rig.provider.shared.fills.is_closed());
    until("the socket is gone", || !path.exists()).await;
}

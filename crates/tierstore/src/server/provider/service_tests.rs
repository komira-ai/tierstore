//! The provider on its socket: the four calls and a refusal over gRPC, the codes and the bound of a
//! refusal's reason, a tree with no L2 sidecar mounted from its list, and when the socket is bound,
//! rebound and removed.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tierstore_trees_api::provider::v1::{
    KeptAttempt, MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest,
    ReleaseResponse, SweepRequest, SweepResponse,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tonic::Code;

use super::service::status_of;
use super::testing::{AGENT, Client, READ_ONLY, Rig, attempt_of, member, source};
use super::{MAX_REASON_BYTES, Provider, ProviderError, serve};
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::Tick;
use crate::server::local_cache::testing::{until, wait_advancing};
use crate::{Op, TreeList};

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

/// Asserts that the attempt of a source and the runs a sweep keeps from an attempt on reach the
/// provider over the socket: two attempts of one source are prepared and mounted at their own
/// directories, a late release of the first leaves the second placed, and a sweep that keeps the
/// run from attempt 3 releases the second.
#[tokio::test]
async fn attempts_are_answered_over_the_socket() {
    let rig = Rig::new("prov-socket-attempt").await;
    let place = Place::new("prov-socket-attempt");
    let served = Served::start(&rig.provider, &rig.bench.rig.cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    for attempt in [1, 2] {
        let prepare = PrepareRequest {
            source: attempt_of("js.1", 0, attempt),
            r#ref: rig.key.clone(),
            mode: READ_ONLY,
            host: "host-1".into(),
        };
        let _: PrepareResponse = client.call("Prepare", &prepare).await.unwrap();
        let mount = MountRequest {
            source: attempt_of("js.1", 0, attempt),
        };
        let mounted: MountResponse = client.call("Mount", &mount).await.unwrap();
        let expected =
            Path::new(super::testing::MOUNT).join(Rig::attempt_place_of("js.1", 0, attempt));
        assert_eq!(mounted.path, expected.to_string_lossy());
    }
    let release = ReleaseRequest {
        source: attempt_of("js.1", 0, 1),
    };
    let _: ReleaseResponse = client.call("Release", &release).await.unwrap();
    let placed = vec![Rig::attempt_place_of("js.1", 0, 2)];
    assert_eq!(rig.bench.store.mount_paths(), placed);
    let sweep = SweepRequest {
        agent: AGENT.into(),
        keep: vec!["js.1".into()],
        keep_attempts: vec![KeptAttempt {
            run: "js.1".into(),
            attempt: 3,
        }],
    };
    let swept: SweepResponse = client.call("Sweep", &sweep).await.unwrap();
    assert_eq!(swept.released, 1);
    assert!(rig.bench.store.mount_paths().is_empty());
    served.stop().await;
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
        keep_attempts: Vec::new(),
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
        keep_attempts: Vec::new(),
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

/// Asserts that a tree whose version 2 list is in the store but whose L2 sidecar (`cas/l2/<id>`) is
/// not is mounted over the socket from its list: `Mount` answers the source's directory, and a
/// `Mount` asked again answers the same; the list is read once, and the preparation is counted
/// once as a missing sidecar and once as a tree placed from its list, and as no refusal.
#[tokio::test]
async fn a_tree_with_no_sidecar_is_mounted_from_its_list_and_counted() {
    let rig = Rig::new("prov-no-sidecar").await;
    let place = Place::new("prov-no-sidecar");
    let served = Served::start(&rig.provider, &rig.bench.rig.cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    let list = TreeList {
        entries: vec![member(&rig.bench.pack, "only", 0, 0o644)],
    };
    let key = rig.put_bytes(&list.encode_v2().unwrap()).await;
    let prepare = PrepareRequest {
        source: source("js.1", 0),
        r#ref: key.clone(),
        mode: READ_ONLY,
        host: "host-1".into(),
    };
    let prepared: PrepareResponse = client.call("Prepare", &prepare).await.unwrap();
    assert_eq!(prepared, PrepareResponse {});
    let mount = MountRequest {
        source: source("js.1", 0),
    };
    for _ in 0..2 {
        let mounted: MountResponse = client.call("Mount", &mount).await.unwrap();
        assert_eq!(
            mounted.path,
            Rig::path_of("js.1", 0).to_string_lossy().into_owned()
        );
    }
    let metricz = rig.bench.rig.cache.metrics();
    for counted in [
        "tierstore_list_refused_total{reason=\"missing_sidecar\"} 0\n",
        "tierstore_list_refused_total{reason=\"v1_list\"} 0\n",
        "tierstore_l2_sidecar_missing_total 1\n",
        "tierstore_l2_sidecar_fallback_total 1\n",
    ] {
        assert!(metricz.contains(counted), "{counted} in {metricz}");
    }
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_some()
    );
    assert_eq!(rig.bench.rig.calls_on(Op::Get, &key), 1);
    served.stop().await;
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
        keep_attempts: Vec::new(),
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

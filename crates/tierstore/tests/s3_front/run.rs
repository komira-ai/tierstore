//! `tierstore::server::cli::run` in this process: it serves what it is asked, until it is
//! stopped, and then drains.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use objstore_s3::s3::{Credentials, S3Config, S3Store};
use objstore_s3::{ByteRange, Capabilities, ObjectKey, ObjectStore};
use tierstore::server::Drained;
use tierstore::server::cli::{Cli, run};
use tokio::sync::oneshot;

use crate::files::{BIN_KEY_ID, BIN_SECRET, Files, Tables};

fn store(addr: SocketAddr) -> S3Store {
    let config = S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: "tier-bucket".to_owned(),
        credentials: Credentials::new(BIN_KEY_ID, BIN_SECRET),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    S3Store::new(config).unwrap()
}

/// The settings of a start with these tables, with a drain that waits no longer than it must.
fn settings(files: &Files, tables: &Tables) -> Cli {
    let mut cli = Cli::from_config_text(&files.text(tables)).expect("a file");
    cli.drain_lb_wait = Duration::from_millis(50);
    cli
}

/// Runs the server over these settings, calls `work` with a client of it, then stops it and gives
/// how the drain ended.
async fn serving<F, Fut>(cli: Cli, work: F) -> Drained
where
    F: FnOnce(S3Store) -> Fut,
    Fut: Future<Output = ()>,
{
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = run(
        &cli,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        work(store(addr.await.unwrap())).await;
        stop.send(()).unwrap();
    };
    let (drained, ()) = tokio::join!(running, driving);
    drained.unwrap()
}

fn kept_key() -> ObjectKey {
    ObjectKey::new("cas/kept").unwrap()
}

/// Asserts that tiers that are directories keep what they are given: an object put through a
/// server is a file in the hot tier's directory, and a server started again over the same
/// directories (a restart) reads it.
#[tokio::test]
async fn directory_tiers_keep_their_objects_across_a_restart() {
    let files = Files::new("run-dirs");
    let root = files.key.parent().unwrap().to_path_buf();
    let tables = Tables {
        hot: format!(
            "kind = \"dir\"\npath = {:?}\nbudget = 1048576",
            root.join("hot").display().to_string()
        ),
        cold: format!(
            "kind = \"dir\"\npath = {:?}",
            root.join("cold").display().to_string()
        ),
        ..Tables::default()
    };
    let cli = settings(&files, &tables);
    let first = serving(cli.clone(), |store| async move {
        let body = Bytes::from_static(b"kept on disk");
        store.put_new(&kept_key(), body, None).await.unwrap();
    })
    .await;
    assert_eq!(first, Drained { abandoned: 0 });
    assert!(root.join("hot/cas~kept").is_file());
    let second = serving(cli, |store| async move {
        let first = ByteRange::new(0, 4).unwrap();
        assert_eq!(store.get_range(&kept_key(), first).await.unwrap(), "kept");
    })
    .await;
    assert_eq!(second, Drained { abandoned: 0 });
}

/// Asserts that `run` binds the address, tells it to `ready` before it takes a request, serves
/// the S3 calls of the key in the key file, and returns, when its stop future resolves, a drain
/// with nothing cut.
#[tokio::test]
async fn run_serves_until_it_is_stopped() {
    let files = Files::new("run-serve");
    let cli = settings(&files, &Tables::default());
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = run(
        &cli,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        let store = store(addr.await.unwrap());
        let k = ObjectKey::new("cas/run").unwrap();
        let body = Bytes::from_static(b"in process");
        store.put_new(&k, body, None).await.unwrap();
        let first = ByteRange::new(0, 2).unwrap();
        assert_eq!(store.get_range(&k, first).await.unwrap(), "in");
        stop.send(()).unwrap();
    };
    let (drained, ()) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
}

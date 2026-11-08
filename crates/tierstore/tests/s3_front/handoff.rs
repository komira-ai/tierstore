//! The live update as `run_handing` runs it: a new instance asks the running one, over a control
//! socket in a scratch directory, for its S3 and OCI sockets, and the running one drains. The
//! control socket's directory is [`socket_dir`]'s, not the key file's under the build's scratch
//! root, whose path in a deep checkout is too long for a Unix socket to bind.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::Client;
use tierstore::server::Drained;
use tierstore::server::cli::{Addresses, Cli, run_handing};
use tierstore::server::handoff::Handoff;
use tokio::sync::oneshot;

use crate::files::{Files, Tables, oci_interface, socket_dir};

/// A client that keeps no connection after an answer, so that each call is a new connection.
fn client() -> Client {
    Client::builder().pool_max_idle_per_host(0).build().unwrap()
}

/// The settings of a start with these tables, with a drain that waits no longer than it must.
fn cli(files: &Files, tables: &Tables) -> Cli {
    let mut cli = Cli::from_config_text(&files.text(tables)).expect("a file");
    cli.drain_lb_wait = Duration::from_millis(50);
    cli
}

/// Asserts that `/healthz` of the S3 front and `/v2/` of the registry answer 200 at `bound`.
async fn assert_served(client: &Client, bound: &Addresses) {
    let s3 = client
        .get(format!("http://{}/healthz", bound.s3))
        .send()
        .await
        .unwrap();
    assert_eq!(s3.status(), 200, "S3 at {}", bound.s3);
    let oci_at = bound.oci.expect("an OCI front");
    let oci = client
        .get(format!("http://{oci_at}/v2/"))
        .send()
        .await
        .unwrap();
    assert_eq!(oci.status(), 200, "OCI at {oci_at}");
}

/// Asserts the live update through `run_handing`: the second instance takes the S3 and the OCI
/// socket of the first (it says the same addresses to `ready`, though its settings name port 0),
/// every call to either front that is made from before the take to the end of the first
/// instance's drain is answered 200, the first ends with nothing cut once the second has the
/// sockets, the second answers the control socket in its place, and a stop of the second ends it
/// with nothing cut.
#[tokio::test]
async fn a_new_instance_takes_both_fronts_and_the_old_one_drains() {
    let files = Files::new("handoff-run");
    let (_control, socket) = socket_dir("handoff-run", "control.sock");
    let setup = Handoff {
        socket: socket.clone(),
        wait: Duration::from_secs(3600),
    };
    let tables = Tables {
        more: oci_interface(&["127.0.0.1:0"], ""),
        ..Tables::default()
    };
    let cli = cli(&files, &tables);
    let (old_told, old_bound) = oneshot::channel::<Addresses>();
    let (new_told, new_bound) = oneshot::channel::<Addresses>();
    let (go, going) = oneshot::channel::<()>();
    let (stop_old, stopped_old) = oneshot::channel::<()>();
    let (stop_new, stopped_new) = oneshot::channel::<()>();
    let old_ended = AtomicBool::new(false);
    let old = async {
        let ready = move |bound| drop(old_told.send(bound));
        let stopped = async move {
            let _ = stopped_old.await;
        };
        let drained = run_handing(&cli, Some(&setup), stopped, ready).await;
        old_ended.store(true, Ordering::SeqCst);
        drained
    };
    let new = async {
        going.await.unwrap();
        let ready = move |bound| drop(new_told.send(bound));
        let stopped = async move {
            let _ = stopped_new.await;
        };
        run_handing(&cli, Some(&setup), stopped, ready).await
    };
    let driving = async {
        let old_at = old_bound.await.unwrap();
        let client = client();
        assert_served(&client, &old_at).await;
        go.send(()).unwrap();
        while !old_ended.load(Ordering::SeqCst) {
            assert_served(&client, &old_at).await;
        }
        let new_at = new_bound.await.unwrap();
        assert_eq!(
            new_at, old_at,
            "the second instance serves the very sockets"
        );
        for _ in 0..5 {
            assert_served(&client, &new_at).await;
        }
        assert!(
            socket.exists(),
            "the second instance answers the control socket"
        );
        stop_new.send(()).unwrap();
        drop(stop_old);
    };
    let (old, new, ()) = tokio::join!(old, new, driving);
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
    assert!(
        !socket.exists(),
        "an instance that stops without a successor removes the socket"
    );
}

/// Asserts that settings that name another address than the running instance serves are refused
/// with the exact words of the hand-off error, and that the running instance goes on serving and
/// then stops on its own stop, with nothing cut.
#[tokio::test]
async fn settings_that_name_other_addresses_are_refused_and_the_old_one_goes_on() {
    let files = Files::new("handoff-refused");
    let (_control, socket) = socket_dir("handoff-refused", "control.sock");
    let setup = Handoff {
        socket,
        wait: Duration::from_secs(3600),
    };
    let old_cli = cli(&files, &Tables::default());
    let other = Tables {
        listen: vec!["127.0.0.2:0".to_owned()],
        ..Tables::default()
    };
    let new_cli = cli(&files, &other);
    let (told, bound) = oneshot::channel::<Addresses>();
    let (stop, stopped) = oneshot::channel::<()>();
    let old = run_handing(
        &old_cli,
        Some(&setup),
        async move {
            let _ = stopped.await;
        },
        move |at| drop(told.send(at)),
    );
    let driving = async {
        let at = bound.await.unwrap();
        let refused = run_handing(&new_cli, Some(&setup), async {}, |_| ()).await;
        let message = refused.expect_err("refused").to_string();
        let expected = format!(
            "hand-off: the running instance listens on [\"s3 {}\"], \
             these settings name [\"s3 127.0.0.2:0\"]",
            at.s3
        );
        assert_eq!(message, expected);
        let answer = client()
            .get(format!("http://{}/healthz", at.s3))
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 200);
        stop.send(()).unwrap();
    };
    let (drained, ()) = tokio::join!(old, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
}

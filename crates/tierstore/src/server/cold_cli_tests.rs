//! A node over the cold tier alone: what it serves, the hand-off a storage node makes to it, and
//! the refusals of its start.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use objstore_s3::{ByteRange, ObjectKey, ObjectStore as _};
use tierstore_testkit::dir::TestDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use super::{announce_line, run_cold};
use crate::Backend as _;
use crate::server::Drained;
use crate::server::build_info::{COMMIT, VERSION};
use crate::server::cache_cli_tests::{client, probe};
use crate::server::cli::{Addresses, Error, checked, run_serving};
use crate::server::combined_cli_tests::{Node, QUICK};
use crate::server::config::cold::ColdSettings;
use crate::server::config::{Settings, settings_from_text};
use crate::server::config_testing::oci_interface;
use crate::server::oci::OciMode;

/// What a client sends to ask `/healthz`, keeping the connection alive.
const HEALTHZ: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// A short pause between two looks at a state that another task changes.
const PAUSE: Duration = Duration::from_millis(20);

/// The `[[interface]]` and `[server]` tables of the files of `node`, with `server_extra` added to
/// `[server]`.
fn head(node: &Node, server_extra: &str) -> String {
    format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "n1"
bucket          = "tier-bucket"
access_key_file = "{key}"
{server_extra}
"#,
        key = node.key.display(),
    )
}

/// The `[cold_store]` table of `node`: its cold directory.
fn cold_table(node: &Node) -> String {
    format!(
        "\n[cold_store]\nkind = \"dir\"\npath = \"{}\"\n",
        node.cold().display()
    )
}

/// The `server.handoff_socket` line naming `socket`, or nothing.
fn handing(socket: Option<&Path>) -> String {
    socket.map_or_else(String::new, |socket| {
        format!("handoff_socket = {:?}", socket.display().to_string())
    })
}

/// The file of a node over the cold tier of `node` alone, with `server_extra` in `[server]`.
fn cold_text(node: &Node, server_extra: &str) -> String {
    format!("{}{}", head(node, server_extra), cold_table(node))
}

/// The settings of [`cold_text`].
fn cold_settings(text: &str) -> ColdSettings {
    settings_from_text(text, QUICK)
        .unwrap()
        .into_cold()
        .unwrap()
}

/// A scratch directory for the control socket (under the system's temporary directory, whose
/// path is short enough for a Unix socket) and the socket's path in it.
fn control() -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), "tscold-handoff");
    let socket = dir.path().join("control.sock");
    (dir, socket)
}

/// Runs the node of `settings` over its cold directory, calls `work` with its addresses, then
/// stops it.
async fn serving<F, Fut>(settings: &ColdSettings, work: F) -> Result<Drained, Error>
where
    F: FnOnce(Addresses) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = run_cold(
        settings,
        async move {
            let _ = stopped.await;
        },
        move |at| drop(told.send(at)),
    );
    let driving = async move {
        let at = bound.await.unwrap();
        work(at).await;
        let _ = stop.send(());
    };
    let (ran, ()) = tokio::join!(running, driving);
    ran
}

/// The holder named by the lease of the hot directory `hot`, or none when there is no lease.
async fn lease_holder(hot: &Path) -> Option<String> {
    let body = Node::tier(hot.to_owned()).get("_tiering/lease", None).await;
    let body = body.ok()?;
    let text = String::from_utf8_lossy(&body).into_owned();
    text.lines().next().map(str::to_owned)
}

/// Reads one answer to [`HEALTHZ`] from `connection`: its head and its body `ok`.
async fn answer_of(connection: &mut TcpStream) -> String {
    let mut answer = Vec::new();
    let mut piece = [0_u8; 1024];
    while !answer.ends_with(b"\r\n\r\nok") {
        let read = connection.read(&mut piece).await.unwrap();
        assert_ne!(read, 0, "{:?}", String::from_utf8_lossy(&answer));
        answer.extend_from_slice(&piece[..read]);
    }
    String::from_utf8_lossy(&answer).into_owned()
}

/// A connection to `at` whose second request is half written: one `/healthz` is answered, and
/// the first 16 bytes of the next are sent.
async fn half_written(at: SocketAddr) -> TcpStream {
    let mut connection = TcpStream::connect(at).await.unwrap();
    connection.write_all(HEALTHZ).await.unwrap();
    let first = answer_of(&mut connection).await;
    assert!(first.starts_with("HTTP/1.1 200"), "{first}");
    connection.write_all(&HEALTHZ[..16]).await.unwrap();
    connection
}

/// Asserts what a node over the cold tier serves: `/readyz` says `ready` (there is no cache, so
/// no mode), `/usagez` says the usage is not watched, `/metricz` has the OCI front's block and
/// neither a proxy's nor a cache's; a put is in the cold directory when it is acknowledged and
/// reads back, a delete removes it there, the OCI front answers, and nothing is made beside the
/// cold directory (no hot directory). Both fronts end with nothing cut.
#[tokio::test]
async fn a_node_over_the_cold_tier_serves_the_cold_store_and_keeps_nothing_else() {
    let node = Node::new("tscold-serve");
    let text = format!(
        "{}{}",
        cold_text(&node, ""),
        oci_interface("127.0.0.1:0", "")
    );
    let settings = cold_settings(&text);
    let cold = node.cold();
    let hot = node.hot();
    let work = |at: Addresses| async move {
        let (status, said) = probe(at.s3, "/readyz").await;
        assert_eq!((status, said.as_str()), (200, "ready"));
        let (status, said) = probe(at.s3, "/usagez").await;
        assert_eq!((status, said.as_str()), (200, "usage not watched"));
        let (status, said) = probe(at.s3, "/metricz").await;
        assert_eq!(status, 200);
        assert!(said.contains("tierstore_oci_"), "{said}");
        assert!(!said.contains("tierstore_reads_total"), "{said}");
        assert!(!said.contains("tierstore_cache_"), "{said}");

        let store = client(at.s3);
        let key = ObjectKey::new("cas/obj").unwrap();
        let body = Bytes::from_static(b"cold bytes");
        store.put_new(&key, body.clone(), None).await.unwrap();
        let kept = Node::tier(cold.clone()).head("cas/obj").await.unwrap();
        assert_eq!(kept.size, 10);
        let all = ByteRange::new(0, 10).unwrap();
        assert_eq!(store.get_range(&key, all).await.unwrap(), body);
        store.delete(&key).await.unwrap();
        let gone = Node::tier(cold).head("cas/obj").await;
        assert_eq!(gone, Err(crate::Error::NotFound("cas/obj".to_owned())));

        let oci = at.oci.expect("the OCI front");
        let (status, _) = probe(oci, "/v2/").await;
        assert_eq!(status, 200);
        assert!(
            !hot.exists(),
            "a node over the cold tier makes no hot directory"
        );
    };
    let drained = serving(&settings, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts the restart of a storage node (hot and cold tiers, no cache) into a node over its cold
/// tier alone, by the hand-off: the second start takes the first's sockets while a request of the
/// first is half written and says the very addresses; the half-written request is answered; once
/// the first has ended, its reconciler's lease is gone from the hot tier; the second is `ready`,
/// and a put through it is in the cold directory and not in the hot one; both end with nothing
/// cut and the control socket is removed. Catches: a node over the cold tier that binds beside
/// the running storage node or drops its requests, a storage node that leaves its lease behind
/// when it hands over (the reconciler of the processes still on the hot tier could not take
/// over), and a successor that still writes to the hot tier.
#[tokio::test]
async fn a_storage_node_hands_its_sockets_to_a_node_over_the_cold_tier() {
    let node = Node::new("tscold-handoff");
    node.activate().await;
    let (_dir, socket) = control();
    let hot_table = format!(
        "\n[hot_store]\nkind   = \"dir\"\npath   = \"{}\"\nbudget = \"1MiB\"\n",
        node.hot().display()
    );
    let server = handing(Some(&socket));
    let storage = format!("{}{hot_table}{}", head(&node, &server), cold_table(&node));
    let old = settings_from_text(&storage, QUICK)
        .unwrap()
        .into_storage()
        .unwrap();
    let new = cold_settings(&cold_text(&node, &server));
    let (old_told, old_bound) = oneshot::channel::<Addresses>();
    let (new_told, new_bound) = oneshot::channel::<Addresses>();
    let (old_done, old_ended) = oneshot::channel::<()>();
    let (go, going) = oneshot::channel::<()>();
    let (stop_old, stopped_old) = oneshot::channel::<()>();
    let (stop_new, stopped_new) = oneshot::channel::<()>();
    let first = async {
        let ran = run_serving(
            &old,
            async move {
                let _ = stopped_old.await;
            },
            move |at| drop(old_told.send(at)),
        )
        .await;
        let _ = old_done.send(());
        ran
    };
    let second = async {
        going.await.unwrap();
        run_cold(
            &new,
            async move {
                let _ = stopped_new.await;
            },
            move |at| drop(new_told.send(at)),
        )
        .await
    };
    let (hot, cold) = (node.hot(), node.cold());
    let drive = async move {
        let at = old_bound.await.unwrap();
        // The first's reconciler takes the lease: a lease that never comes is stopped by
        // nextest's hang guard, not by a bound of this test.
        while lease_holder(&hot).await.is_none() {
            tokio::time::sleep(PAUSE).await;
        }
        let mut before = half_written(at.s3).await;
        go.send(()).unwrap();
        let taken = new_bound.await.unwrap();
        assert_eq!(taken, at, "the second serves the very sockets");
        before.write_all(&HEALTHZ[16..]).await.unwrap();
        let late = answer_of(&mut before).await;
        assert!(late.starts_with("HTTP/1.1 200"), "{late}");
        // The first instance's drain holds a connection until the client closes its end.
        drop(before);
        let _ = old_ended.await;
        assert_eq!(
            lease_holder(&hot).await,
            None,
            "the first gave the lease up"
        );
        let (status, said) = probe(at.s3, "/readyz").await;
        assert_eq!((status, said.as_str()), (200, "ready"));

        let store = client(at.s3);
        let key = ObjectKey::new("cas/fresh").unwrap();
        store
            .put_new(&key, Bytes::from_static(b"new"), None)
            .await
            .unwrap();
        let landed = Node::tier(cold).head("cas/fresh").await.unwrap();
        assert_eq!(landed.size, 3);
        let in_hot = Node::tier(hot).head("cas/fresh").await;
        assert_eq!(in_hot, Err(crate::Error::NotFound("cas/fresh".to_owned())));
        let _ = stop_new.send(());
        drop(stop_old);
    };
    let (first, second, ()) = tokio::join!(first, second, drive);
    assert_eq!(first.unwrap(), Drained { abandoned: 0 });
    assert_eq!(second.unwrap(), Drained { abandoned: 0 });
    assert!(
        !socket.exists(),
        "a stop with no successor removes the socket"
    );
}

/// The `ready` of a start that must be refused before it binds.
fn never_bound(_: Addresses) {
    panic!("a refused start binds nothing");
}

/// Asserts that a start with a request timeout of zero is refused with the exact message before
/// anything is bound, and `--check-config` refuses the same file with the same message; that a
/// cold directory that cannot be made is refused with the message naming the tier; and that an
/// access key file that is not there is refused with the message naming it. Nothing is bound in
/// any of them.
#[tokio::test]
async fn a_start_with_no_request_time_or_a_cold_tier_it_cannot_open_is_refused() {
    let node = Node::new("tscold-refused");
    let zero = cold_settings(&cold_text(&node, "s3_request_timeout = \"0s\""));
    let why = run_cold(&zero, std::future::pending::<()>(), never_bound)
        .await
        .unwrap_err();
    let want = "--s3-request-timeout must be longer than zero";
    assert_eq!(why.to_string(), want);
    let checking = checked(&Settings::Cold(Box::new(zero))).unwrap_err();
    assert_eq!(checking.to_string(), want);

    let file = node.dir.path().join("a-file");
    std::fs::write(&file, b"not a directory").unwrap();
    let under = file.join("cold");
    let text = cold_text(&node, "").replace(
        &node.cold().display().to_string(),
        &under.display().to_string(),
    );
    let blocked = cold_settings(&text);
    let why = run_cold(&blocked, std::future::pending::<()>(), never_bound)
        .await
        .unwrap_err()
        .to_string();
    let named = format!("--cold \"dir:{}\": ", under.display());
    assert!(why.starts_with(&named), "{why}");

    let text = cold_text(&node, "").replace(&node.key.display().to_string(), "/no/such/access.key");
    let keyless = cold_settings(&text);
    let why = run_cold(&keyless, std::future::pending::<()>(), never_bound)
        .await
        .unwrap_err()
        .to_string();
    assert!(why.contains("/no/such/access.key"), "{why}");
}

/// Asserts what `--check-config` finds in the file of a node over the cold tier: a whole file
/// passes, and a key file that is not there is refused with a message that names it.
#[test]
fn check_config_of_a_node_over_the_cold_tier_reads_its_key_files() {
    let node = Node::new("tscold-check");
    let whole = cold_settings(&cold_text(&node, ""));
    assert!(checked(&Settings::Cold(Box::new(whole))).is_ok());
    let text = cold_text(&node, "").replace(&node.key.display().to_string(), "/no/such/access.key");
    let gone = cold_settings(&text);
    let why = checked(&Settings::Cold(Box::new(gone))).unwrap_err();
    assert!(why.to_string().contains("/no/such/access.key"), "{why}");
}

/// Asserts the line a start says once it serves: the release, the S3 addresses, `mode=cold-only`
/// and the commit, with the OCI front's addresses and mode between when it is served.
#[test]
fn the_start_line_names_the_shape_and_each_front() {
    let s3: SocketAddr = "127.0.0.1:9300".parse().unwrap();
    let oci: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    let plain = Addresses {
        s3,
        s3_all: vec![s3],
        oci: None,
        oci_all: Vec::new(),
        oci_mode: None,
        trees_all: Vec::new(),
    };
    assert_eq!(
        announce_line(&plain),
        format!("tierstore {VERSION} listen=127.0.0.1:9300 mode=cold-only commit={COMMIT}\n")
    );
    let with_oci = Addresses {
        oci: Some(oci),
        oci_all: vec![oci],
        oci_mode: Some(OciMode::Http),
        ..plain
    };
    let mode = OciMode::Http.as_str();
    assert_eq!(
        announce_line(&with_oci),
        format!(
            "tierstore {VERSION} listen=127.0.0.1:9300 mode=cold-only oci-listen=127.0.0.1:5000 \
             ({mode}) commit={COMMIT}\n"
        )
    );
}

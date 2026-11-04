//! The binary's file starts the front: `run_serving` in this process, and the `tierstore`
//! process itself.

use std::fs::Permissions;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use objstore_s3::s3::{Credentials, S3Config, S3Store};
use objstore_s3::{Capabilities, ObjectKey, ObjectStore};
use rustix::process::{Pid, Signal, kill_process};
use tierstore::server::Drained;
use tierstore::server::cli::{Addresses, Cli, run_serving};
use tierstore::server::oci::OciMode;
use tierstore_testkit::dir::TestDir;
use tierstore_testkit::reaped::{first_line, spawn};
use tokio::sync::oneshot;

use crate::client::{Conn, connector};
use crate::pki::{Leaf, ca, ip, leaf};
use crate::rig::hex_of;

const KEY_ID: &str = "AKIDOCI";
const SECRET: &str = "oci-secret";

pub struct Files {
    _dir: TestDir,
    dir: std::path::PathBuf,
    key: String,
    leaf: Leaf,
    pub ca_pem: String,
}

pub fn files(name: &str) -> Files {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let key = dir.path().join("key");
    std::fs::write(&key, format!("{KEY_ID}\n{SECRET}\n")).expect("key file");
    std::fs::set_permissions(&key, Permissions::from_mode(0o600)).expect("mode");
    let ca = ca();
    let leaf = leaf(dir.path(), "front", &ca, vec![ip("127.0.0.1")]);
    Files {
        key: key.display().to_string(),
        leaf,
        ca_pem: ca.pem,
        dir: dir.path().to_owned(),
        _dir: dir,
    }
}

impl Files {
    /// The certificate and key params of an `oci` interface that name this rig's leaf.
    fn tls(&self) -> String {
        let cert = self.leaf.cert.to_str().expect("a path");
        let key = self.leaf.key.to_str().expect("a path");
        format!(", tls_cert_file = {cert:?}, tls_key_file = {key:?}")
    }

    /// Writes the file of a start whose `oci` interface listens at `listen` with `params` after its
    /// address (`None`: no `oci` interface) as `name`, and gives the arguments of that start: the
    /// name of the program, which the callers that run the binary leave out, then the file and a
    /// short drain.
    fn start_with(&self, name: &str, oci: Option<(&str, &str)>) -> Vec<String> {
        let oci = oci.map_or_else(String::new, |(listen, params)| {
            format!(
                "\n[[interface]]\ntype = \"oci\"\nparams = {{ addresses = [{listen:?}]{params} }}\n"
            )
        });
        let text = format!(
            "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n{oci}\n\
             [server]\nproxy_id = \"oci\"\nbucket = \"tier-bucket\"\naccess_key_file = {:?}\n\n\
             [hot_store]\nkind = \"memory\"\nbudget = 1048576\n\n[cold_store]\nkind = \"memory\"\n",
            self.key
        );
        let file = self.dir.join(name);
        std::fs::write(&file, text).expect("the file");
        [
            "tierstore",
            "--config",
            &file.display().to_string(),
            "--drain-lb-wait",
            "50ms",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    /// The arguments of a start, with the OCI front on (HTTPS, with this rig's certificate and
    /// key) if `oci`.
    pub fn flags(&self, oci: bool) -> Vec<String> {
        let front = oci.then(|| ("127.0.0.1:0", self.tls()));
        self.start_with(
            "tierstore.toml",
            front.as_ref().map(|(at, tls)| (*at, tls.as_str())),
        )
    }

    /// The arguments of a start with the OCI front on plain HTTP at `listen`: no certificate, no key.
    pub fn plain(&self, listen: &str) -> Vec<String> {
        self.start_with("plain.toml", Some((listen, "")))
    }

    /// The arguments of a start whose `oci` interface is at `listen` with `params` after the address.
    pub fn with_params(&self, listen: &str, params: &str) -> Vec<String> {
        self.start_with("params.toml", Some((listen, params)))
    }
}

/// The settings that the arguments of a start stand for (the program name, then `--config <file>`
/// and `--drain-lb-wait <wait>`, in seconds or milliseconds).
pub fn cli_of(flags: &[String]) -> Cli {
    let value = |flag: &str| {
        let at = flags.iter().position(|word| word == flag).expect("a flag");
        flags[at + 1].as_str()
    };
    let text = std::fs::read_to_string(value("--config")).expect("the file");
    let mut cli = Cli::from_config_text(&text).expect("a file");
    let wait = value("--drain-lb-wait");
    cli.drain_lb_wait = match wait.strip_suffix("ms") {
        Some(millis) => Duration::from_millis(millis.parse().expect("milliseconds")),
        None => Duration::from_secs(
            wait.strip_suffix('s')
                .expect("seconds")
                .parse()
                .expect("seconds"),
        ),
    };
    cli
}

pub fn store(addr: SocketAddr) -> S3Store {
    let config = S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: "tier-bucket".to_owned(),
        credentials: Credentials::new(KEY_ID, SECRET),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    S3Store::new(config).expect("a client")
}

/// Runs the server over the arguments `flags`, calls `work` with the addresses it is bound to, then stops it.
async fn serving<F, Fut>(flags: Vec<String>, work: F) -> Drained
where
    F: FnOnce(Addresses) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let cli = cli_of(&flags);
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = run_serving(
        &cli,
        async move {
            let _ = stopped.await;
        },
        move |addresses| {
            let _ = told.send(addresses);
        },
    );
    let driving = async {
        work(bound.await.expect("bound")).await;
        stop.send(()).expect("running");
    };
    let (drained, ()) = tokio::join!(running, driving);
    drained.expect("a drain")
}

/// Asserts that a blob put through the S3 front with its full key is pulled by digest over
/// HTTPS from the OCI front of the same process; that `/metricz` on the S3 listener shows the
/// request and the bytes; and that a stop drains with nothing cut and closes the OCI socket.
#[tokio::test]
async fn an_object_put_over_s3_is_pulled_by_digest_over_https() {
    let files = files("oci-binary-run");
    let content = b"a layer put through the S3 front".repeat(100);
    let hex = hex_of(&content);
    let connector = connector(&files.ca_pem);
    let drained = serving(files.flags(true), |bound| async move {
        let oci = bound.oci.expect("the OCI front is bound");
        let key = ObjectKey::new(format!("oci/blobs/sha256/{hex}")).expect("a key");
        store(bound.s3)
            .put_new(&key, Bytes::from(content.clone()), None)
            .await
            .expect("put");
        let mut conn = Conn::open(oci, &connector).await.expect("a connection");
        let path = format!("/v2/library/app/blobs/sha256:{hex}");
        let reply = conn.request("GET", &path, &[]).await.expect("an answer");
        assert_eq!(reply.status, 200);
        assert!(reply.body == content);
        let metrics = reqwest::get(format!("http://{}/metricz", bound.s3))
            .await
            .expect("metricz");
        assert_eq!(metrics.status(), 200);
        let text = metrics.text().await.expect("text");
        let ok = r#"tierstore_oci_requests_total{route="blob",outcome="ok"} 1"#;
        assert!(text.contains(ok), "{text}");
        let bytes = format!("tierstore_oci_bytes_served_total {}\n", content.len());
        assert!(text.contains(&bytes), "{text}");
        assert!(
            text.contains("tierstore_oci_digest_mismatches_total 0\n"),
            "{text}"
        );
    })
    .await;
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that without the OCI flags no such listener is bound and `/metricz` has no block of
/// the front, only that of the proxy.
#[tokio::test]
async fn without_the_flags_there_is_no_front() {
    let files = files("oci-binary-off");
    let drained = serving(files.flags(false), |bound| async move {
        assert_eq!(bound.oci, None);
        let metrics = reqwest::get(format!("http://{}/metricz", bound.s3))
            .await
            .expect("metricz");
        assert_eq!(metrics.status(), 200);
        let text = metrics.text().await.expect("text");
        assert!(!text.contains("tierstore_oci_requests_total"), "{text}");
        let hits = r#"tierstore_hits_total{tier="hot",op="get"} 0"#;
        assert!(text.contains(hits), "{text}");
    })
    .await;
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that the process prints the OCI address on its start line, serves HTTPS there, and
/// exits 0 on SIGTERM, and that a start without a usable certificate exits 2 and says why.
#[tokio::test]
async fn the_process_announces_the_front_and_refuses_a_bad_certificate() {
    let files = files("oci-binary-process");
    let bin = env!("CARGO_BIN_EXE_tierstore");
    let mut command = Command::new(bin);
    command
        .args(&files.flags(true)[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = spawn(&mut command, 3).expect("spawned");
    let stdout = child.stdout.take().expect("stdout");
    let (line, _reader) = first_line(stdout, Duration::from_secs(30)).expect("a start line");
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words.len(), 6, "{line}");
    assert_eq!(words[4], "(https)", "{line}");
    let commit = tierstore::server::build_info::COMMIT;
    assert_eq!(words[5], format!("commit={commit}"), "{line}");
    assert!(words[2].starts_with("listen="), "{line}");
    let oci: SocketAddr = words[3]
        .strip_prefix("oci-listen=")
        .expect("oci-listen=")
        .parse()
        .expect("an address");
    let mut conn = Conn::open(oci, &connector(&files.ca_pem))
        .await
        .expect("a connection");
    assert_eq!(
        conn.request("GET", "/v2/", &[])
            .await
            .expect("an answer")
            .status,
        200
    );
    drop(conn);
    let pid = Pid::from_raw(i32::try_from(child.id()).expect("a pid")).expect("a pid");
    kill_process(pid, Signal::TERM).expect("signalled");
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("joined");
    assert_eq!(status.expect("waited").code(), Some(0));

    let params = format!(
        ", tls_cert_file = {:?}, tls_key_file = {:?}",
        files.key,
        files.leaf.key.to_str().expect("a path")
    );
    let flags = files.with_params("127.0.0.1:0", &params);
    let out = Command::new(bin).args(&flags[1..]).output().expect("ran");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("params.tls_cert_file"), "{stderr}");
}

/// Asserts that a start whose OCI address is taken fails with the address in the message, and
/// that the process exits with the status of a start that a restart may mend.
#[tokio::test]
async fn an_oci_address_in_use_fails_the_start() {
    let files = files("oci-binary-taken");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bound");
    let busy = taken.local_addr().expect("an address").to_string();
    let cli = cli_of(&files.with_params(&busy, &files.tls()));
    let started = run_serving(&cli, std::future::pending(), |_| {}).await;
    let why = started.expect_err("a failed start").to_string();
    assert!(why.starts_with("binding 127.0.0.1:"), "{why}");
}

/// Asserts that without a certificate and key the front is plain HTTP on a loopback address: an
/// object put through the S3 front is pulled by digest over plain HTTP, the start reports the
/// mode, `/metricz` says `http`, and a stop drains with nothing cut.
#[tokio::test]
async fn a_loopback_front_without_a_certificate_serves_plain_http() {
    let files = files("oci-binary-plain");
    let content = b"a layer pulled over loopback".repeat(100);
    let hex = hex_of(&content);
    let drained = serving(files.plain("127.0.0.1:0"), |bound| async move {
        assert_eq!(bound.oci_mode, Some(OciMode::Http));
        let oci = bound.oci.expect("the OCI front is bound");
        let key = ObjectKey::new(format!("oci/blobs/sha256/{hex}")).expect("a key");
        store(bound.s3)
            .put_new(&key, Bytes::from(content.clone()), None)
            .await
            .expect("put");
        let mut conn = Conn::open_plain(oci).await.expect("a connection");
        let path = format!("/v2/library/app/blobs/sha256:{hex}");
        let reply = conn.request("GET", &path, &[]).await.expect("an answer");
        assert_eq!(reply.status, 200);
        assert!(reply.body == content);
        let metrics = reqwest::get(format!("http://{}/metricz", bound.s3))
            .await
            .expect("metricz");
        let text = metrics.text().await.expect("text");
        assert!(
            text.contains("tierstore_oci_front_info{mode=\"http\"} 1\n"),
            "{text}"
        );
        assert!(!text.contains("mode=\"https\""), "{text}");
    })
    .await;
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that `/metricz` says `https` when the front has a certificate.
#[tokio::test]
async fn metricz_says_https_when_the_front_has_a_certificate() {
    let files = files("oci-binary-https-mode");
    let drained = serving(files.flags(true), |bound| async move {
        assert_eq!(bound.oci_mode, Some(OciMode::Https));
        let metrics = reqwest::get(format!("http://{}/metricz", bound.s3))
            .await
            .expect("metricz");
        let text = metrics.text().await.expect("text");
        assert!(
            text.contains("tierstore_oci_front_info{mode=\"https\"} 1\n"),
            "{text}"
        );
    })
    .await;
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that the process prints `(http)` after the OCI address of a plain front and serves
/// plain HTTP there, and exits 0 on SIGTERM.
#[tokio::test]
async fn the_process_announces_a_plain_front() {
    let files = files("oci-binary-plain-process");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tierstore"));
    command
        .args(&files.plain("127.0.0.1:0")[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = spawn(&mut command, 3).expect("spawned");
    let stdout = child.stdout.take().expect("stdout");
    let (line, _reader) = first_line(stdout, Duration::from_secs(30)).expect("a start line");
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words.len(), 6, "{line}");
    assert_eq!(words[4], "(http)", "{line}");
    let oci: SocketAddr = words[3]
        .strip_prefix("oci-listen=")
        .expect("oci-listen=")
        .parse()
        .expect("an address");
    let mut conn = Conn::open_plain(oci).await.expect("a connection");
    let reply = conn.request("GET", "/v2/", &[]).await.expect("an answer");
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.must("docker-distribution-api-version"),
        "registry/2.0"
    );
    drop(conn);
    let pid = Pid::from_raw(i32::try_from(child.id()).expect("a pid")).expect("a pid");
    kill_process(pid, Signal::TERM).expect("signalled");
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("joined");
    assert_eq!(status.expect("waited").code(), Some(0));
}

/// The status and the text on stderr of a start with `flags`, which must end before it serves.
fn refused(flags: &[String]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .args(&flags[1..])
        .output()
        .expect("ran");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Asserts that the process starts a plain front on the unspecified address, serves a request
/// there and exits 0 on SIGTERM, and prints the plain-HTTP warning exactly once; and that on
/// loopback it prints no such warning.
#[tokio::test]
async fn a_plain_front_off_loopback_starts_and_warns_once() {
    let files = files("oci-binary-plain-wide");
    let marker = "serves plain HTTP on a non-loopback address";
    for (listen, warnings) in [("0.0.0.0:0", 1), ("127.0.0.1:0", 0)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tierstore"));
        command
            .args(&files.plain(listen)[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = spawn(&mut command, 3).expect("spawned");
        let stdout = child.stdout.take().expect("stdout");
        let mut stderr = child.stderr.take().expect("stderr");
        let (line, _reader) = first_line(stdout, Duration::from_secs(30)).expect("a start line");
        let words: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(words.len(), 6, "{line}");
        assert_eq!(words[4], "(http)", "{line}");
        let bound: SocketAddr = words[3]
            .strip_prefix("oci-listen=")
            .expect("oci-listen=")
            .parse()
            .expect("an address");
        let dial = SocketAddr::from(([127, 0, 0, 1], bound.port()));
        let mut conn = Conn::open_plain(dial).await.expect("a connection");
        assert_eq!(
            conn.request("GET", "/v2/", &[])
                .await
                .expect("an answer")
                .status,
            200,
            "{listen}"
        );
        drop(conn);
        let pid = Pid::from_raw(i32::try_from(child.id()).expect("a pid")).expect("a pid");
        kill_process(pid, Signal::TERM).expect("signalled");
        let (status, text) = tokio::task::spawn_blocking(move || {
            let status = child.wait().expect("waited");
            let mut text = String::new();
            std::io::Read::read_to_string(&mut stderr, &mut text).expect("stderr");
            (status, text)
        })
        .await
        .expect("joined");
        assert_eq!(status.code(), Some(0), "{listen}: {text}");
        assert_eq!(text.matches(marker).count(), warnings, "{listen}: {text}");
    }
}

/// Asserts that a certificate without a key, and a key without a certificate, are refused when
/// the file is read, with status 2 and the exact message that names the param that is missing.
#[tokio::test]
async fn a_certificate_without_a_key_is_refused_at_start() {
    let files = files("oci-binary-half-pair");
    let cert = files.leaf.cert.to_str().expect("a path");
    let key = files.leaf.key.to_str().expect("a path");
    for (param, path, missing, partner) in [
        ("tls_cert_file", cert, "tls_key_file", "tls_cert_file"),
        ("tls_key_file", key, "tls_cert_file", "tls_key_file"),
    ] {
        let flags = files.with_params("127.0.0.1:0", &format!(", {param} = {path:?}"));
        let (status, stderr) = refused(&flags);
        assert_eq!(status, Some(2), "{param}: {stderr}");
        let want = format!(
            "tierstore: config file {}: interface[1].params.{missing}: it is needed with {partner}: give both, or neither\n",
            flags[2]
        );
        assert_eq!(stderr, want);
    }
}

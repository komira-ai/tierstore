//! `params.upstream` at the start: what the client is made of, what refuses to start and with
//! which exit status, and that the front the start binds pulls through.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest as _, Sha256};
use tierstore_testkit::dir::TestDir;
use tokio::sync::watch;

use super::{Cli, Error, build, exit_status};
use crate::server::cli_serve::bind_fronts;
use crate::server::config_interface::UpstreamTable;
use crate::server::config_units::Dur;
use crate::server::oci::fake_upstream::{Fake, Registry};
use crate::server::oci::test_pki::ca;
use crate::{Backend, MemoryBackend};

/// The settings of a start with a plain OCI front on loopback and no upstream.
fn cli(dir: &Path) -> Cli {
    let key = file(dir, "key", "AKID\nsecret-0123456789\n", 0o600);
    let text = format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\n[[interface]]\ntype = \"oci\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\n[server]\nproxy_id = \"p1\"\nbucket = \"tier-bucket\"\naccess_key_file = \"{}\"\n\n[hot_store]\nkind = \"memory\"\nbudget = 1000\n\n[cold_store]\nkind = \"memory\"\n",
        key.display()
    );
    Cli::from_config_text(&text).expect("a file")
}

/// A file with `text` and `mode` in `dir`.
fn file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write");
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).expect("mode");
    path
}

fn table(url: &str) -> UpstreamTable {
    UpstreamTable {
        url: Some(url.to_owned()),
        username: None,
        password_file: None,
        ca_file: None,
        timeout: None,
        max_redirects: None,
        recheck_timeout: None,
    }
}

fn with_login(url: &str, password_file: PathBuf) -> UpstreamTable {
    UpstreamTable {
        username: Some("puller".to_owned()),
        password_file: Some(password_file),
        ..table(url)
    }
}

/// The exit status and the message of the start `cli` makes, which is refused.
fn refused(cli: &Cli) -> (u8, String) {
    let error: Error = build(cli).err().expect("a refused start");
    (exit_status(&error), error.to_string())
}

/// Asserts a start with no `params.upstream` makes no client, and that one with the table makes
/// one, with a login, a CA file and both limits given.
#[test]
fn the_start_makes_a_client_only_for_an_upstream_table() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-upstream-start");
    let plain = build(&cli(dir.path()))
        .expect("built")
        .oci
        .expect("a front");
    assert!(plain.upstream.is_none());
    let password = file(dir.path(), "upstream.pw", "s3cret\n", 0o600);
    let roots = file(dir.path(), "ca.pem", &ca().pem(), 0o644);
    let mut given = cli(dir.path());
    given.oci_upstream = Some(UpstreamTable {
        ca_file: Some(roots),
        timeout: Some(Dur(Duration::from_secs(45))),
        max_redirects: Some(2),
        recheck_timeout: Some(Dur(Duration::from_secs(2))),
        ..with_login("https://registry.example", password)
    });
    let made = build(&given).expect("built").oci.expect("a front");
    assert!(made.upstream.is_some());
    let mut bare = cli(dir.path());
    bare.oci_upstream = Some(table("https://registry.example"));
    assert!(
        build(&bare)
            .expect("built")
            .oci
            .expect("a front")
            .upstream
            .is_some()
    );
}

/// Asserts the password is the file's first line without its line end, and goes to the upstream
/// as the Basic login of the user, which the upstream asks for with its 401.
#[tokio::test]
async fn the_password_is_the_files_text_without_its_line_end() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-upstream-login");
    let want = format!("Basic {}", STANDARD.encode("puller:s3cret"));
    let registry = Registry::start(move |seen| match &seen.authorization {
        Some(given) if *given == want => Fake::ok("in"),
        _ => Fake::challenge(r#"Basic realm="fake""#),
    })
    .await;
    let password = file(dir.path(), "upstream.pw", "s3cret\r\n", 0o600);
    let mut flags = cli(dir.path());
    flags.oci_upstream = Some(with_login(&registry.url(), password));
    let upstream = build(&flags).expect("built").oci.expect("a front").upstream;
    let reply = upstream.expect("a client").get("/v2/", "*/*").await;
    let body = reply.expect("granted").bytes(100).await.expect("a body");
    assert_eq!(body, "in");
}

/// Asserts every upstream that cannot be used is refused at the start with the status of its
/// kind and a message that names the key: 2 for what a restart cannot mend (a password file
/// others can read, missing or empty, a CA file with no certificate, an address that cannot
/// work), 1 for a CA file that cannot be read.
#[test]
fn an_upstream_that_cannot_be_used_is_refused_with_the_status_of_its_kind() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-upstream-refused");
    let open = file(dir.path(), "open.pw", "s3cret\n", 0o644);
    let empty = file(dir.path(), "empty.pw", "\n", 0o600);
    let missing = dir.path().join("missing.pw");
    let no_ca = file(dir.path(), "none.pem", "nothing", 0o644);
    let https = "https://registry.example";
    let cases: Vec<(UpstreamTable, u8, String)> = vec![
        (
            with_login(https, open.clone()),
            2,
            format!(
                "params.upstream.password_file {}: others can read it; make it mode 0600",
                open.display()
            ),
        ),
        (
            with_login(https, empty.clone()),
            2,
            format!(
                "params.upstream.password_file {}: it is empty",
                empty.display()
            ),
        ),
        (
            with_login(https, missing.clone()),
            2,
            format!("params.upstream.password_file {}: ", missing.display()),
        ),
        (
            UpstreamTable {
                ca_file: Some(no_ca),
                ..table(https)
            },
            2,
            "params.upstream: ca_file: it holds no certificate".to_owned(),
        ),
        (
            UpstreamTable {
                ca_file: Some(dir.path().join("missing.pem")),
                ..table(https)
            },
            1,
            "params.upstream.ca_file ".to_owned(),
        ),
        (
            table("https://registry.example/v2/"),
            2,
            "params.upstream: url: it must be a host and a port, with no path or query".to_owned(),
        ),
        (
            UpstreamTable {
                url: None,
                ..table(https)
            },
            2,
            "params.upstream: url: it is not an address".to_owned(),
        ),
    ];
    for (upstream, status, message) in cases {
        let mut flags = cli(dir.path());
        flags.oci_upstream = Some(upstream);
        let (got, why) = refused(&flags);
        assert_eq!(got, status, "{why}");
        assert!(why.contains(&message), "wanted {message:?} in {why:?}");
    }
}

/// Asserts a login that would go over plain HTTP to an address that is not loopback is refused at
/// the start (exit 2), and is taken over HTTPS or to loopback.
#[test]
fn a_login_is_not_sent_in_the_clear() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-upstream-clear");
    let password = file(dir.path(), "upstream.pw", "s3cret\n", 0o600);
    let mut flags = cli(dir.path());
    flags.oci_upstream = Some(with_login("http://registry.example:5000", password.clone()));
    let (status, why) = refused(&flags);
    assert_eq!(status, 2, "{why}");
    assert!(why.contains("a password is sent only over https"), "{why}");
    for url in ["https://registry.example", "http://127.0.0.1:5000"] {
        flags.oci_upstream = Some(with_login(url, password.clone()));
        build(&flags).expect(url);
    }
}

/// Asserts the front a start binds pulls through: a blob the store lacks is fetched from the
/// upstream over the front's own listener and served, and the next request for it does not reach
/// the upstream.
#[tokio::test]
async fn the_bound_front_pulls_a_missing_blob_from_the_upstream() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-upstream-bound");
    let content: Vec<u8> = (0..3000_u32)
        .map(|i| u8::try_from(i % 251).expect("a byte"))
        .collect();
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&content)));
    let path = format!("/v2/library/app/blobs/{digest}");
    let served = content.clone();
    let registry = Registry::start(move |seen| {
        if seen.path == path {
            Fake::ok(served.clone())
        } else {
            Fake::status(404)
        }
    })
    .await;
    let mut flags = cli(dir.path());
    flags.oci_upstream = Some(table(&registry.url()));
    let built = build(&flags).expect("built");
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let s3: Vec<std::net::SocketAddr> = vec!["127.0.0.1:0".parse().expect("an address")];
    let own =
        || crate::server::cli_serve::bind_s3(&s3, crate::server::ListenTuning::default().backlog);
    let bound = bind_fronts(None, &s3, own, built.oci, backend)
        .await
        .expect("bound");
    let listener = bound.oci.expect("a front");
    let addr = listener.local_addr().expect("an address");
    let (stop, stopped) = watch::channel(false);
    let serving = tokio::spawn(listener.serve(stopped));
    let url = format!("http://{addr}/v2/library/app/blobs/{digest}");
    for _ in 0..2 {
        let reply = reqwest::get(&url).await.expect("an answer");
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.bytes().await.expect("a body"), content.as_slice());
    }
    let missing = format!(
        "http://{addr}/v2/library/app/blobs/sha256:{}",
        "0".repeat(64)
    );
    let reply = reqwest::get(&missing).await.expect("an answer");
    assert_eq!(reply.status(), 404);
    assert_eq!(registry.seen().len(), 2);
    stop.send(true).expect("a front still serving");
    serving.await.expect("the front ended");
}

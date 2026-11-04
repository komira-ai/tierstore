//! The `oci` interface: what its params make, what refuses to start, and the exit status of each
//! refusal.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::super::stop_watcher::oci_cut_note;
use super::{Cli, Error, build, exit_status, run_serving};
use crate::server::oci::plain_warning;
use crate::server::oci::test_pki::{Leaf, ca, dns, ip, leaf};

/// A directory with a key file and a certificate and key that name 127.0.0.1.
struct Rig {
    _dir: TestDir,
    key: String,
    leaf: Leaf,
    dir: std::path::PathBuf,
}

impl Rig {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let key = dir.path().join("key");
        std::fs::write(&key, "AKID\nsecret-0123456789\n").expect("write");
        std::fs::set_permissions(&key, Permissions::from_mode(0o600)).expect("mode");
        let leaf = leaf(dir.path(), "front", &ca(), vec![ip("127.0.0.1")]);
        let path = dir.path().to_owned();
        Self {
            _dir: dir,
            key: key.display().to_string(),
            leaf,
            dir: path,
        }
    }

    /// The certificate and key params of an `oci` interface, which name this rig's leaf.
    fn tls(&self) -> String {
        let cert = self.leaf.cert.to_str().expect("a path");
        let key = self.leaf.key.to_str().expect("a path");
        format!(", tls_cert_file = \"{cert}\", tls_key_file = \"{key}\"")
    }

    /// The file of a start whose `oci` interface listens at `listen` and has `params` after its
    /// address; with none, the file has no `oci` interface.
    fn file(&self, oci: Option<(&str, &str)>) -> String {
        let oci = oci.map_or_else(String::new, |(listen, params)| {
            format!(
                "\n[[interface]]\ntype = \"oci\"\nparams = {{ addresses = [\"{listen}\"]{params} }}\n"
            )
        });
        format!(
            "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n{oci}\n[server]\nproxy_id = \"p1\"\nbucket = \"tier-bucket\"\naccess_key_file = \"{}\"\n\n[hot_store]\nkind = \"memory\"\nbudget = 1000\n\n[cold_store]\nkind = \"memory\"\n",
            self.key
        )
    }

    /// The settings of a start with the `oci` interface on, with the certificate and key and then
    /// `more` params (each after a comma).
    fn cli(&self, more: &str) -> Cli {
        let params = format!("{}{more}", self.tls());
        Cli::from_config_text(&self.file(Some(("127.0.0.1:0", &params)))).expect("a file")
    }
}

/// The exit status and the message of the start `cli` makes.
fn refused(cli: &Cli) -> (u8, String) {
    let error: Error = build(cli).err().expect("a refused start");
    (exit_status(&error), error.to_string())
}

/// Asserts that a start with no address for the S3 front is refused before it serves: the file
/// needs an `s3` interface, and settings made another way do not slip through.
#[tokio::test]
async fn a_start_with_no_listen_address_is_refused() {
    let rig = Rig::new("oci-interface-no-s3");
    let mut cli = rig.cli("");
    cli.drain_lb_wait = Duration::from_millis(50);
    cli.listen.clear();
    let refused = run_serving(&cli, async {}, |_| {}).await.err();
    let why = refused.expect("a refused start").to_string();
    assert!(why.contains("an s3 interface is needed"), "{why}");
}

/// Asserts that the front is off without an `oci` interface: nothing is made, and `/metricz` has
/// no counters to show.
#[test]
fn there_is_no_front_without_an_oci_interface() {
    let rig = Rig::new("oci-interface-off");
    let cli = Cli::from_config_text(&rig.file(None)).expect("a file");
    let built = build(&cli).expect("built");
    assert!(built.oci.is_none());
    assert!(built.server.oci_metrics.is_none());
}

/// Asserts the settings the params make: the defaults (prefix `oci/`, 64 connections, an hour,
/// the S3 front's header and idle timeouts) and the values given, and that the counters in the
/// server's settings are the front's.
#[test]
fn the_params_make_the_front() {
    let rig = Rig::new("oci-interface-on");
    let built = build(&rig.cli("")).expect("built");
    let setup = built.oci.expect("a front");
    assert_eq!(setup.prefix.as_str(), "oci/");
    assert_eq!(setup.listen, vec!["127.0.0.1:0".parse().unwrap()]);
    assert_eq!(setup.limits.max_connections, 64);
    assert_eq!(setup.limits.lifetime, Duration::from_secs(3600));
    assert_eq!(setup.limits.handshake_timeout, Duration::from_secs(10));
    assert_eq!(setup.limits.header_timeout, built.server.header_timeout);
    assert_eq!(setup.limits.write_idle, built.server.body_idle_timeout);
    assert_eq!(setup.limits.drain_deadline, built.server.drain_deadline);
    assert!(setup.acceptor.is_some());
    let shown = built.server.oci_metrics.expect("counters");
    assert!(Arc::ptr_eq(&shown, &setup.metrics));
    assert!(
        shown
            .render()
            .contains("tierstore_oci_front_info{mode=\"https\"} 1\n")
    );

    let given = ", prefix = \"images/v1/\", max_connections = 7, connection_lifetime = \"90s\"";
    let built = build(&rig.cli(given)).expect("built");
    let setup = built.oci.expect("a front");
    assert_eq!(setup.prefix.as_str(), "images/v1/");
    assert_eq!(setup.limits.max_connections, 7);
    assert_eq!(setup.limits.lifetime, Duration::from_secs(90));
}

/// Asserts that a prefix that cannot work, a lifetime of nothing, and a certificate or key that
/// cannot serve are refused at start with status 2, and that a file that cannot be read is
/// status 1, which a restart may mend.
#[test]
fn a_front_that_cannot_serve_is_refused_at_start() {
    let rig = Rig::new("oci-interface-refused");
    for prefix in ["_journal/", "_tiering/x/", "oci", "", "a//b/", "../"] {
        let (status, why) = refused(&rig.cli(&format!(", prefix = {prefix:?}")));
        assert_eq!(status, 2, "{prefix:?}: {why}");
        assert!(why.contains("--oci-prefix"), "{why}");
    }
    let open = rig.dir.join("open.key");
    std::fs::copy(&rig.leaf.key, &open).expect("copy");
    std::fs::set_permissions(&open, Permissions::from_mode(0o644)).expect("mode");
    let mut cli = rig.cli("");
    cli.oci_tls_key = Some(open);
    let (status, why) = refused(&cli);
    assert_eq!(status, 2, "{why}");
    assert!(why.contains("any user can read the key"), "{why}");
    let (status, why) = refused(&rig.cli(", connection_lifetime = \"0s\""));
    assert_eq!(status, 2, "{why}");
    assert!(why.contains("--oci-connection-lifetime"), "{why}");

    let other = leaf(
        &rig.dir,
        "other",
        &ca(),
        vec![ip("192.0.2.99"), dns("registry.test")],
    );
    let mut cli = rig.cli("");
    cli.oci_tls_cert = Some(other.cert);
    cli.oci_tls_key = Some(other.key);
    let (status, why) = refused(&cli);
    assert_eq!(status, 2, "{why}");
    assert!(why.contains("no IP subjectAltName for 127.0.0.1"), "{why}");

    let mut cli = rig.cli("");
    cli.oci_tls_cert = Some(rig.dir.join("missing.pem"));
    assert_eq!(refused(&cli).0, 1);
    let mut cli = rig.cli("");
    cli.oci_tls_key = Some(rig.dir.join("missing.key"));
    assert_eq!(refused(&cli).0, 1);

    // The file does not let these through; a settings struct built by hand might. They are
    // refused on loopback too, where no certificate at all would do.
    for drop in [0, 1] {
        let mut cli = rig.cli("");
        match drop {
            0 => cli.oci_tls_cert = None,
            _ => cli.oci_tls_key = None,
        }
        let (status, why) = refused(&cli);
        assert_eq!(status, 2, "{why}");
        assert!(why.contains("go together"), "{why}");
    }
}

/// Asserts that an OCI address that is in use fails the start with status 1 (a restart may mend
/// it), and the message names the address.
#[tokio::test]
async fn an_oci_address_in_use_is_status_1() {
    let rig = Rig::new("oci-interface-in-use");
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("a socket");
    let addr = held.local_addr().expect("an address").to_string();
    let cli = Cli::from_config_text(&rig.file(Some((&addr, &rig.tls())))).expect("a file");
    let stop = std::future::pending::<()>();
    let error = run_serving(&cli, stop, |_| ()).await.expect_err("refused");
    assert_eq!(exit_status(&error), 1);
    assert!(
        error.to_string().starts_with(&format!("binding {addr}: ")),
        "{error}"
    );
}

/// Asserts that a bucket called `metricz` is refused like the other probe paths.
#[test]
fn the_metrics_path_is_not_a_bucket_name() {
    let rig = Rig::new("oci-interface-bucket");
    let text = rig.file(None).replacen("tier-bucket", "metricz", 1);
    let cli = Cli::from_config_text(&text).expect("a file");
    // Status 1, as for healthz, readyz and usagez: the probe-path refusal is not a refusal of the file.
    let (status, why) = refused(&cli);
    assert_eq!(status, 1, "{why}");
    assert!(why.contains("probe path"), "{why}");
}

/// Asserts what is said of OCI connections cut at the drain deadline: nothing for none.
#[test]
fn the_cut_connections_are_reported() {
    assert_eq!(oci_cut_note(0), "");
    assert_eq!(
        oci_cut_note(3),
        "tierstore: 3 OCI connections were cut at the drain deadline\n"
    );
}

/// The settings of a start whose `oci` interface is at `listen` with no certificate and no key.
fn plain_cli(rig: &Rig, listen: &str) -> Cli {
    Cli::from_config_text(&rig.file(Some((listen, "")))).expect("a file")
}

/// Asserts that with no certificate and no key a loopback address makes a plain front (no TLS
/// acceptor), that `/metricz` says so, and that the start line's mode is the listener's.
#[test]
fn a_loopback_front_without_a_certificate_is_plain() {
    let rig = Rig::new("oci-interface-plain");
    for listen in ["127.0.0.1:0", "127.0.0.1:9500", "[::1]:0"] {
        let built = build(&plain_cli(&rig, listen)).expect("built");
        let setup = built.oci.expect("a front");
        assert!(setup.acceptor.is_none(), "{listen}");
        assert_eq!(setup.listen, vec![listen.parse().unwrap()]);
        let shown = built.server.oci_metrics.expect("counters").render();
        assert!(
            shown.contains("tierstore_oci_front_info{mode=\"http\"} 1\n"),
            "{shown}"
        );
    }
}

/// Asserts that a plain front off loopback is not refused: the start succeeds with no TLS
/// acceptor and `/metricz` says `http`, whatever the address (the unspecified addresses and the
/// documentation ranges of both IP versions), and each of these addresses has a warning to print.
#[test]
fn a_plain_front_off_loopback_starts_with_a_warning() {
    let rig = Rig::new("oci-interface-plain-off");
    for listen in [
        "0.0.0.0:0",
        "[::]:0",
        "192.0.2.1:9500",
        "[2001:db8::1]:9500",
    ] {
        let built = build(&plain_cli(&rig, listen)).expect("built");
        let setup = built.oci.expect("a front");
        assert!(setup.acceptor.is_none(), "{listen}");
        assert_eq!(setup.listen, vec![listen.parse().unwrap()]);
        let warning = plain_warning(setup.listen[0]).expect("a warning");
        assert!(warning.contains(listen), "{warning}");
        let shown = built.server.oci_metrics.expect("counters").render();
        assert!(
            shown.contains("tierstore_oci_front_info{mode=\"http\"} 1\n"),
            "{shown}"
        );
    }
}

/// Asserts that a certificate and key keep the front HTTPS at an address that is not loopback,
/// with the checks of the certificate as before.
#[test]
fn a_certificate_and_key_allow_an_address_off_loopback() {
    let rig = Rig::new("oci-interface-tls-wide");
    let wide = leaf(&rig.dir, "wide", &ca(), vec![ip("192.0.2.1")]);
    let mut cli = rig.cli("");
    cli.oci_listen = vec!["192.0.2.1:9500".parse().expect("an address")];
    cli.oci_tls_cert = Some(wide.cert);
    cli.oci_tls_key = Some(wide.key);
    let setup = build(&cli).expect("built").oci.expect("a front");
    assert!(setup.acceptor.is_some());
    // A certificate for another address is refused as before.
    let mut cli = rig.cli("");
    cli.oci_listen = vec!["192.0.2.1:9500".parse().expect("an address")];
    let (status, why) = refused(&cli);
    assert_eq!(status, 2, "{why}");
    assert!(why.contains("no IP subjectAltName for 192.0.2.1"), "{why}");
}

/// Asserts that `params.require_auth` makes the front ask for keys and that the front is
/// anonymous without it (and with `false`): the setup holds the keys of the server only with it.
#[test]
fn the_param_makes_the_front_ask_for_keys() {
    let rig = Rig::new("oci-params-auth");
    let off = build(&rig.cli("")).expect("built").oci.expect("a front");
    assert!(off.auth.is_none());
    let off = build(&rig.cli(", require_auth = false")).expect("built");
    assert!(off.oci.expect("a front").auth.is_none());
    let on = build(&rig.cli(", require_auth = true")).expect("built");
    assert!(on.oci.expect("a front").auth.is_some());
}

/// Asserts that keys are not asked for over plain HTTP off loopback, where Basic would carry the
/// secret in the clear (exit 2, naming the address and the way out), and are over loopback and
/// over TLS at any address.
#[test]
fn the_keys_are_not_asked_for_in_the_clear() {
    let rig = Rig::new("oci-params-auth-clear");
    for listen in ["0.0.0.0:0", "[::]:0", "192.0.2.1:9500"] {
        let text = rig.file(Some((listen, ", require_auth = true")));
        let cli = Cli::from_config_text(&text).expect("a file");
        let (status, why) = refused(&cli);
        assert_eq!(status, 2, "{listen}: {why}");
        assert!(why.contains(listen), "{why}");
        assert!(why.contains("--oci-tls-cert and --oci-tls-key"), "{why}");
    }
    for listen in ["127.0.0.1:0", "[::1]:0"] {
        let text = rig.file(Some((listen, ", require_auth = true")));
        let cli = Cli::from_config_text(&text).expect("a file");
        let setup = build(&cli).expect("built").oci.expect("a front");
        assert!(setup.auth.is_some(), "{listen}");
    }
    let wide = leaf(&rig.dir, "wide-auth", &ca(), vec![ip("192.0.2.1")]);
    let mut cli = rig.cli(", require_auth = true");
    cli.oci_listen = vec!["192.0.2.1:9500".parse().expect("an address")];
    cli.oci_tls_cert = Some(wide.cert);
    cli.oci_tls_key = Some(wide.key);
    let setup = build(&cli).expect("built").oci.expect("a front");
    assert!(setup.auth.is_some());
}

//! The settings of the trees front: its defaults, its TLS files and the warning for plain text.

use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tracing::Level;
use tracing::subscriber::with_default;

use super::rig::config;
use super::setup::{DEFAULT_CONNECTIONS, DEFAULT_LIFETIME, DEFAULT_MAX_REQUEST_BYTES};
use super::{TreesFlags, trees_setup_of};
use crate::server::build_info::stderr_log::LineLog;
use crate::server::cli::exit_status;
use crate::server::oci::test_pki::{ca, dns, ip, leaf};

/// A writer whose bytes the test reads back: the operator's stderr.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<Vec<u8>>>);

impl Stderr {
    fn text(&self) -> String {
        let bytes = self.0.lock().expect("the buffer is not poisoned").clone();
        String::from_utf8(bytes).expect("the lines are text")
    }
}

impl Write for Stderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut held = self.0.lock().expect("the buffer is not poisoned");
        held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn at(text: &str) -> TreesFlags {
    TreesFlags {
        listen: vec![text.parse().expect("an address")],
        ..TreesFlags::default()
    }
}

/// Asserts a node with no `trees` interface has no front, and one with an interface and nothing
/// else given has the defaults of the design: 64 connections, an hour of lifetime, 256 MiB of
/// request, the server's handshake time and drain deadline, and plain HTTP/2.
#[test]
fn no_interface_is_no_front_and_a_bare_one_has_the_defaults() {
    let server = config();
    assert!(
        trees_setup_of(&TreesFlags::default(), &server, true)
            .expect("setup")
            .is_none()
    );
    let setup = trees_setup_of(&at("127.0.0.1:9600"), &server, true)
        .expect("setup")
        .expect("a front");
    assert_eq!(
        setup.listen,
        vec!["127.0.0.1:9600".parse::<SocketAddr>().expect("an address")]
    );
    assert!(setup.acceptor.is_none());
    assert_eq!(setup.limits.max_connections, DEFAULT_CONNECTIONS as usize);
    assert_eq!(setup.limits.lifetime, DEFAULT_LIFETIME);
    assert_eq!(setup.limits.handshake_timeout, server.oci_handshake_timeout);
    assert_eq!(setup.limits.drain_deadline, server.drain_deadline);
    assert_eq!(setup.max_request_bytes as u64, DEFAULT_MAX_REQUEST_BYTES);
    assert!(setup.core.is_none());
}

/// Asserts what the interface gives replaces the defaults, and a request limit too big for the
/// platform's `usize` is the largest there is.
#[test]
fn what_the_interface_gives_replaces_the_defaults() {
    let mut flags = at("127.0.0.1:9600");
    flags.max_connections = Some(3);
    flags.connection_lifetime = Some(Duration::from_secs(5));
    flags.max_request_bytes = Some(1024);
    let setup = trees_setup_of(&flags, &config(), false)
        .expect("setup")
        .expect("a front");
    assert_eq!(setup.limits.max_connections, 3);
    assert_eq!(setup.limits.lifetime, Duration::from_secs(5));
    assert_eq!(setup.max_request_bytes, 1024);
}

/// Asserts a certificate and key that name the address make an acceptor, a leaf with a DNS name
/// serves any address, and one that names no address of the front is refused with the param that
/// names the file, as invalid flags (exit 2); a file that is not there is exit 1.
#[test]
fn the_tls_files_are_checked_as_the_other_fronts_are() {
    let dir = TestDir::under(std::env::temp_dir(), "trees-setup-tls");
    let authority = ca();
    let named = leaf(dir.path(), "named", &authority, vec![ip("127.0.0.1")]);
    let dns_named = leaf(dir.path(), "dns", &authority, vec![dns("trees.test")]);
    let stranger = leaf(dir.path(), "stranger", &authority, vec![ip("192.0.2.8")]);
    let server = config();
    let with = |files: &crate::server::oci::test_pki::Leaf| TreesFlags {
        tls_cert: Some(files.cert.clone()),
        tls_key: Some(files.key.clone()),
        ..at("127.0.0.1:9600")
    };
    for files in [&named, &dns_named] {
        let setup = trees_setup_of(&with(files), &server, true)
            .expect("setup")
            .expect("a front");
        assert!(setup.acceptor.is_some());
    }
    let refused = trees_setup_of(&with(&stranger), &server, true)
        .err()
        .expect("refused");
    let text = refused.to_string();
    assert!(text.starts_with("params.tls_cert_file "), "{text}");
    assert!(
        text.contains("no IP subjectAltName for 127.0.0.1"),
        "{text}"
    );
    assert_eq!(exit_status(&refused), 2);

    let missing = TreesFlags {
        tls_cert: Some(dir.path().join("missing.pem")),
        tls_key: Some(named.key.clone()),
        ..at("127.0.0.1:9600")
    };
    let refused = trees_setup_of(&missing, &server, true)
        .err()
        .expect("refused");
    assert!(refused.to_string().starts_with("params.tls_cert_file "));
    assert_eq!(exit_status(&refused), 1);
}

/// Asserts plain HTTP/2 off loopback is said once for each such address on stderr, and not for
/// loopback, and that `--check-config` (no warning asked) says nothing.
#[test]
fn plain_text_off_loopback_is_warned_of() {
    let server = config();
    let warned = |flags: &TreesFlags, warn: bool| {
        let stderr = Stderr::default();
        let logged = LineLog::new(stderr.clone(), Level::INFO);
        with_default(logged, || {
            trees_setup_of(flags, &server, warn).expect("setup")
        });
        stderr.text()
    };
    let mut flags = at("0.0.0.0:9600");
    flags
        .listen
        .push("127.0.0.1:9601".parse().expect("an address"));
    flags
        .listen
        .push("192.0.2.9:9602".parse().expect("an address"));
    let text = warned(&flags, true);
    let wanted = "tierstore: warning: the trees interface at 0.0.0.0:9600 serves plain HTTP/2 on a non-loopback address; give params tls_cert_file and tls_key_file to serve TLS\n\
                  tierstore: warning: the trees interface at 192.0.2.9:9602 serves plain HTTP/2 on a non-loopback address; give params tls_cert_file and tls_key_file to serve TLS\n";
    assert_eq!(text, wanted);
    assert_eq!(warned(&at("127.0.0.1:9600"), true), "");
    assert_eq!(warned(&flags, false), "");
}

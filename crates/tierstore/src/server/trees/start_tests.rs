//! The interface in the start of a node, from the text of its file.

use std::sync::Arc;

use tierstore_testkit::dir::TestDir;
use tierstore_trees_api::v1::GetPlacementRequest;
use tokio::sync::oneshot;
use tonic::Code;

use super::rig::{assert_refused, connect, framed, send, signed_request};
use crate::server::ServerConfig;
use crate::server::cache_cli::serve_cache;
use crate::server::cache_cli_tests::{Node, QUICK};
use crate::server::cli::{checked, exit_status};
use crate::server::config::{Settings, settings_from_text};
use crate::server::local_cache::{Disk, SystemDisk};
use crate::server::oci::test_pki::{ca, dns, ip, leaf};
use crate::{Backend, MemoryBackend, SystemClock};

const KEY: (&str, &str) = ("AKIDCACHE", "cache-secret-0123456789");

fn trees_text(node: &Node, trees: &str) -> String {
    let more = format!("[[interface]]\ntype = \"trees\"\nparams = {{ {trees} }}\n");
    format!("{more}{}", node.text("", ""))
}

/// Asserts a build node whose file has a `trees` interface serves it: the address is among the
/// start's addresses, a call that is not signed is `UNAUTHENTICATED`, a call signed with the node's
/// key is `UNAVAILABLE` `NotServing` since the node has no tree core, and the node drains.
#[tokio::test]
async fn a_build_node_serves_the_trees_interface_of_its_file() {
    let node = Node::new("trees-start");
    let text = trees_text(&node, "addresses = [\"127.0.0.1:0\"]");
    let settings = settings_from_text(&text, QUICK)
        .expect("settings")
        .into_cache()
        .expect("a build node");
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addresses) = oneshot::channel();
    let running = serve_cache(
        &settings,
        upstream,
        disk,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        let bound = addresses.await.expect("a start");
        assert_eq!(bound.trees_all.len(), 1);
        let addr = bound.trees_all[0];
        let config = ServerConfig::new("tier-bucket", KEY.0, KEY.1, Arc::new(SystemClock));
        let authority = addr.to_string();
        let mut sender = connect(addr, None).await;
        let message = framed(&GetPlacementRequest::default());
        let mut bare = signed_request(&config, KEY, &authority, "GetPlacement", message.clone());
        bare.headers_mut().remove("authorization");
        let answer = send(&mut sender, bare).await;
        assert_eq!(answer.status.code(), Code::Unauthenticated);
        let signed = signed_request(&config, KEY, &authority, "GetPlacement", message);
        let answer = send(&mut sender, signed).await;
        assert_refused(&answer.status, Code::Unavailable, "NotServing");
        drop(sender);
        let _ = stop.send(());
    };
    let (drained, ()) = tokio::join!(running, driving);
    assert_eq!(drained.expect("a drain").abandoned, 0);
}

/// Asserts `--check-config` refuses a `trees` interface whose certificate does not name its address
/// as the other TLS fronts are refused, exit status 2, and accepts one that does and one with no
/// TLS; a combined node's file is checked the same.
#[test]
fn check_config_judges_the_tls_files_of_the_trees_interface() {
    let node = Node::new("trees-check");
    let dir = TestDir::under(std::env::temp_dir(), "trees-check-pki");
    let authority = ca();
    let good = leaf(dir.path(), "good", &authority, vec![ip("127.0.0.1")]);
    let wrong = leaf(dir.path(), "wrong", &authority, vec![ip("192.0.2.8")]);
    let named = leaf(dir.path(), "named", &authority, vec![dns("trees.test")]);
    let params = |files: &crate::server::oci::test_pki::Leaf| {
        format!(
            "addresses = [\"127.0.0.1:9600\"], tls_cert_file = \"{}\", tls_key_file = \"{}\"",
            files.cert.display(),
            files.key.display()
        )
    };
    let load = |params: &str| -> Settings {
        settings_from_text(&trees_text(&node, params), QUICK).expect("settings")
    };
    checked(&load("addresses = [\"127.0.0.1:9600\"]")).expect("plain is fine");
    checked(&load(&params(&good))).expect("a named address");
    checked(&load(&params(&named))).expect("a DNS name");
    let refused = checked(&load(&params(&wrong))).expect_err("refused");
    assert!(
        refused.to_string().starts_with("params.tls_cert_file "),
        "{refused}"
    );
    assert_eq!(exit_status(&refused), 2);
}

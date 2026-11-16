//! The `tierstore` binary started from the file of a node over the cold tier alone: one process
//! whose fronts serve a cold directory, with no hot tier and no cache.

use bytes::Bytes;
use objstore_s3::{ByteRange, ObjectKey, ObjectStore};
use rustix::process::Signal;

use crate::binary::{run, start};
use crate::files::Files;

/// The file of a node over the cold directory beside the key of `files`, with `server_extra` in
/// `[server]`.
fn node_file(files: &Files, name: &str, server_extra: &str) -> String {
    let root = files.key.parent().unwrap().to_path_buf();
    let text = format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "bin-cold"
bucket          = "tier-bucket"
access_key_file = "{key}"
{server_extra}

[cold_store]
kind = "dir"
path = "{cold}"
"#,
        key = files.key.display(),
        cold = root.join("cold").display(),
    );
    files.secret(name, &text).display().to_string()
}

fn args(config: &str) -> Vec<String> {
    ["--config", config, "--drain-lb-wait", "50ms"]
        .map(str::to_owned)
        .to_vec()
}

/// Asserts that a node over the cold tier announces `mode=cold-only`, takes a put into the cold
/// directory and answers the read of it, makes no hot or cache directory, deletes the object on a
/// delete, and exits with status 0 on SIGTERM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_over_the_cold_tier_puts_reads_deletes_and_exits_0_on_sigterm() {
    let files = Files::new("bin-cold-node");
    let root = files.key.parent().unwrap().to_path_buf();
    let config = node_file(&files, "cold.toml", "");
    let mut running = start(&args(&config));
    assert!(
        running.line().contains(" mode=cold-only commit="),
        "{}",
        running.line()
    );
    let store = running.store();
    let key = ObjectKey::new("cas/obj").unwrap();
    let body = Bytes::from_static(b"cold bytes");
    store.put_new(&key, body.clone(), None).await.unwrap();
    let all = ByteRange::new(0, 10).unwrap();
    assert_eq!(store.get_range(&key, all).await.unwrap(), body);
    assert!(root.join("cold").read_dir().unwrap().next().is_some());
    assert!(!root.join("hot").exists());
    assert!(!root.join("cache").exists());
    store.delete(&key).await.unwrap();
    assert!(store.get_range(&key, all).await.is_err());
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that `--check-config` of a good file prints nothing and exits 0, and that a file whose
/// request timeout is zero is refused naming the key and not a flag, with nothing on stdout: with
/// status 2 by `--check-config`, and with status 1 by a start, as a storage node's and a build
/// node's start refuse it.
#[test]
fn a_cold_file_that_cannot_run_exits_2_with_keys_not_flags() {
    let files = Files::new("bin-cold-refused");
    let good = node_file(&files, "good.toml", "");
    let checking = ["--config", good.as_str(), "--check-config"].map(str::to_owned);
    assert_eq!(run(&checking), (0, String::new(), String::new()));

    let zero = node_file(&files, "zero.toml", "s3_request_timeout = \"0s\"");
    let checking = ["--config", zero.as_str(), "--check-config"].map(str::to_owned);
    for (given, status) in [(checking.to_vec(), 2), (args(&zero), 1)] {
        let (code, out, err) = run(&given);
        assert_eq!((code, out.as_str()), (status, ""), "{err}");
        assert!(
            err.contains("s3_request_timeout must be longer than zero"),
            "{err}"
        );
        assert!(!err.contains("--"), "{err}");
    }
}

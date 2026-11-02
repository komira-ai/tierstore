//! The `tierstore` binary started from the file of a storage node with a cache in front of its
//! own proxy: one process over two directory tiers.

use bytes::Bytes;
use objstore_s3::{ByteRange, ObjectKey, ObjectStore};
use reqwest::Client;
use rustix::process::Signal;

use crate::binary::{run, start};
use crate::files::Files;

/// The file of a node with a cache: directory tiers and the cache beside them in the scratch
/// directory of `files`, and one policy that keeps `cas/`.
fn node_file(files: &Files) -> String {
    let root = files.key.parent().unwrap().to_path_buf();
    let text = format!(
        r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "bin-combined"
bucket          = "tier-bucket"
access_key_file = "{key}"

[hot_store]
kind   = "dir"
path   = "{hot}"
budget = "1MiB"

[cold_store]
kind = "dir"
path = "{cold}"

[local_cache]
path   = "{cache}"
budget = "1MiB"

[[policy]]
prefix = "cas/"
cache  = true
"#,
        key = files.key.display(),
        hot = root.join("hot").display(),
        cold = root.join("cold").display(),
        cache = root.join("cache").display(),
    );
    files.secret("combined.toml", &text).display().to_string()
}

fn args(config: &str) -> Vec<String> {
    ["--config", config, "--drain-lb-wait", "50ms"]
        .map(str::to_owned)
        .to_vec()
}

/// Asserts that a node with a cache announces `mode=storage-cache`, takes a put and answers the
/// reads of it, shows the proxy's counters and the cache's on `/metricz`, keeps the object in the
/// cache directory and the hot tier, deletes it through both on a delete, and exits with status 0
/// on SIGTERM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_combined_node_puts_reads_deletes_and_exits_0_on_sigterm() {
    let files = Files::new("bin-combined-node");
    let root = files.key.parent().unwrap().to_path_buf();
    let config = node_file(&files);
    let mut running = start(&args(&config));
    assert!(
        running.line().contains("mode=storage-cache"),
        "{}",
        running.line()
    );
    let store = running.store();
    let key = ObjectKey::new("cas/obj").unwrap();
    let body = Bytes::from_static(b"combined bytes");
    store.put_new(&key, body.clone(), None).await.unwrap();
    let all = ByteRange::new(0, 14).unwrap();
    assert_eq!(store.get_range(&key, all).await.unwrap(), body);
    assert_eq!(store.get_range(&key, all).await.unwrap(), body);
    assert!(root.join("hot").read_dir().unwrap().next().is_some());
    assert!(root.join("cache").join("obj").is_dir());

    let metrics = Client::new()
        .get(format!("http://{}/metricz", running.addr()))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), 200);
    let text = metrics.text().await.unwrap();
    assert!(text.contains("tierstore_reads_total"), "{text}");
    assert!(text.contains("tierstore_cache_hits_total"), "{text}");

    store.delete(&key).await.unwrap();
    assert!(store.get_range(&key, all).await.is_err());
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that a file with the three tables and `write_through = false` exits 2 naming the key
/// and not a flag, prints nothing on stdout, and that `--check-config` of a good file prints
/// nothing and exits 0 without making the cache directory.
#[test]
fn a_combined_file_that_cannot_run_exits_2_with_keys_not_flags() {
    let files = Files::new("bin-combined-refused");
    let root = files.key.parent().unwrap().to_path_buf();
    let good = node_file(&files);
    let checking = ["--config", good.as_str(), "--check-config"].map(str::to_owned);
    assert_eq!(run(&checking), (0, String::new(), String::new()));
    assert!(!root.join("cache").exists());

    let text = std::fs::read_to_string(&good).unwrap();
    let from = "budget = \"1MiB\"\n\n[[policy]]";
    assert!(text.contains(from));
    let to = "budget = \"1MiB\"\nwrite_through = false\n\n[[policy]]";
    let refused = files
        .secret("refused.toml", &text.replacen(from, to, 1))
        .display()
        .to_string();
    let (code, out, err) = run(&args(&refused));
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("local_cache.write_through"), "{err}");
    assert!(!err.contains("--"), "{err}");
}

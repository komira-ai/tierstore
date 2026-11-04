//! The `tierstore` binary started from a configuration file (`--config`): it serves what the file
//! says, and refuses what the file must not say and the flags that went.

use std::path::Path;

use bytes::Bytes;
use objstore_s3::s3::S3Store;
use objstore_s3::{ByteRange, KeyPrefix, ObjectKey, ObjectStore, PageSize};
use rustix::process::Signal;

use crate::binary::{run, start};
use crate::files::{Files, Tables};

/// The calls of one session, and what each answered: two puts, a put of a key that is there,
/// a ranged read of an object and of one that is not there, a listing, a delete and a listing
/// again. The transcript has no address and no time in it, so two servers over the same
/// settings give the same one.
async fn session(store: &S3Store) -> Vec<String> {
    let key = |name: &str| ObjectKey::new(name).unwrap();
    let mut said = Vec::new();
    let (a, b) = (key("cas/a"), key("cas/b"));
    let alpha = store.put_new(&a, Bytes::from_static(b"alpha"), None).await;
    said.push(format!("put a: {alpha:?}"));
    let bravo = store.put_new(&b, Bytes::from_static(b"bravo!"), None).await;
    said.push(format!("put b: {bravo:?}"));
    let again = store.put_new(&a, Bytes::from_static(b"other"), None).await;
    said.push(format!("put a again: {again:?}"));
    let part = ByteRange::new(1, 3).unwrap();
    said.push(format!(
        "get a 1..4: {:?}",
        store.get_range(&key("cas/a"), part).await
    ));
    let first = ByteRange::new(0, 1).unwrap();
    said.push(format!(
        "get absent: {:?}",
        store.get_range(&key("cas/none"), first).await
    ));
    let prefix = KeyPrefix::new("cas/").unwrap();
    for step in ["list", "list after delete"] {
        let page = store.list(&prefix, None, PageSize::MAX).await.unwrap();
        let names: Vec<String> = page
            .objects
            .iter()
            .map(|object| format!("{} {}", object.key.as_str(), object.size))
            .collect();
        said.push(format!("{step}: {names:?}"));
        said.push(format!("delete b: {:?}", store.delete(&key("cas/b")).await));
    }
    said
}

/// Starts the binary over these arguments, runs the session against it, drains it with SIGTERM
/// and checks that it exits with status 0.
async fn served(args: &[String]) -> Vec<String> {
    let mut running = start(args);
    let said = session(&running.store()).await;
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
    said
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// The text of a file for two directory tiers under `root`, written by hand, with the budget as
/// a string with a unit.
fn directory_file(key: &Path, root: &Path) -> String {
    format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\n\
         [server]\nproxy_id = \"by-hand\"\nbucket = \"tier-bucket\"\n\
         access_key_file = \"{}\"\n\n[hot_store]\nkind = \"dir\"\npath = \"{}\"\nbudget = \"1MiB\"\n\n\
         [cold_store]\nkind = \"dir\"\npath = \"{}\"\n",
        key.display(),
        root.join("hot").display(),
        root.join("cold").display(),
    )
}

/// Asserts that the binary started from a file answers the session as the file says: the puts,
/// the refusal of the second put of a key, the ranged reads, the listings and the delete; and
/// that a key of the file that is not the default (`copy_parallel`) is taken.
#[tokio::test]
async fn a_server_started_from_a_file_serves_the_session() {
    let files = Files::new("cfg-serve");
    let tables = Tables {
        hot: "kind = \"memory\"\nbudget = 1048576\ncopy_parallel = 3".to_owned(),
        ..Tables::default()
    };
    let said = served(&files.flags_of(&tables, &["--drain-lb-wait", "50ms"])).await;
    assert!(said[0].contains("Ok"), "{said:?}");
    assert!(said[2].contains("AlreadyExists"), "{said:?}");
    assert!(said[4].contains("NotFound"), "{said:?}");
    assert_eq!(said[5], "list: [\"cas/a 5\", \"cas/b 6\"]");
    assert_eq!(said[7], "list after delete: [\"cas/a 5\"]");
}

/// Asserts that a file written by hand, with directory tiers and a budget written `1MiB`, starts
/// the binary, which keeps what it is given in the hot tier's directory, on disk.
#[tokio::test]
async fn a_file_written_by_hand_with_directory_tiers_keeps_objects_on_disk() {
    let files = Files::new("cfg-dirs");
    let root = files.key.parent().unwrap().to_path_buf();
    let file = files.secret("by-hand.toml", &directory_file(&files.key, &root));
    let args = strings(&[
        "--config",
        &file.display().to_string(),
        "--drain-lb-wait",
        "50ms",
    ]);
    crate::files::activate_dir(&root.join("hot")).await;
    let said = served(&args).await;
    assert!(said[0].contains("Ok"), "{said:?}");
    assert!(root.join("hot/cas~a").is_file());
}

/// Asserts the refusals of a start with a file, each with status 2, nothing on stdout and a
/// message that names what is wrong: a data path flag given with the file (the first one named,
/// with its key), a file that is not there, a mistyped key, an inline secret, and `[local_cache]`
/// along with `[hot_store]` and nothing a cache keeps.
#[test]
fn a_start_with_a_file_that_must_be_refused_says_why() {
    let files = Files::new("cfg-refused");
    let good = directory_file(&files.key, files.key.parent().unwrap());
    let file = files.secret("good.toml", &good);
    let path = file.display().to_string();

    let args = strings(&[
        "--config",
        &path,
        "--listen",
        "127.0.0.1:0",
        "--hot",
        "memory",
    ]);
    let (code, out, err) = run(&args);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(
        err.starts_with("tierstore: --listen was removed: "),
        "{err}"
    );
    let (code, _, err) = run(&strings(&["--config", "/no/such/tierstore.toml"]));
    assert_eq!(code, 2);
    assert!(err.contains("/no/such/tierstore.toml"), "{err}");

    let refused = [
        ("[server]\n", "[server]\nslot = \"x\"\n", "slot"),
        (
            "[hot_store]\n",
            "[hot_store]\nsecret_access_key = \"x\"\n",
            "hot_store.secret_access_key",
        ),
        (
            "[cold_store]\n",
            "[local_cache]\npath = \"/c\"\n\n[cold_store]\n",
            "no policy has cache = true",
        ),
    ];
    for (from, to, want) in refused {
        let bad = files.secret("bad.toml", &good.replacen(from, to, 1));
        let (code, out, err) = run(&strings(&["--config", &bad.display().to_string()]));
        assert_eq!((code, out.as_str()), (2, ""), "{want}");
        assert!(err.contains(want) && err.contains("bad.toml"), "{err}");
    }
}

/// Asserts that a start from a file whose settings the checks of the start refuse (water marks
/// out of order) exits with status 1 and says so in the keys of the file, not in flags.
#[test]
fn a_refusal_of_the_start_speaks_of_the_keys_of_the_file() {
    let files = Files::new("cfg-keys");
    let text = directory_file(&files.key, files.key.parent().unwrap());
    let text = text.replacen("budget = \"1MiB\"", "budget = \"1MiB\"\nlow_water = 90", 1);
    let file = files.secret("marks.toml", &text);
    let (code, out, err) = run(&strings(&["--config", &file.display().to_string()]));
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.contains("hot_store.low_water"), "{err}");
    assert!(err.contains("hot_store.high_water"), "{err}");
    assert!(!err.contains("--low-water"), "{err}");
}

/// Asserts that a file with a key this release does not know is checked and started as the file
/// without it: `--check-config` exits 0, says nothing on stdout and says on stderr, once for each
/// key and with exactly this text, that the key is not known and is ignored, and a file with only
/// known keys says nothing.
#[test]
fn a_file_with_an_unknown_key_is_checked_with_one_warning_for_each_key() {
    let files = Files::new("cfg-unknown-keys");
    let good = directory_file(&files.key, files.key.parent().unwrap());
    let plain = files.secret("plain.toml", &good);
    let args = |file: &Path| strings(&["--config", &file.display().to_string(), "--check-config"]);
    assert_eq!(run(&args(&plain)), (0, String::new(), String::new()));

    let text = good.replacen("[server]\n", "[server]\nnew_knob = 1\n", 1) + "\n[future]\nx = 1\n";
    let file = files.secret("future.toml", &text);
    let (code, out, err) = run(&args(&file));
    let line = |key: &str| {
        format!("tierstore: config key {key} is not known to this release and is ignored\n")
    };
    assert_eq!((code, out), (0, String::new()));
    assert_eq!(err, line("future") + &line("server.new_knob"));
}

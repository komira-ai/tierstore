//! The files and the flags that a start of the binary needs.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use tierstore::{Backend as _, DirBackend};
use tierstore_testkit::dir::TestDir;

/// The access key in the key file.
pub const BIN_KEY_ID: &str = "AKIDBIN";

pub const BIN_SECRET: &str = "bin-secret-0123456789";

/// Writes the active format into the directory tier at `hot`, as `tierstore format-epoch --activate`
/// does for a new store, so that a node started over it takes writes.
pub async fn activate_dir(hot: &std::path::Path) {
    let tier = DirBackend::open(hot).unwrap();
    let active = Bytes::from_static(br#"{"epoch":2,"phase":"active"}"#);
    tier.put("_tiering/format", active).await.unwrap();
}

/// The parts of the file of a start that a test changes. The rest is the same in every start: a
/// bucket called `tier-bucket` and the key file of [`Files`].
#[derive(Clone, Debug)]
pub struct Tables {
    /// The addresses of the one `s3` interface the file always has.
    pub listen: Vec<String>,
    /// More tables after the interface, whole: further `[[interface]]` entries, `[[policy]]`.
    pub more: String,
    /// The id of the proxy.
    pub proxy_id: String,
    /// Lines of `[server]` after the id, the bucket and the key file.
    pub server: String,
    /// The lines of `[hot_store]`.
    pub hot: String,
    /// The lines of `[cold_store]`.
    pub cold: String,
}

impl Default for Tables {
    /// Two memory tiers, and the S3 front on a loopback address the system picks.
    fn default() -> Self {
        Self {
            listen: vec!["127.0.0.1:0".to_owned()],
            more: String::new(),
            proxy_id: "bin".to_owned(),
            server: String::new(),
            hot: "kind = \"memory\"\nbudget = 1048576".to_owned(),
            cold: "kind = \"memory\"".to_owned(),
        }
    }
}

/// The addresses as the text of a TOML list: `"a", "b"`.
fn quoted(addresses: &[&str]) -> String {
    let each: Vec<String> = addresses
        .iter()
        .map(|address| format!("{address:?}"))
        .collect();
    each.join(", ")
}

/// An `[[interface]]` entry of type `oci` at `listen`, with `params` after the addresses (each
/// after a comma), as a table of [`Tables::more`].
pub fn oci_interface(listen: &[&str], params: &str) -> String {
    let listen = quoted(listen);
    format!("\n[[interface]]\ntype = \"oci\"\nparams = {{ addresses = [{listen}]{params} }}\n")
}

/// A scratch directory for a Unix socket called `file`, and the socket's path in it: under the
/// system's temporary directory, or under the build's when the socket's path there is too long to
/// bind (a socket's path has a short limit, which a deep checkout or a long `TMPDIR` passes).
pub fn socket_dir(name: &str, file: &str) -> (TestDir, PathBuf) {
    let system = std::env::temp_dir();
    let build = Path::new(env!("CARGO_TARGET_TMPDIR"));
    TestDir::for_socket(&[&system, build], name, file)
}

/// A scratch directory with a key file in it, and the file and the flags of a start that uses it.
pub struct Files {
    /// Removes the directory when the files are dropped.
    _dir: TestDir,
    pub key: PathBuf,
}

impl Files {
    pub fn new(name: &str) -> Self {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let key = dir.path().join("key");
        std::fs::write(&key, format!("{BIN_KEY_ID}\n{BIN_SECRET}\n")).unwrap();
        std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
        Self { _dir: dir, key }
    }

    /// A file called `name` next to the key file, that only its owner can read, with this text.
    pub fn secret(&self, name: &str, text: &str) -> PathBuf {
        let path = self.key.with_file_name(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        path
    }

    /// The text of the file of a start with these tables.
    pub fn text(&self, tables: &Tables) -> String {
        let Tables {
            listen,
            more,
            proxy_id,
            server,
            hot,
            cold,
        } = tables;
        let listen = quoted(&listen.iter().map(String::as_str).collect::<Vec<_>>());
        format!(
            "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [{listen}] }}\n{more}\n\
             [server]\nproxy_id = \"{proxy_id}\"\nbucket = \"tier-bucket\"\naccess_key_file = \"{}\"\n{server}\n\n\
             [hot_store]\n{hot}\n\n[cold_store]\n{cold}\n",
            self.key.display()
        )
    }

    /// Writes the file of a start with these tables as `tierstore.toml` beside the key file, and
    /// returns the flags of that start, then `extra` (flags of the run).
    pub fn flags_of(&self, tables: &Tables, extra: &[&str]) -> Vec<String> {
        let file = self.secret("tierstore.toml", &self.text(tables));
        let mut flags = vec!["--config".to_owned(), file.display().to_string()];
        flags.extend(extra.iter().map(|flag| (*flag).to_owned()));
        flags
    }

    /// The flags of a start with two memory tiers, then `extra` (flags of the run).
    pub fn flags(&self, extra: &[&str]) -> Vec<String> {
        self.flags_of(&Tables::default(), extra)
    }
}

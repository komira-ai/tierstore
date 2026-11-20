//! The trees of the replay, load and command line tests, on the in-process fake store.
//!
//! `ROOT` is a tree of two directories and four files of three packs (cells are 1 MiB):
//!
//! | path    | pack | offset    | size      |
//! |---------|------|-----------|-----------|
//! | `a`     | A    | 0         | 2 621 440 |
//! | `b`     | B    | 0         | 2 097 252 |
//! | `sub/c` | A    | 2 621 440 | 524 288   |
//! | `sub/d` | C    | 0         | 100       |
//!
//! plus `e` (no bytes) and the link `l`. Pack A is 3 MiB (3 cells), B is 2 MiB and 100 bytes (3
//! cells, the last short) and C is 100 bytes. `ROOT2` is one file, `x`, the whole of pack C. The
//! fake holds the packs and manifests in chunks of 1 MiB on volumes 7 and 8, each on both of its
//! two volume servers (nodes 2 and 3).

use std::sync::Arc;

use readbench::http::HttpClient;
use readbench::replay::Store;
use readbench::swfs::{FilerFetch, VolumeServer, load_map};
use serde_json::json;

use crate::support::Fake;

pub const BUCKET: &str = "kbf-cas";
pub const ROOT: &str = "1111111111111111111111111111111111111111111111111111111111111111";
pub const SUB: &str = "2222222222222222222222222222222222222222222222222222222222222222";
pub const ROOT2: &str = "3333333333333333333333333333333333333333333333333333333333333333";
pub const PACK_A: &str = "cas/b/a/pack";
pub const PACK_B: &str = "cas/b/b/pack";
pub const PACK_C: &str = "cas/b/c/pack";
pub const SIZE_A: u64 = 3 << 20;
pub const SIZE_B: u64 = (2 << 20) + 100;
pub const SIZE_C: u64 = 100;
/// The bytes of the four files of `ROOT`.
pub const ROOT_BYTES: u64 = 2_621_440 + SIZE_B + 524_288 + 100;

/// `n` bytes that differ from cell to cell and with `salt`.
pub fn data(n: u64, salt: u64) -> Vec<u8> {
    (0..n)
        .map(|i| (i.wrapping_mul(131) + (i >> 20) * 7 + salt) as u8)
        .collect()
}

/// What a test changes in the store.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Everything in place.
    Whole,
    /// The pack is not there.
    Without(&'static str),
    /// The pack is held with only this many bytes.
    Short(&'static str, u64),
    /// The pack has the right size and one byte of its first cell changed.
    Corrupt(&'static str),
}

fn file(name: &str, size: u64, pack: &str, offset: u64) -> serde_json::Value {
    json!({"name": name, "kind": "file", "sha256": hex::encode([7u8; 32]), "size": size,
        "exec": false, "source": {"key": pack, "offset": offset}})
}

fn manifests() -> Vec<(String, serde_json::Value)> {
    vec![
        (
            ROOT.to_owned(),
            json!({"v": 1, "children": [
                file("a", 2_621_440, PACK_A, 0),
                file("b", SIZE_B, PACK_B, 0),
                {"name": "e", "kind": "file", "sha256": hex::encode([8u8; 32]), "size": 0, "exec": false},
                {"name": "l", "kind": "symlink", "target": "a"},
                {"name": "sub", "kind": "dir", "sha256": SUB, "size": 100},
            ]}),
        ),
        (
            SUB.to_owned(),
            json!({"v": 1, "children": [
                file("c", 524_288, PACK_A, 2_621_440),
                file("d", 100, PACK_C, 0),
            ]}),
        ),
        (
            ROOT2.to_owned(),
            json!({"v": 1, "children": [file("x", SIZE_C, PACK_C, 0)]}),
        ),
    ]
}

/// The fake store with the trees and packs, changed as `variant` says.
pub async fn fake(variant: Variant) -> Fake {
    let mut builder = Fake::builder()
        .volume_servers(2)
        .volume(7, &[0, 1], 1)
        .volume(8, &[0, 1], 2)
        .chunk_size(1 << 20);
    for (key, size, salt) in [
        (PACK_A, SIZE_A, 1),
        (PACK_B, SIZE_B, 2),
        (PACK_C, SIZE_C, 3),
    ] {
        let bytes = match variant {
            Variant::Without(gone) if gone == key => continue,
            Variant::Short(short, len) if short == key => data(len, salt),
            Variant::Corrupt(bad) if bad == key => {
                let mut bytes = data(size, salt);
                bytes[5] ^= 0xff;
                bytes
            }
            _ => data(size, salt),
        };
        builder = builder.object(BUCKET, key, bytes);
    }
    for (root, manifest) in manifests() {
        builder = builder.object(
            BUCKET,
            &format!("cas/l/{root}"),
            manifest.to_string().into_bytes(),
        );
    }
    builder.start().await
}

/// The servers of the fake as the harness numbers them: nodes 2 and 3.
pub fn servers(fake: &Fake) -> Vec<VolumeServer> {
    (0..2)
        .map(|i| VolumeServer {
            node: 2 + u8::try_from(i).expect("two servers"),
            addr: fake.volume(i).addr().to_owned(),
        })
        .collect()
}

/// The store of the fake: its filer and the map of both volume servers.
pub async fn store(fake: &Fake) -> Store {
    let http = HttpClient::new(10_000);
    let (map, failed) = load_map(&http, &servers(fake)).await;
    assert!(failed.is_empty());
    Store {
        fetch: FilerFetch {
            http: http.clone(),
            base: fake.filer(0).url("/buckets/kbf-cas"),
        },
        map: Arc::new(map),
        http,
    }
}

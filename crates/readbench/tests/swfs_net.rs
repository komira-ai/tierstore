//! The filer and the volume status pages of `swfs`, against the in-process fake store.

#![cfg(target_os = "linux")]

#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the fake")]
mod support;

use futures::TryStreamExt as _;
use readbench::http::HttpClient;
use readbench::manifest::Fetch as _;
use readbench::swfs::{FilerFetch, VolumeServer, load_map};
use support::{Fake, Kind};

const BUCKET: &str = "packs";
const KEY: &str = "cas/b/p1/pack";

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn filer(fake: &Fake) -> FilerFetch {
    FilerFetch {
        http: HttpClient::new(10_000),
        base: fake.filer(0).url("/buckets/packs"),
    }
}

fn server(node: u8, fake: &Fake, i: usize) -> VolumeServer {
    VolumeServer {
        node,
        addr: fake.volume(i).addr().to_owned(),
    }
}

#[tokio::test]
async fn the_map_gathers_every_servers_volumes_and_audits_the_copies() {
    let fake = Fake::builder()
        .volume(7, &[0, 1], 1)
        .volume(8, &[1], 2)
        .start()
        .await;
    let servers = [server(2, &fake, 0), server(3, &fake, 1)];
    let (map, failed) = load_map(&HttpClient::new(10_000), &servers).await;
    assert!(failed.is_empty());
    assert_eq!(map.volumes(), 2);
    assert_eq!(
        map.histogram().into_iter().collect::<Vec<_>>(),
        [(1, 1), (2, 1)]
    );
    let nodes: Vec<(u8, u32)> = map.replicas(7).iter().map(|r| (r.node, r.disk)).collect();
    assert_eq!(nodes, [(2, 1), (3, 1)]);
    assert_eq!(map.replicas(8)[0].disk, 2);
    assert_eq!(fake.volume(0).count(Kind::VolumeStatus), 1);
    assert_eq!(fake.volume(1).count(Kind::VolumeStatus), 1);
}

#[tokio::test]
async fn servers_that_fail_are_listed_and_the_others_are_kept() {
    let fake = Fake::builder().volume(7, &[0, 1], 1).start().await;
    let closed = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().to_string()
    };
    fake.volume(1).fail_status("/status", 200, None);
    let servers = [
        server(2, &fake, 0),
        VolumeServer {
            node: 3,
            addr: closed.clone(),
        },
        server(4, &fake, 1),
    ];
    let (map, failed) = load_map(&HttpClient::new(10_000), &servers).await;
    assert_eq!(map.replicas(7).len(), 1);
    assert_eq!(failed.len(), 2);
    assert_eq!(failed[0].0.node, 3);
    assert!(
        failed[0]
            .1
            .to_string()
            .starts_with(&format!("http://{closed}/status: transport")),
        "{}",
        failed[0].1
    );
    assert_eq!(failed[1].0.node, 4);
    assert!(
        failed[1]
            .1
            .to_string()
            .starts_with(&format!("{} /status: ", fake.volume(1).addr())),
        "{}",
        failed[1].1
    );
}

#[tokio::test]
async fn a_server_that_answers_an_error_is_listed() {
    let fake = Fake::builder().start().await;
    fake.volume(0).fail_status("/status", 500, None);
    let (map, failed) = load_map(&HttpClient::new(10_000), &[server(1, &fake, 0)]).await;
    assert_eq!(map.volumes(), 0);
    assert_eq!(failed.len(), 1);
    assert!(failed[0].1.to_string().ends_with(": status 500"));
}

#[tokio::test]
async fn the_filer_gives_an_objects_bytes_whole() {
    let fake = Fake::builder().object(BUCKET, KEY, data(10)).start().await;
    let bytes = filer(&fake).get(KEY).await.unwrap();
    assert_eq!(bytes.as_ref(), data(10).as_slice());
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 1);
}

#[tokio::test]
async fn a_missing_object_is_not_found_and_a_broken_filer_is_its_status() {
    let fake = Fake::builder().object(BUCKET, KEY, data(10)).start().await;
    let f = filer(&fake);
    let err = f.get("cas/b/none").await.unwrap_err();
    assert_eq!(err.to_string(), format!("{}/cas/b/none: not found", f.base));
    fake.filer(0)
        .fail_status(&format!("/buckets/{BUCKET}/{KEY}"), 500, None);
    let err = f.get(KEY).await.unwrap_err();
    assert_eq!(err.to_string(), format!("{}/{KEY}: status 500", f.base));
}

#[tokio::test]
async fn the_entry_lists_the_chunks_and_a_missing_object_has_none() {
    let fake = Fake::builder()
        .chunk_size(4)
        .object(BUCKET, KEY, data(10))
        .start()
        .await;
    let f = filer(&fake);
    let entry = f.entry(KEY).await.unwrap().expect("the object is there");
    assert_eq!(entry.size, 10);
    assert_eq!(entry.etag, None);
    let sizes: Vec<(u64, u64)> = entry.chunks.iter().map(|c| (c.offset, c.size)).collect();
    assert_eq!(sizes, [(0, 4), (4, 4), (8, 2)]);
    assert_eq!(entry.chunks[0].fid, fake.chunks(BUCKET, KEY)[0].fid);
    assert!(f.entry("cas/b/none").await.unwrap().is_none());
    assert_eq!(fake.filer(0).count(Kind::FilerMetadata), 2);
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 0);
}

#[tokio::test]
async fn an_entry_that_is_not_json_is_a_parse_error_naming_the_key() {
    let fake = Fake::builder().object(BUCKET, KEY, data(10)).start().await;
    fake.filer(0)
        .fail_status(&format!("/buckets/{BUCKET}/{KEY}"), 200, None);
    let err = filer(&fake).entry(KEY).await.unwrap_err();
    assert!(err.to_string().starts_with(&format!("{KEY}: ")), "{err}");
}

#[tokio::test]
async fn a_ranged_read_gives_just_those_bytes() {
    let fake = Fake::builder().object(BUCKET, KEY, data(10)).start().await;
    let answer = filer(&fake).read_range(KEY, 3, 4).await.unwrap();
    assert_eq!(answer.status, 206);
    let body: Vec<_> = answer.body.try_collect().await.unwrap();
    assert_eq!(body.concat(), data(10)[3..7].to_vec());
}

#[tokio::test]
async fn a_ranged_read_of_nothing_or_of_a_missing_object_or_past_the_end_fails() {
    let fake = Fake::builder().object(BUCKET, KEY, data(10)).start().await;
    let f = filer(&fake);
    let url = format!("{}/{KEY}", f.base);
    let err = f.read_range(KEY, 3, 0).await.unwrap_err();
    assert_eq!(err.to_string(), format!("{url}: empty range 3..3"));
    let err = f.read_range(KEY, u64::MAX, 2).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("{url}: range {}+2 runs past 2^64", u64::MAX)
    );
    let err = f.read_range("cas/b/none", 0, 4).await.unwrap_err();
    assert_eq!(err.to_string(), format!("{}/cas/b/none: not found", f.base));
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 1);
}

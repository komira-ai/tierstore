//! The HTTP client against the in-process fake store (`tests/support`), and the fake's own
//! features, each read through the client.

#![cfg(target_os = "linux")]
#![expect(
    clippy::unwrap_used,
    reason = "the helpers of this file are part of its tests: their failure is the failure reported"
)]

#[cfg(test)]
mod support;

use futures::StreamExt as _;
use futures::TryStreamExt as _;
use readbench::Error;
use readbench::http::{BodyStream, HttpClient};
use serde_json::{Value, json};
use support::{Fake, Kind, raw};

const BUCKET: &str = "packs";
const KEY: &str = "cas/b/190405-0/pack";
const PATH: &str = "/buckets/packs/cas/b/190405-0/pack";

/// Ten bytes, `0123456789`.
fn digits() -> Vec<u8> {
    b"0123456789".to_vec()
}

/// A fake with one filer, one volume server and `digits()` as the object `KEY`.
async fn one_object() -> Fake {
    Fake::builder().object(BUCKET, KEY, digits()).start().await
}

fn client() -> HttpClient {
    HttpClient::new(10_000)
}

async fn collect(body: BodyStream) -> Result<Vec<u8>, Error> {
    body.try_fold(Vec::new(), |mut all, piece| async move {
        all.extend_from_slice(&piece);
        Ok(all)
    })
    .await
}

/// What an HTTP error says went wrong: its text after the URL.
fn http_why(e: &Error) -> String {
    e.to_string()
        .split_once(": ")
        .map(|(_, why)| why.to_owned())
        .unwrap_or_default()
}

#[tokio::test]
async fn head_reports_status_length_and_unquoted_etag() {
    let fake = one_object().await;
    let head = client().head(&fake.filer(0).url(PATH)).await.expect("head");
    assert_eq!(head.status, 200);
    assert_eq!(head.len, Some(10));
    let etag = head.etag.expect("an etag");
    assert_eq!(etag.len(), 32);
    assert!(etag.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(fake.filer(0).count(Kind::FilerHead), 1);
    assert_eq!(fake.total(Kind::FilerGet), 0);
}

#[tokio::test]
async fn etag_follows_the_bytes() {
    let fake = Fake::builder()
        .object(BUCKET, "a", digits())
        .object(BUCKET, "b", digits())
        .object(BUCKET, "c", b"other".to_vec())
        .start()
        .await;
    let etag = |key: &'static str| {
        let url = fake.filer(0).url(&format!("/buckets/{BUCKET}/{key}"));
        async move { client().head(&url).await.expect("head").etag }
    };
    assert_eq!(etag("a").await, etag("b").await);
    assert_ne!(etag("a").await, etag("c").await);
}

#[tokio::test]
async fn head_of_a_missing_object_is_ok_with_status_404() {
    let fake = one_object().await;
    let head = client()
        .head(&fake.filer(0).url("/buckets/packs/nothing"))
        .await
        .expect("head");
    assert_eq!(head.status, 404);
    assert_eq!(head.etag, None);
}

#[tokio::test]
async fn head_with_another_status_is_an_error_naming_the_url() {
    let fake = one_object().await;
    fake.filer(0).fail_status(PATH, 500, None);
    let url = fake.filer(0).url(PATH);
    let e = client().head(&url).await.expect_err("a 500");
    assert_eq!(e.to_string(), format!("{url}: status 500"));
}

#[tokio::test]
async fn head_of_a_needle_has_no_etag() {
    let fake = one_object().await;
    let fid = fake.chunks(BUCKET, KEY)[0].fid.clone();
    let head = client()
        .head(&fake.volume(0).url(&format!("/{fid}")))
        .await
        .expect("head");
    assert_eq!((head.status, head.len, head.etag), (200, Some(10), None));
    assert_eq!(fake.volume(0).count(Kind::VolumeHead), 1);
}

#[tokio::test]
async fn get_without_a_range_streams_the_whole_object() {
    let fake = one_object().await;
    let answer = client()
        .get(&fake.filer(0).url(PATH), None, false)
        .await
        .expect("get");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.len, Some(10));
    assert_eq!(answer.etag.as_deref().map(str::len), Some(32));
    let shown = format!("{answer:?}");
    assert!(
        shown.starts_with("Response { status: 200, etag: Some(\""),
        "{shown}"
    );
    assert!(shown.ends_with(", len: Some(10), .. }"), "{shown}");
    assert_eq!(collect(answer.body).await.expect("body"), digits());
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 1);
    assert_eq!(fake.filer(0).bytes_served(), 10);
}

#[tokio::test]
async fn get_with_a_range_gets_a_206_of_exactly_those_bytes() {
    let fake = one_object().await;
    let answer = client()
        .get(&fake.filer(0).url(PATH), Some((3, 7)), false)
        .await
        .expect("get");
    assert_eq!((answer.status, answer.len), (206, Some(4)));
    assert_eq!(collect(answer.body).await.expect("body"), b"3456");
    assert_eq!(fake.filer(0).bytes_served(), 4);
}

#[tokio::test]
async fn a_range_past_the_end_is_clamped_to_the_object() {
    let fake = one_object().await;
    let answer = client()
        .get(&fake.filer(0).url(PATH), Some((5, 1000)), false)
        .await
        .expect("get");
    assert_eq!((answer.status, answer.len), (206, Some(5)));
    assert_eq!(collect(answer.body).await.expect("body"), b"56789");
}

#[tokio::test]
async fn a_range_that_starts_past_the_end_is_an_error() {
    let fake = Fake::builder()
        .object(BUCKET, KEY, digits())
        .object(BUCKET, "empty", Vec::new())
        .start()
        .await;
    let past = client()
        .get(&fake.filer(0).url(PATH), Some((10, 12)), false)
        .await
        .expect_err("416");
    assert_eq!(http_why(&past), "status 416");
    let empty = client()
        .get(
            &fake.filer(0).url("/buckets/packs/empty"),
            Some((0, 1)),
            false,
        )
        .await
        .expect_err("416 of an empty object");
    assert_eq!(http_why(&empty), "status 416");
    assert!(fake.chunks(BUCKET, "empty").is_empty());
}

#[tokio::test]
async fn an_empty_range_is_refused_without_a_request() {
    let fake = one_object().await;
    let url = fake.filer(0).url(PATH);
    let e = client()
        .get(&url, Some((4, 4)), false)
        .await
        .expect_err("empty");
    assert_eq!(e.to_string(), format!("{url}: empty range 4..4"));
    assert_eq!(fake.filer(0).path_count(PATH), 0);
}

#[tokio::test]
async fn a_server_that_ignores_ranges_is_accepted_only_when_it_sent_what_was_asked() {
    let fake = one_object().await;
    let url = fake.filer(0).url(PATH);
    fake.filer(0).ignore_range(PATH);
    // The range names the whole object: the 200 is the answer.
    let whole = client()
        .get(&url, Some((0, 10)), false)
        .await
        .expect("whole");
    assert_eq!(whole.status, 200);
    assert_eq!(collect(whole.body).await.expect("body"), digits());
    let wider = client()
        .get(&url, Some((0, 99)), false)
        .await
        .expect("wider");
    assert_eq!(wider.status, 200);
    // A part of the object is not.
    let from_zero = client()
        .get(&url, Some((0, 5)), false)
        .await
        .expect_err("200");
    assert_eq!(http_why(&from_zero), "status 200");
    let from_two = client()
        .get(&url, Some((2, 99)), false)
        .await
        .expect_err("200");
    assert_eq!(http_why(&from_two), "status 200");
}

#[tokio::test]
async fn a_missing_object_is_ok_with_status_404_also_for_a_ranged_read() {
    let fake = one_object().await;
    let url = fake.filer(0).url("/buckets/packs/nothing");
    let plain = client().get(&url, None, false).await.expect("get");
    assert_eq!(plain.status, 404);
    let ranged = client().get(&url, Some((0, 4)), false).await.expect("get");
    assert_eq!(ranged.status, 404);
}

#[tokio::test]
async fn other_statuses_are_errors_until_the_failure_is_used_up() {
    let fake = one_object().await;
    let url = fake.filer(0).url(PATH);
    fake.filer(0).fail_status(PATH, 503, Some(2));
    for _ in 0..2 {
        let e = client().get(&url, None, false).await.expect_err("503");
        assert_eq!(e.to_string(), format!("{url}: status 503"));
    }
    let ok = client().get(&url, None, false).await.expect("recovered");
    assert_eq!(ok.status, 200);
    assert_eq!(fake.filer(0).path_count(PATH), 3);
}

#[tokio::test]
async fn clearing_the_rules_of_a_path_restores_it() {
    let fake = one_object().await;
    let url = fake.filer(0).url(PATH);
    fake.filer(0).fail_status(PATH, 500, None);
    fake.filer(0).cut_after(PATH, 1);
    fake.filer(0).ignore_range(PATH);
    let _gate = fake.filer(0).gate(PATH);
    fake.filer(0).clear_rules(PATH);
    let answer = client().get(&url, Some((2, 4)), false).await.expect("get");
    assert_eq!(answer.status, 206);
    assert_eq!(collect(answer.body).await.expect("body"), b"23");
}

#[tokio::test]
async fn get_bytes_returns_the_whole_body() {
    let fake = one_object().await;
    let bytes = client()
        .get_bytes(&fake.filer(0).url(PATH), false)
        .await
        .expect("bytes");
    assert_eq!(bytes.as_ref(), digits().as_slice());
}

#[tokio::test]
async fn get_bytes_refuses_a_missing_object_and_a_204() {
    let fake = one_object().await;
    let missing = fake.filer(0).url("/buckets/packs/nothing");
    let e = client().get_bytes(&missing, false).await.expect_err("404");
    assert_eq!(e.to_string(), format!("{missing}: status 404"));
    fake.filer(0).fail_status(PATH, 204, None);
    let url = fake.filer(0).url(PATH);
    let e = client().get_bytes(&url, false).await.expect_err("204");
    assert_eq!(e.to_string(), format!("{url}: status 204"));
}

#[tokio::test]
async fn get_bytes_fails_when_the_body_breaks_off() {
    let fake = one_object().await;
    fake.filer(0).cut_after(PATH, 4);
    let url = fake.filer(0).url(PATH);
    let e = client().get_bytes(&url, false).await.expect_err("cut");
    assert!(http_why(&e).starts_with("body: "), "{e}");
}

#[tokio::test]
async fn a_cut_body_delivers_what_was_sent_and_then_an_error() {
    let fake = one_object().await;
    fake.filer(0).cut_after(PATH, 4);
    let answer = client()
        .get(&fake.filer(0).url(PATH), None, false)
        .await
        .expect("get");
    assert_eq!(answer.len, Some(10));
    let pieces: Vec<_> = answer.body.collect().await;
    let (failure, sent) = pieces.split_last().expect("the body has an end");
    let got: Vec<u8> = sent
        .iter()
        .flat_map(|piece| piece.as_ref().expect("a piece before the cut").to_vec())
        .collect();
    assert_eq!(got, b"0123");
    let failure = failure.as_ref().expect_err("the cut");
    assert!(http_why(failure).starts_with("body: "));
    assert_eq!(fake.filer(0).bytes_served(), 4);
}

#[tokio::test]
async fn metadata_names_the_size_and_the_chunks() {
    let fake = Fake::builder()
        .chunk_size(4)
        .object(BUCKET, KEY, digits())
        .start()
        .await;
    let url = fake.filer(0).url(&format!("{PATH}?metadata=true"));
    let json = client().get_bytes(&url, true).await.expect("metadata");
    let doc: serde_json::Value = serde_json::from_slice(&json).expect("json");
    assert_eq!(doc["FileSize"], 10);
    assert_eq!(doc["FullPath"], PATH);
    assert!(doc["Md5"].is_null());
    let chunks = doc["chunks"].as_array().expect("chunks");
    let named: Vec<(String, u64, u64)> = chunks
        .iter()
        .map(|c| {
            (
                c["file_id"].as_str().expect("fid").to_owned(),
                c["offset"].as_u64().expect("offset"),
                c["size"].as_u64().expect("size"),
            )
        })
        .collect();
    let held: Vec<(String, u64, u64)> = fake
        .chunks(BUCKET, KEY)
        .into_iter()
        .map(|c| (c.fid, c.offset, c.size))
        .collect();
    assert_eq!(named, held);
    assert_eq!(
        held.iter().map(|c| (c.1, c.2)).collect::<Vec<_>>(),
        [(0, 4), (4, 4), (8, 2)]
    );
    assert_eq!(fake.filer(0).count(Kind::FilerMetadata), 1);
    assert_eq!(fake.filer(0).path_count(PATH), 1);
}

#[tokio::test]
async fn metadata_needs_both_the_query_and_the_json_accept() {
    let fake = one_object().await;
    let with_query = fake.filer(0).url(&format!("{PATH}?metadata=true"));
    let raw_bytes = client().get_bytes(&with_query, false).await.expect("data");
    assert_eq!(raw_bytes.as_ref(), digits().as_slice());
    let json_only = client()
        .get_bytes(&fake.filer(0).url(PATH), true)
        .await
        .expect("data");
    assert_eq!(json_only.as_ref(), digits().as_slice());
    let other_query = fake.filer(0).url(&format!("{PATH}?x=1"));
    client().get_bytes(&other_query, true).await.expect("data");
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 3);
    assert_eq!(fake.filer(0).count(Kind::FilerMetadata), 0);
}

#[tokio::test]
async fn chunks_are_spread_over_the_volumes_and_served_by_the_servers_that_hold_them() {
    let fake = Fake::builder()
        .volume_servers(2)
        .volume(7, &[0, 1], 1)
        .volume(9, &[1], 2)
        .chunk_size(4)
        .object(BUCKET, KEY, digits())
        .object_on(BUCKET, "pinned", b"abcdef".to_vec(), 9)
        .start()
        .await;
    let chunks = fake.chunks(BUCKET, KEY);
    assert_eq!(
        chunks.iter().map(|c| c.volume).collect::<Vec<_>>(),
        [7, 9, 7]
    );
    assert_eq!(
        chunks.iter().map(|c| c.fid.as_str()).collect::<Vec<_>>(),
        ["7,00000001", "9,00000002", "7,00000003"]
    );
    let pinned = fake.chunks(BUCKET, "pinned");
    assert_eq!(
        pinned.iter().map(|c| c.fid.as_str()).collect::<Vec<_>>(),
        ["9,00000004", "9,00000005"]
    );
    // Volume 7's chunks are on both servers; volume 9's only on server 1.
    let first = &chunks[0].fid;
    for server in 0..2 {
        let got = client()
            .get_bytes(&fake.volume(server).url(&format!("/{first}")), false)
            .await
            .expect("needle");
        assert_eq!(got.as_ref(), b"0123");
    }
    let second = &chunks[1].fid;
    let on_zero = client()
        .get(&fake.volume(0).url(&format!("/{second}")), None, false)
        .await
        .expect("get");
    assert_eq!(on_zero.status, 404);
    let on_one = client()
        .get_bytes(&fake.volume(1).url(&format!("/{second}")), false)
        .await
        .expect("needle");
    assert_eq!(on_one.as_ref(), b"4567");
    assert_eq!(fake.total(Kind::VolumeGet), 4);
    assert_eq!(fake.volume(0).count(Kind::VolumeGet), 2);
}

#[tokio::test]
async fn a_needle_is_read_by_range() {
    let fake = one_object().await;
    let fid = fake.chunks(BUCKET, KEY)[0].fid.clone();
    let answer = client()
        .get(
            &fake.volume(0).url(&format!("/{fid}")),
            Some((8, 10)),
            false,
        )
        .await
        .expect("get");
    assert_eq!((answer.status, answer.etag), (206, None));
    assert_eq!(collect(answer.body).await.expect("body"), b"89");
}

#[tokio::test]
async fn a_chunk_of_a_volume_nobody_holds_is_not_found() {
    let fake = Fake::builder()
        .object_on(BUCKET, KEY, digits(), 99)
        .start()
        .await;
    let chunk = fake.chunks(BUCKET, KEY).remove(0);
    assert_eq!((chunk.volume, chunk.offset, chunk.size), (99, 0, 10));
    let answer = client()
        .get(&fake.volume(0).url(&format!("/{}", chunk.fid)), None, false)
        .await
        .expect("get");
    assert_eq!(answer.status, 404);
}

#[tokio::test]
async fn the_status_page_lists_the_volumes_a_server_holds_and_its_disks() {
    let fake = Fake::builder()
        .volume_servers(1)
        .volume(13, &[0, 2], 1)
        .volume(14, &[2], 2)
        .disks(3)
        .object_on(BUCKET, KEY, digits(), 13)
        .start()
        .await;
    let status = |server: usize| {
        let url = fake.volume(server).url("/status");
        async move {
            let bytes = client().get_bytes(&url, true).await.expect("status");
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")
        }
    };
    let zero = status(0).await;
    assert_eq!(zero["Version"], "30GB 4.54 fake");
    let dirs: Vec<&str> = zero["DiskStatuses"]
        .as_array()
        .expect("disks")
        .iter()
        .map(|d| d["dir"].as_str().expect("dir"))
        .collect();
    assert_eq!(dirs, ["/mnt/hdd1/swfs", "/mnt/hdd2/swfs", "/mnt/hdd3/swfs"]);
    let volumes = zero["Volumes"].as_array().expect("volumes");
    assert_eq!(volumes.len(), 1);
    assert_eq!(volumes[0]["Id"], 13);
    assert_eq!(volumes[0]["DiskId"], 1);
    assert_eq!(volumes[0]["Collection"], BUCKET);
    assert_eq!(volumes[0]["Size"], 10);
    // The server nobody asked for by number exists because a volume is declared on it.
    let two = status(2).await;
    let ids: Vec<u64> = two["Volumes"]
        .as_array()
        .expect("volumes")
        .iter()
        .map(|v| v["Id"].as_u64().expect("id"))
        .collect();
    assert_eq!(ids, [13, 14]);
    assert_eq!(status(1).await["Volumes"].as_array().map(Vec::len), Some(0));
    assert_eq!(fake.total(Kind::VolumeStatus), 3);
}

#[tokio::test]
async fn with_no_volume_declared_volume_1_is_on_server_0() {
    let fake = one_object().await;
    assert_eq!(fake.chunks(BUCKET, KEY)[0].fid, "1,00000001");
    let bytes = client()
        .get_bytes(&fake.volume(0).url("/status"), true)
        .await
        .expect("status");
    let doc: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(doc["Volumes"][0]["Id"], 1);
    assert_eq!(doc["Volumes"][0]["DiskId"], 1);
    assert_eq!(doc["DiskStatuses"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn a_chunk_size_of_zero_counts_as_one() {
    let fake = Fake::builder()
        .chunk_size(0)
        .object(BUCKET, KEY, b"abc".to_vec())
        .start()
        .await;
    assert_eq!(fake.chunks(BUCKET, KEY).len(), 3);
}

#[tokio::test]
async fn filers_hold_the_same_objects_and_count_apart() {
    let fake = Fake::builder()
        .filers(2)
        .object(BUCKET, KEY, digits())
        .start()
        .await;
    for filer in 0..2 {
        client()
            .get_bytes(&fake.filer(filer).url(PATH), false)
            .await
            .expect("get");
    }
    client()
        .get_bytes(&fake.filer(1).url(PATH), false)
        .await
        .expect("get");
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 1);
    assert_eq!(fake.filer(1).count(Kind::FilerGet), 2);
    assert_eq!(fake.total(Kind::FilerGet), 3);
    assert_ne!(fake.filer(0).addr(), fake.filer(1).addr());
    assert!(fake.filer(0).addr().starts_with("127.0.0.1:"));
}

#[tokio::test]
async fn a_filer_counts_a_path_outside_buckets_as_other() {
    let fake = one_object().await;
    let answer = client()
        .get(&fake.filer(0).url("/status"), None, false)
        .await
        .expect("get");
    assert_eq!(answer.status, 404);
    assert_eq!(fake.filer(0).count(Kind::Other), 1);
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 0);
    assert_eq!(fake.volume(0).count(Kind::VolumeStatus), 0);
}

#[tokio::test]
async fn requests_share_a_connection() {
    let fake = one_object().await;
    let http = client();
    for _ in 0..3 {
        http.get_bytes(&fake.filer(0).url(PATH), false)
            .await
            .expect("get");
    }
    assert_eq!(fake.filer(0).connections(), 1);
    assert_eq!(fake.filer(0).path_count(PATH), 3);
}

#[tokio::test]
async fn a_gate_holds_a_request_until_it_is_opened() {
    let fake = one_object().await;
    let gate = fake.filer(0).gate(PATH);
    let url = fake.filer(0).url(PATH);
    let http = client();
    let waiting = tokio::spawn(async move { http.get_bytes(&url, false).await });
    gate.arrived(1).await;
    assert!(!waiting.is_finished());
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 1);
    assert_eq!(fake.filer(0).bytes_served(), 0);
    gate.open();
    let bytes = waiting.await.expect("task").expect("get");
    assert_eq!(bytes.as_ref(), digits().as_slice());
    // An open gate lets later requests through, and still counts them.
    let again = client()
        .get_bytes(&fake.filer(0).url(PATH), false)
        .await
        .expect("get");
    assert_eq!(again.len(), 10);
    gate.arrived(2).await;
}

#[tokio::test]
async fn a_request_that_gets_no_answer_in_time_is_an_error() {
    let fake = one_object().await;
    let _closed = fake.filer(0).gate(PATH);
    let url = fake.filer(0).url(PATH);
    let e = HttpClient::new(1)
        .get(&url, None, false)
        .await
        .expect_err("timeout");
    assert_eq!(e.to_string(), format!("{url}: no answer within 1 ms"));
}

#[tokio::test]
async fn a_node_that_is_gone_is_a_transport_error() {
    let fake = one_object().await;
    let url = fake.filer(0).url(PATH);
    drop(fake);
    let e = client().head(&url).await.expect_err("refused");
    assert!(http_why(&e).starts_with("transport: "), "{e}");
}

#[tokio::test]
async fn a_url_that_is_not_one_is_a_bad_request() {
    let e = client()
        .get("not a url", None, false)
        .await
        .expect_err("bad");
    assert!(http_why(&e).starts_with("bad request: "), "{e}");
}

#[tokio::test]
async fn a_ranged_answer_names_its_content_range() {
    let fake = one_object().await;
    let answer = raw(
        fake.filer(0).addr(),
        &format!("GET {PATH} HTTP/1.1\r\nRange: bytes=2-4"),
    )
    .await
    .to_ascii_lowercase();
    assert!(answer.starts_with("http/1.1 206"), "{answer}");
    assert!(answer.contains("content-range: bytes 2-4/10"), "{answer}");
    assert!(answer.contains("accept-ranges: bytes"), "{answer}");
    assert!(answer.ends_with("234"), "{answer}");
    let open_ended = raw(
        fake.filer(0).addr(),
        &format!("GET {PATH} HTTP/1.1\r\nRange: bytes=8-"),
    )
    .await
    .to_ascii_lowercase();
    assert!(
        open_ended.contains("content-range: bytes 8-9/10"),
        "{open_ended}"
    );
    let unsatisfiable = raw(
        fake.filer(0).addr(),
        &format!("GET {PATH} HTTP/1.1\r\nRange: bytes=10-"),
    )
    .await
    .to_ascii_lowercase();
    assert!(unsatisfiable.starts_with("http/1.1 416"), "{unsatisfiable}");
    assert!(
        unsatisfiable.contains("content-range: bytes */10"),
        "{unsatisfiable}"
    );
}

#[tokio::test]
async fn a_range_the_fake_does_not_understand_gets_the_whole_object() {
    let fake = one_object().await;
    for range in ["items=0-1", "bytes=5", "bytes=x-4", "bytes=1-x", "bytes=-5"] {
        let answer = raw(
            fake.filer(0).addr(),
            &format!("GET {PATH} HTTP/1.1\r\nRange: {range}"),
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{range}: {answer}");
        assert!(answer.ends_with("0123456789"), "{range}: {answer}");
    }
    let backwards = raw(
        fake.filer(0).addr(),
        &format!("GET {PATH} HTTP/1.1\r\nRange: bytes=5-2"),
    )
    .await;
    assert!(backwards.starts_with("HTTP/1.1 416"), "{backwards}");
}

#[tokio::test]
async fn a_fake_without_filers_still_knows_its_chunks() {
    let fake = Fake::builder()
        .object(BUCKET, KEY, digits())
        .filers(0)
        .start()
        .await;
    assert_eq!(fake.chunks(BUCKET, "missing"), Vec::new());
    assert_eq!(fake.chunks(BUCKET, KEY).len(), 1);
}

async fn listing(fake: &Fake, query: &str) -> Value {
    let url = fake.filer(0).url(&format!("/buckets/packs/cas/l/{query}"));
    let body = client().get_bytes(&url, true).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn the_fake_lists_a_directory_in_pages_after_a_name() {
    let manifest = br#"{"v":1,"children":[]}"#.to_vec();
    let mut builder = Fake::builder();
    for (name, mtime) in [
        ("t01", "2026-09-01T00:00:00Z"),
        ("t02", "2026-11-01T00:00:00Z"),
        ("t03", "2026-11-02T00:00:00Z"),
        ("t04", "2026-11-03T00:00:00Z"),
        ("t05", "2026-11-04T00:00:00Z"),
        ("t07", "2026-11-06T00:00:00Z"),
        ("sub/t99", "2026-11-07T00:00:00Z"),
    ] {
        builder = builder.object_at(BUCKET, &format!("cas/l/{name}"), manifest.clone(), mtime);
    }
    let fake = builder.start().await;
    let manifest_len = manifest.len();
    let page = listing(&fake, "?limit=3&lastFileName=t02").await;
    let names: Vec<&str> = page["Entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["FullPath"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "/buckets/packs/cas/l/t03",
            "/buckets/packs/cas/l/t04",
            "/buckets/packs/cas/l/t05"
        ]
    );
    assert_eq!(page["Entries"][0]["FileSize"], manifest_len);
    assert_eq!(page["Entries"][0]["Mtime"], "2026-11-02T00:00:00Z");
    assert_eq!(page["LastFileName"], "t05");
    assert_eq!(page["ShouldDisplayLoadMore"], true);
    assert_eq!(page["Path"], "/buckets/packs/cas/l");
    // The rest: exactly what is left is not "more".
    let page = listing(&fake, "?limit=2&lastFileName=t05").await;
    assert_eq!(page["LastFileName"], "t07");
    assert_eq!(page["ShouldDisplayLoadMore"], false);
    // Without parameters: everything, up to 100 entries; nothing after the last name.
    assert_eq!(
        listing(&fake, "").await["Entries"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    let page = listing(&fake, "?lastFileName=t07").await;
    assert_eq!(page["Entries"], json!([]));
    assert_eq!(page["LastFileName"], "");
    assert_eq!(fake.filer(0).count(Kind::FilerList), 4);
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 0);
}

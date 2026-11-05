//! HeadBucket, CreateBucket and DeleteBucket of the one bucket the server serves, through
//! `Server::handle`. The bucket is made by configuration, so a create finds it there and a
//! delete does not remove it.

use hyper::StatusCode;

use crate::Op;

use super::AccessKey;
use super::bucket_tests::{rig_with_every_role, statuses};
use super::testing::{BUCKET, Reply, Rig, texts};

const MEBIBYTE: u64 = 1 << 20;

/// The id and the secret of a read-only key.
const READER: (&str, &str) = ("AKIDREADER", "reader-secret-0123456789");

async fn rig_in(region: &'static str) -> Rig {
    Rig::with(MEBIBYTE, |config| config.region = region.to_owned()).await
}

/// A `PUT` of the bucket with `body`.
async fn create(rig: &Rig, body: &str) -> Reply {
    rig.send("PUT", &format!("/{BUCKET}"), &[], body.as_bytes())
        .await
}

fn constraint(region: &str) -> String {
    format!(
        "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>"
    )
}

/// Asserts that HeadBucket carries `x-amz-bucket-region`, the region of the server, which the
/// SDKs read to find where a bucket is, and has no body.
#[tokio::test(start_paused = true)]
async fn head_bucket_names_the_region_of_the_bucket() {
    for region in ["us-east-1", "eu-west-1"] {
        let rig = rig_in(region).await;
        let reply = rig.send("HEAD", &format!("/{BUCKET}"), &[], b"").await;
        assert_eq!(reply.status, StatusCode::OK, "{region}");
        assert_eq!(reply.header("x-amz-bucket-region"), region);
        assert_eq!(reply.body, "");
    }
}

/// Asserts that CreateBucket of the bucket the server already serves is answered as S3 answers a
/// create of a bucket one owns: 200 with `Location` in `us-east-1` (where S3 keeps that
/// answer for old clients), 409 `BucketAlreadyOwnedByYou` in every other region, with or
/// without a `LocationConstraint` that names the region.
#[tokio::test(start_paused = true)]
async fn creating_the_bucket_again_is_as_s3_answers_it() {
    let home = rig_in("us-east-1").await;
    for body in ["", constraint("").as_str(), "<CreateBucketConfiguration/>"] {
        let reply = create(&home, body).await;
        assert_eq!(reply.status, StatusCode::OK, "{body}");
        assert_eq!(reply.header("location"), format!("/{BUCKET}"));
    }
    let away = rig_in("eu-west-1").await;
    for body in ["", constraint("eu-west-1").as_str()] {
        let reply = create(&away, body).await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{body}");
        assert_eq!(reply.code(), "BucketAlreadyOwnedByYou", "{body}");
    }
}

/// Asserts that a `LocationConstraint` for another region than the server's is refused 400
/// `IllegalLocationConstraintException` (as S3 refuses a constraint that is not the region of the
/// endpoint), whichever region the server is in, and that a body which is not a
/// `CreateBucketConfiguration` is 400 `MalformedXML`.
#[tokio::test(start_paused = true)]
async fn a_create_with_another_region_or_a_bad_body_is_refused() {
    for region in ["us-east-1", "eu-west-1"] {
        let rig = rig_in(region).await;
        let reply = create(&rig, &constraint("ap-south-1")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{region}");
        assert_eq!(reply.code(), "IllegalLocationConstraintException");
        let said = texts(&reply.body, "Message").concat();
        assert_eq!(
            said,
            "The ap-south-1 location constraint is incompatible for the region specific \
             endpoint this request was sent to."
        );
        for bad in ["not xml", "<Other/>", "<CreateBucketConfiguration><Oops"] {
            let reply = create(&rig, bad).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(reply.code(), "MalformedXML", "{bad}");
        }
    }
}

/// Asserts that a bucket of another name is `NoSuchBucket` for create, head and delete: the server
/// serves one bucket and cannot make another.
#[tokio::test(start_paused = true)]
async fn another_bucket_cannot_be_made_or_deleted() {
    let rig = Rig::new(MEBIBYTE).await;
    for method in ["PUT", "HEAD", "DELETE"] {
        let reply = rig.send(method, "/other-bucket", &[], b"").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method}");
        if method != "HEAD" {
            assert_eq!(reply.code(), "NoSuchBucket", "{method}");
        }
    }
}

/// Asserts that DeleteBucket of a bucket that has an object is 409 `BucketNotEmpty`, as in S3,
/// and the object is still there.
#[tokio::test(start_paused = true)]
async fn deleting_a_bucket_with_objects_is_bucket_not_empty() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.put("cas/a", b"kept").await;
    let reply = rig.send("DELETE", &format!("/{BUCKET}"), &[], b"").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.code(), "BucketNotEmpty");
    assert_eq!(
        texts(&reply.body, "Message"),
        ["The bucket you tried to delete is not empty."]
    );
    assert_eq!(rig.get("cas/a", None).await.body, "kept");
}

/// Asserts that DeleteBucket of an empty bucket is refused 403 `AccessDenied` with the reason,
/// since the bucket is set up by the configuration of the server and no call removes it: it
/// stays, and HeadBucket is still 200. A read-only key is refused as for every write, and a
/// listing that fails is the failure of the store, not an answer about the bucket.
#[tokio::test(start_paused = true)]
async fn deleting_an_empty_bucket_is_refused_and_it_stays() {
    let keys = vec![AccessKey::new(READER.0, READER.1)];
    let rig = Rig::with(MEBIBYTE, |config| config.read_only_keys = keys).await;
    let uri = format!("/{BUCKET}");
    let reply = rig.send("DELETE", &uri, &[], b"").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "AccessDenied");
    assert_eq!(
        texts(&reply.body, "Message"),
        ["The bucket is set up by the configuration of this server and is not deleted by a call."]
    );
    assert_eq!(
        rig.send("HEAD", &uri, &[], b"").await.status,
        StatusCode::OK
    );
    let reader = rig.send_as(READER, "DELETE", &uri, &[], b"").await;
    assert_eq!(reader.code(), "AccessDenied");
    rig.hot.fail_next(Op::List, 1);
    let failed = rig.send("DELETE", &uri, &[], b"").await;
    assert!(failed.status.is_server_error(), "{}", failed.status);
}

/// Asserts which role may make HeadBucket, CreateBucket and DeleteBucket: HeadBucket is 200 for the
/// full and the read-only key; CreateBucket (200, the bucket exists in `us-east-1`) is for the full
/// key only; DeleteBucket of a bucket with an object (409 `BucketNotEmpty` for the full key) is
/// refused 403 to the read-only key; and a scoped key and a GC key are 403 on all three.
#[tokio::test(start_paused = true)]
async fn only_the_full_key_creates_or_deletes_and_the_read_only_key_heads_the_bucket() {
    let rig = rig_with_every_role().await;
    let uri = format!("/{BUCKET}");
    assert_eq!(statuses(&rig, "HEAD", &uri).await, [200, 200, 403, 403]);
    assert_eq!(statuses(&rig, "PUT", &uri).await, [200, 403, 403, 403]);
    rig.call("PUT", "k", b"x").await;
    assert_eq!(statuses(&rig, "DELETE", &uri).await, [409, 403, 403, 403]);
}

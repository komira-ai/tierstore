//! The check of a call's head, and the three calls over a core.

use std::sync::Arc;

use hyper::Request;
use hyper::http::request::Parts;
use tierstore_trees_api::v1::trees_service_server::TreesService;
use tierstore_trees_api::v1::{
    CreateTreeRequest, DeleteTreeRequest, Entry, File, FsLocation, GetPlacementRequest, Location,
    PlacementState, S3Location, Source, entry, location,
};
use tonic::{Code, Request as GrpcRequest, Status};

use super::rig::{
    FULL, FakeCore, HOST, NOW, READER, SCOPED, assert_refused, config, config_at, framed,
    signed_request,
};
use super::service::{TreesFront, authenticate, signed_body, with_host};
use super::{Caller, Reason, TreeCore};
use crate::server::keys::Role;
use crate::server::presign::presign_get;
use crate::server::presign::tests::{parts_of, request};
use crate::server::{AccessKey, PresignKey};

fn parts(key: (&str, &str), method: &str) -> Parts {
    let message = framed(&GetPlacementRequest::default());
    let signed = signed_request(&config(), key, HOST, method, message);
    with_host(signed.into_parts().0)
}

/// Asserts a call signed with any key of the server is the call of that key, with the role the
/// server gives it, with the host the authority of the request.
#[test]
fn a_signed_call_names_its_key_and_its_role() {
    let full = authenticate(&config(), &parts(FULL, "GetPlacement")).expect("signed");
    let expected = Caller {
        key_id: FULL.0.to_owned(),
        role: Role::Full,
    };
    assert_eq!(full, expected);
    let reader = authenticate(&config(), &parts(READER, "GetPlacement")).expect("signed");
    assert_eq!(reader.role, Role::ReadOnly);
    let scoped = authenticate(&config(), &parts(SCOPED, "CreateTree")).expect("signed");
    assert_eq!(scoped.key_id, SCOPED.0);
    assert!(matches!(scoped.role, Role::Scoped(_)));
}

/// Asserts the `host` a signature covers is the `host` header when the client sends one, and the
/// authority of the request when it does not, and that a request with neither has no host.
#[test]
fn the_host_of_a_call_is_its_header_or_its_authority() {
    let with_header = Request::builder()
        .uri("http://other.test/p")
        .header("host", "header.test")
        .body(())
        .expect("a request");
    let kept = with_host(with_header.into_parts().0);
    assert_eq!(kept.headers["host"], "header.test");

    let authority = Request::builder()
        .uri("http://auth.test:9600/p")
        .body(())
        .expect("a request");
    let made = with_host(authority.into_parts().0);
    assert_eq!(made.headers["host"], "auth.test:9600");

    let neither = Request::builder().uri("/p").body(()).expect("a request");
    assert!(
        !with_host(neither.into_parts().0)
            .headers
            .contains_key("host")
    );
}

fn refused(parts: &Parts, config: &crate::server::ServerConfig) -> Status {
    Status::from(authenticate(config, parts).expect_err("refused"))
}

/// Asserts each way a head can be wrong is `UNAUTHENTICATED` with the reason the S3 front would
/// give: an unknown key, a wrong secret, a signature over another host, a time too far from the
/// server's, and a head with no signature at all.
#[test]
fn a_head_that_is_not_signed_by_a_key_of_the_server_is_unauthenticated() {
    let server = config();
    let unknown = refused(&parts(("AKIDOTHER", "x"), "GetPlacement"), &server);
    assert_refused(
        &unknown,
        Code::Unauthenticated,
        "unauthenticated: The access key id or the region is not one this server accepts.",
    );
    let mismatch = "unauthenticated: The request signature does not match the one calculated from the request.";
    let wrong = refused(&parts((FULL.0, "not-the-secret"), "GetPlacement"), &server);
    assert_refused(&wrong, Code::Unauthenticated, mismatch);

    let mut elsewhere = parts(FULL, "GetPlacement");
    elsewhere
        .headers
        .insert("host", "another.test".parse().expect("a value"));
    assert_refused(
        &refused(&elsewhere, &server),
        Code::Unauthenticated,
        mismatch,
    );

    let late = config_at(NOW + 16 * 60 * 1000);
    assert_refused(
        &refused(&parts(FULL, "GetPlacement"), &late),
        Code::Unauthenticated,
        "unauthenticated: The time of the request is too far from the time of the server.",
    );

    let mut bare = parts(FULL, "GetPlacement");
    bare.headers.remove("authorization");
    assert_refused(
        &refused(&bare, &server),
        Code::Unauthenticated,
        "unauthenticated: Access Denied: requests must be signed with AWS Signature Version 4.",
    );
}

/// Asserts a key that only signs presigned URLs may not make a call, and neither may a presigned
/// URL, which carries its signature in the query and names its key in no header.
#[test]
fn a_presigning_key_and_a_presigned_url_are_not_calls() {
    let mut server = config();
    let grant = crate::server::presign::tests::grant();
    server.presign_keys =
        vec![PresignKey::new("AKIDPRESIGN", "presign-secret-0123456789", grant).expect("a key")];
    let by_presigner = parts(("AKIDPRESIGN", "presign-secret-0123456789"), "GetPlacement");
    assert_refused(
        &refused(&by_presigner, &server),
        Code::Unauthenticated,
        "unauthenticated: Access Denied: this access key signs presigned PUT URLs only.",
    );

    let key = AccessKey::new(FULL.0, FULL.1);
    let url = presign_get(&key, &request("cas/b/x", 60)).expect("a URL");
    let mut at = config();
    at.presign_max_expires = std::time::Duration::from_secs(600);
    let by_url = parts_of("GET", &url);
    assert_refused(
        &refused(&by_url, &at),
        Code::Unauthenticated,
        "unauthenticated: a call is signed in its Authorization header",
    );
}

fn with_caller<T>(message: T, caller: Option<&Caller>) -> GrpcRequest<T> {
    let mut request = GrpcRequest::new(message);
    if let Some(caller) = caller {
        request.extensions_mut().insert(caller.clone());
    }
    request
}

fn caller(key: (&str, &str)) -> Caller {
    let (_, role) = config().key(key.0).expect("a key of the server");
    Caller {
        key_id: key.0.to_owned(),
        role,
    }
}

fn front(core: &Arc<FakeCore>) -> TreesFront {
    TreesFront::new(Some(Arc::clone(core) as Arc<dyn TreeCore>))
}

/// Asserts a call whose head was not checked has no caller and is `UNAUTHENTICATED`, which no
/// path of the listener makes, and that a node with no core answers `UNAVAILABLE` `NotServing` to
/// each of the three calls of a key that is good.
#[tokio::test]
async fn without_a_core_every_call_is_not_serving() {
    let front = TreesFront::new(None);
    let full = caller(FULL);
    let unchecked = front
        .create_tree(with_caller(CreateTreeRequest::default(), None))
        .await;
    assert_refused(
        &unchecked.expect_err("refused"),
        Code::Unauthenticated,
        "unauthenticated",
    );

    let created = front
        .create_tree(with_caller(CreateTreeRequest::default(), Some(&full)))
        .await;
    assert_refused(
        &created.expect_err("refused"),
        Code::Unavailable,
        "NotServing",
    );
    let shown = front
        .get_placement(with_caller(GetPlacementRequest::default(), Some(&full)))
        .await;
    assert_refused(
        &shown.expect_err("refused"),
        Code::Unavailable,
        "NotServing",
    );
    let deleted = front
        .delete_tree(with_caller(DeleteTreeRequest::default(), Some(&full)))
        .await;
    assert_refused(
        &deleted.expect_err("refused"),
        Code::Unavailable,
        "NotServing",
    );
}

/// Asserts the three calls reach the core with their request and the key that made them, and are
/// answered with what it answers.
#[tokio::test]
async fn a_call_reaches_the_core_with_its_key_and_gets_its_answer() {
    let core = Arc::new(FakeCore::default());
    let front = front(&core);
    let full = caller(FULL);
    let entries = vec![Entry {
        path: "bin/cc".to_owned(),
        kind: None,
    }];
    let request = CreateTreeRequest {
        entries,
        ..CreateTreeRequest::default()
    };
    let made = front
        .create_tree(with_caller(request, Some(&full)))
        .await
        .expect("created");
    assert_eq!(made.get_ref().placement, "p1");
    assert_eq!(made.get_ref().entries, 1);
    let ask = GetPlacementRequest {
        placement: "p1".to_owned(),
        list: None,
    };
    let shown = front
        .get_placement(with_caller(ask, Some(&caller(READER))))
        .await
        .expect("shown");
    assert_eq!(shown.get_ref().placement, "p1");
    assert_eq!(shown.get_ref().state, PlacementState::Ready as i32);
    let gone = DeleteTreeRequest {
        placement: "p1".to_owned(),
    };
    front
        .delete_tree(with_caller(gone, Some(&caller(SCOPED))))
        .await
        .expect("deleted");

    let seen = core.seen.lock().expect("the lock");
    let names: Vec<&str> = seen.iter().map(|(method, _)| *method).collect();
    assert_eq!(names, ["CreateTree", "GetPlacement", "DeleteTree"]);
    let ids: Vec<&str> = seen.iter().map(|(_, who)| who.key_id.as_str()).collect();
    assert_eq!(ids, [FULL.0, READER.0, SCOPED.0]);
}

/// Asserts a key that lacks the right is `PERMISSION_DENIED` and the core is not asked: a read-only
/// key's create and delete, a scoped key's create from a source it may not read.
#[tokio::test]
async fn a_key_without_the_right_is_refused_before_the_core() {
    let core = Arc::new(FakeCore::default());
    let front = front(&core);
    let reader = caller(READER);
    let created = front
        .create_tree(with_caller(CreateTreeRequest::default(), Some(&reader)))
        .await;
    let message =
        "access denied: Access Denied: this access key may read objects and list the bucket only.";
    assert_refused(
        &created.expect_err("refused"),
        Code::PermissionDenied,
        message,
    );
    let deleted = front
        .delete_tree(with_caller(DeleteTreeRequest::default(), Some(&reader)))
        .await;
    assert_refused(
        &deleted.expect_err("refused"),
        Code::PermissionDenied,
        message,
    );

    let from = Source {
        key: "other/x".to_owned(),
        offset: 0,
    };
    let file = File {
        mode: None,
        sha256: vec![1],
        size: 1,
        from: Some(from),
    };
    let request = CreateTreeRequest {
        entries: vec![Entry {
            path: "a".to_owned(),
            kind: Some(entry::Kind::File(file)),
        }],
        at: vec![
            Location {
                r#where: Some(location::Where::S3(S3Location {
                    bucket: "tier-bucket".to_owned(),
                    prefix: "jobs/1/".to_owned(),
                })),
            },
            Location {
                r#where: Some(location::Where::Fs(FsLocation {
                    path: "x".to_owned(),
                })),
            },
        ],
        ..CreateTreeRequest::default()
    };
    let refused = front
        .create_tree(with_caller(request, Some(&caller(SCOPED))))
        .await;
    assert_refused(
        &refused.expect_err("refused"),
        Code::PermissionDenied,
        "access denied: this key may not read other/x",
    );
    assert!(core.seen.lock().expect("the lock").is_empty());
}

/// Asserts a refusal of the core is the status of its reason, with what it names, for each of the
/// three calls.
#[tokio::test]
async fn a_refusal_of_the_core_is_answered_with_its_status() {
    let full = caller(FULL);
    let cases = [
        (
            Reason::LocationTaken,
            Code::AlreadyExists,
            "location taken: bin/cc",
        ),
        (
            Reason::UnknownPlacement,
            Code::NotFound,
            "unknown placement: bin/cc",
        ),
        (
            Reason::NotAvailable,
            Code::Unimplemented,
            "TreesNotAvailable: bin/cc",
        ),
    ];
    for (reason, code, message) in cases {
        let core = Arc::new(FakeCore {
            refuse: Some(reason),
            ..FakeCore::default()
        });
        let front = front(&core);
        let created = front
            .create_tree(with_caller(CreateTreeRequest::default(), Some(&full)))
            .await;
        assert_refused(&created.expect_err("refused"), code, message);
        let shown = front
            .get_placement(with_caller(GetPlacementRequest::default(), Some(&full)))
            .await;
        assert_refused(&shown.expect_err("refused"), code, message);
        let deleted = front
            .delete_tree(with_caller(DeleteTreeRequest::default(), Some(&full)))
            .await;
        assert_refused(&deleted.expect_err("refused"), code, message);
    }
}

/// The head of a call whose `x-amz-content-sha256` is `payload`, and the body it is given.
fn head_claiming(payload: &str) -> Parts {
    let mut parts = parts(FULL, "GetPlacement");
    parts
        .headers
        .insert("x-amz-content-sha256", payload.parse().expect("a header"));
    parts
}

/// A body that gives `first` and then fails, as a connection that is reset does.
struct Broken(Option<bytes::Bytes>);

impl hyper::body::Body for Broken {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let frame = self.0.take().map(hyper::body::Frame::data);
        std::task::Poll::Ready(match frame {
            Some(frame) => Some(Ok(frame)),
            None => Some(Err(std::io::Error::other("reset"))),
        })
    }
}

/// Asserts the body of a call is read only when it is the one whose SHA-256 the signature covers:
/// the signed body is given back whole; another body, a payload of `UNSIGNED-PAYLOAD` and a
/// payload that is not a digest are `UNAUTHENTICATED` with exact words; a body over the limit is
/// `OUT_OF_RANGE`; a body that fails to its end is `INVALID_ARGUMENT`.
#[tokio::test]
async fn the_body_of_a_call_is_the_one_the_signature_covers() {
    use sha2::Digest as _;
    let message = framed(&GetPlacementRequest::default());
    let head = parts(FULL, "GetPlacement");
    let body = || http_body_util::Full::new(bytes::Bytes::from(message.clone()));
    let got = signed_body(&head, body(), 1000)
        .await
        .expect("the signed body");
    assert_eq!(got.as_ref(), message.as_slice());

    let wanted = "unauthenticated: the body is not the one the signature covers: x-amz-content-sha256 is not its SHA-256";
    let other = http_body_util::Full::new(bytes::Bytes::from_static(b"another body"));
    let status = signed_body(&head, other, 1000)
        .await
        .expect_err("another body");
    assert_refused(&status, Code::Unauthenticated, wanted);
    for payload in ["UNSIGNED-PAYLOAD", "not a digest", &"0".repeat(64)] {
        let status = signed_body(&head_claiming(payload), body(), 1000)
            .await
            .expect_err("a payload that is not the hash");
        assert_refused(&status, Code::Unauthenticated, wanted);
    }
    let mut none = head_claiming("x");
    none.headers.remove("x-amz-content-sha256");
    let status = signed_body(&none, body(), 1000)
        .await
        .expect_err("no payload");
    assert_refused(&status, Code::Unauthenticated, wanted);

    let long = http_body_util::Full::new(bytes::Bytes::from(vec![7_u8; 20]));
    let digest = hex::encode(sha2::Sha256::digest([7_u8; 20]));
    let status = signed_body(&head_claiming(&digest), long, 14)
        .await
        .expect_err("a body over the limit");
    assert_refused(
        &status,
        Code::OutOfRange,
        "request too large: the limit is 14 bytes",
    );
    let long = http_body_util::Full::new(bytes::Bytes::from(vec![7_u8; 19]));
    let digest = hex::encode(sha2::Sha256::digest([7_u8; 19]));
    signed_body(&head_claiming(&digest), long, 14)
        .await
        .expect("a body of the limit and the frame head");

    let status = signed_body(&head, Broken(Some(bytes::Bytes::from_static(b"ab"))), 1000)
        .await
        .expect_err("a body that fails");
    assert_refused(
        &status,
        Code::InvalidArgument,
        "the request body could not be read",
    );
}

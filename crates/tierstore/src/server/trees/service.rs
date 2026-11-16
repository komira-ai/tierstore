//! The front: the head of every call is authenticated, and a call that gets past that is the
//! `TreesService` the generated server stub serves, over a [`TreeCore`] if there is one.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, LengthLimitError, Limited};
use hyper::Request;
use hyper::body::{Body as HttpBody, Incoming};
use hyper::http::request::Parts;
use sha2::{Digest as _, Sha256};
use tierstore_trees_api::v1::trees_service_server::{TreesService, TreesServiceServer};
use tierstore_trees_api::v1::{
    CreateTreeRequest, CreateTreeResponse, DeleteTreeRequest, DeleteTreeResponse,
    GetPlacementRequest, GetPlacementResponse,
};
use tonic::body::Body;
use tonic::codegen::Service as _;
use tonic::{Request as GrpcRequest, Response as GrpcResponse, Status};

use super::rights::{permit_create, permit_delete, permit_placement};
use super::{Caller, Reason, Refusal, TreeCore};
use crate::server::ServerConfig;
use crate::server::auth;

/// The service of the trees interface: the three calls over a core, if the node has one.
pub(crate) struct TreesFront {
    core: Option<Arc<dyn TreeCore>>,
}

impl TreesFront {
    /// A service that answers through `core`; with none, `NotServing`.
    pub(crate) fn new(core: Option<Arc<dyn TreeCore>>) -> Self {
        Self { core }
    }

    /// The core, or `NotServing`.
    fn core(&self) -> Result<&Arc<dyn TreeCore>, Refusal> {
        self.core
            .as_ref()
            .ok_or_else(|| Refusal::of(Reason::NotServing))
    }
}

/// The key that made `request`, which the head check put there.
fn caller_of<T>(request: &GrpcRequest<T>) -> Result<Caller, Refusal> {
    let caller = request.extensions().get::<Caller>();
    caller
        .cloned()
        .ok_or_else(|| Refusal::of(Reason::Unauthenticated))
}

#[tonic::async_trait]
impl TreesService for TreesFront {
    async fn create_tree(
        &self,
        request: GrpcRequest<CreateTreeRequest>,
    ) -> Result<GrpcResponse<CreateTreeResponse>, Status> {
        let caller = caller_of(&request)?;
        let core = self.core()?;
        permit_create(&caller, request.get_ref())?;
        let answer = core.create(&caller, request.into_inner()).await?;
        Ok(GrpcResponse::new(answer))
    }

    async fn get_placement(
        &self,
        request: GrpcRequest<GetPlacementRequest>,
    ) -> Result<GrpcResponse<GetPlacementResponse>, Status> {
        let caller = caller_of(&request)?;
        let core = self.core()?;
        permit_placement(&caller)?;
        let answer = core.placement(&caller, request.into_inner()).await?;
        Ok(GrpcResponse::new(answer))
    }

    async fn delete_tree(
        &self,
        request: GrpcRequest<DeleteTreeRequest>,
    ) -> Result<GrpcResponse<DeleteTreeResponse>, Status> {
        let caller = caller_of(&request)?;
        let core = self.core()?;
        permit_delete(&caller)?;
        let answer = core.delete(&caller, request.into_inner()).await?;
        Ok(GrpcResponse::new(answer))
    }
}

/// The head of a call as the signature covers it: SigV4 signs `host`, and a call over HTTP/2 has
/// the `:authority` in its place, which is what a client signs as `host`.
pub(super) fn with_host(mut parts: Parts) -> Parts {
    if !parts.headers.contains_key("host") {
        let authority = parts.uri.authority().map(ToString::to_string);
        if let Some(value) = authority.and_then(|text| text.parse().ok()) {
            parts.headers.insert("host", value);
        }
    }
    parts
}

/// The key that signed the head of a call.
///
/// # Errors
/// [`Reason::Unauthenticated`], with what the S3 front would tell the client: no `Authorization`
/// header, a key the server does not have, a signature that does not match, a time too far from
/// the server's; and a presigned URL, which is not a call of this interface.
pub(super) fn authenticate(config: &ServerConfig, parts: &Parts) -> Result<Caller, Refusal> {
    let signed = auth::verify(config, parts)
        .map_err(|refusal| Refusal::about(Reason::Unauthenticated, refusal.message))?;
    let Some(key_id) = auth::key_id(parts) else {
        let why = "a call is signed in its Authorization header";
        return Err(Refusal::about(Reason::Unauthenticated, why));
    };
    Ok(Caller {
        key_id: key_id.to_owned(),
        role: signed.role,
    })
}

/// The bytes of a call's body, once they are the ones its signature covers: the head carries
/// `x-amz-content-sha256`, which the signature covers (`authenticate` checked it), so the body
/// is read whole (at most `max` message bytes and the 5 of the gRPC frame) and its SHA-256 must be
/// that header's value. A signed head replayed with another body, or a body changed on the way,
/// is refused. `UNSIGNED-PAYLOAD` is not accepted: a call states the hash of what it sends.
///
/// # Errors
/// [`Status`] `OutOfRange` when the body is longer than `max` allows; `InvalidArgument` when it
/// cannot be read to its end; `Unauthenticated` when its hash is not the signed one.
pub(super) async fn signed_body<B>(parts: &Parts, body: B, max: usize) -> Result<Bytes, Status>
where
    B: HttpBody,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let limit = max.saturating_add(FRAME_HEAD);
    let read = Limited::new(body, limit).collect().await;
    let bytes = match read {
        Ok(collected) => collected.to_bytes(),
        Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => {
            let text = format!("request too large: the limit is {max} bytes");
            return Err(Status::out_of_range(text));
        }
        Err(_) => {
            return Err(Status::invalid_argument(
                "the request body could not be read",
            ));
        }
    };
    let claimed = parts
        .headers
        .get("x-amz-content-sha256")
        .and_then(|value| value.to_str().ok());
    if claimed != Some(hex::encode(Sha256::digest(&bytes)).as_str()) {
        let why =
            "the body is not the one the signature covers: x-amz-content-sha256 is not its SHA-256";
        return Err(Refusal::about(Reason::Unauthenticated, why).into());
    }
    Ok(bytes)
}

/// The bytes of a gRPC frame before its message: a compression flag and the length.
const FRAME_HEAD: usize = 5;

/// The trees interface as a connection serves it: the keys that sign its calls, and the service.
#[derive(Clone)]
pub(crate) struct Served {
    config: Arc<ServerConfig>,
    service: TreesServiceServer<TreesFront>,
    max_request_bytes: usize,
}

impl Served {
    /// The interface over `front` for the keys of `config`; a request may be at most
    /// `max_request_bytes` long.
    pub(crate) fn new(
        config: Arc<ServerConfig>,
        front: TreesFront,
        max_request_bytes: usize,
    ) -> Self {
        let service = TreesServiceServer::new(front).max_decoding_message_size(max_request_bytes);
        Self {
            config,
            service,
            max_request_bytes,
        }
    }

    /// Answers one HTTP/2 request: the status of a refused key or body, or the answer of the service.
    pub(crate) async fn handle(
        &self,
        request: Request<Incoming>,
    ) -> Result<hyper::Response<Body>, Infallible> {
        let (parts, body) = request.into_parts();
        let parts = with_host(parts);
        match authenticate(&self.config, &parts) {
            Err(refusal) => Ok(Status::from(refusal).into_http()),
            Ok(caller) => {
                let bound = signed_body(&parts, body, self.max_request_bytes).await;
                match bound {
                    Err(status) => Ok(status.into_http()),
                    Ok(bytes) => {
                        let mut request = Request::from_parts(parts, Body::new(Full::new(bytes)));
                        request.extensions_mut().insert(caller);
                        self.service.clone().call(request).await
                    }
                }
            }
        }
    }
}

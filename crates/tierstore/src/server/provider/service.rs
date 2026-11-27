//! The generated `MountProvider` service over the provider: each call is the provider's, and each
//! answer that is not a success is the status the contract gives it, with a reason of at most
//! [`MAX_REASON_BYTES`].

use tierstore_trees_api::provider::v1::mount_provider_server::MountProvider;
use tierstore_trees_api::provider::v1::{
    MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest, ReleaseResponse,
    SweepRequest, SweepResponse,
};
use tonic::{Code, Request, Response, Status};

use super::{MAX_REASON_BYTES, Provider, ProviderError};

/// The provider as the generated service calls it.
pub(super) struct ProviderFront {
    pub provider: Provider,
}

/// The status of `error`: `INVALID_ARGUMENT`, `FAILED_PRECONDITION` or `UNAVAILABLE`, its reason
/// cut at a character boundary to at most [`MAX_REASON_BYTES`].
pub(super) fn status_of(error: ProviderError) -> Status {
    let (code, mut why) = match error {
        ProviderError::Invalid(why) => (Code::InvalidArgument, why),
        ProviderError::Refused(why) => (Code::FailedPrecondition, why),
        ProviderError::Unavailable(why) => (Code::Unavailable, why),
    };
    let end = (0..=MAX_REASON_BYTES)
        .rev()
        .find(|end| why.is_char_boundary(*end))
        .unwrap_or_default();
    why.truncate(end);
    Status::new(code, why)
}

#[tonic::async_trait]
impl MountProvider for ProviderFront {
    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        let request = request.into_inner();
        self.provider
            .prepare(request.source, &request.r#ref, request.mode)
            .map_err(status_of)?;
        Ok(Response::new(PrepareResponse {}))
    }

    async fn mount(
        &self,
        request: Request<MountRequest>,
    ) -> Result<Response<MountResponse>, Status> {
        let path = self
            .provider
            .mount(request.into_inner().source)
            .await
            .map_err(status_of)?;
        Ok(Response::new(MountResponse {
            path: path.to_string_lossy().into_owned(),
        }))
    }

    async fn release(
        &self,
        request: Request<ReleaseRequest>,
    ) -> Result<Response<ReleaseResponse>, Status> {
        self.provider
            .release(request.into_inner().source)
            .map_err(status_of)?;
        Ok(Response::new(ReleaseResponse {}))
    }

    async fn sweep(
        &self,
        request: Request<SweepRequest>,
    ) -> Result<Response<SweepResponse>, Status> {
        let request = request.into_inner();
        let released = self
            .provider
            .sweep(&request.agent, &request.keep, &request.keep_attempts)
            .map_err(status_of)?;
        Ok(Response::new(SweepResponse { released }))
    }
}

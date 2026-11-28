//! A mount provider on a Unix socket of the test, served as tierstore serves its own (tonic's
//! service on hyper's HTTP/2): Prepare takes any source and remembers its ref, Mount answers the
//! directory `<root>/<run>/<index>` of a prepared source (made empty), Release forgets it; the
//! next answers of a test go before any of that ([`FakeProvider::answer_next`]). Or, for the
//! answers no provider gives, a server that answers every request with one fixed response
//! ([`serve_raw`]), or one that never answers at all ([`serve_silent`]).

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tierstore_trees_api::provider::v1::mount_provider_server::{
    MountProvider, MountProviderServer,
};
use tierstore_trees_api::provider::v1::{
    MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest, ReleaseResponse,
    SweepRequest, SweepResponse,
};
use tokio::net::UnixListener;
use tokio::task::{JoinHandle, JoinSet};
use tonic::codegen::Service as _;
use tonic::{Request, Response, Status};

#[derive(Default)]
struct State {
    prepared: BTreeMap<(String, u32), String>,
    next: VecDeque<Status>,
    calls: Vec<String>,
}

#[derive(Clone)]
struct Front {
    root: PathBuf,
    state: Arc<Mutex<State>>,
}

impl Front {
    /// Logs the call, and takes the next answer a test set, if any.
    fn take(&self, call: String) -> Result<(), Status> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(call);
        state.next.pop_front().map_or(Ok(()), Err)
    }
}

fn of(source: Option<&tierstore_trees_api::provider::v1::Source>) -> (String, u32) {
    let source = source.expect("a source");
    (source.run.clone(), source.index)
}

#[tonic::async_trait]
impl MountProvider for Front {
    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        let request = request.into_inner();
        let key = of(request.source.as_ref());
        self.take(format!("Prepare {} {} {}", key.0, key.1, request.r#ref))?;
        self.state
            .lock()
            .unwrap()
            .prepared
            .insert(key, request.r#ref);
        Ok(Response::new(PrepareResponse {}))
    }

    async fn mount(
        &self,
        request: Request<MountRequest>,
    ) -> Result<Response<MountResponse>, Status> {
        let key = of(request.get_ref().source.as_ref());
        self.take(format!("Mount {} {}", key.0, key.1))?;
        if !self.state.lock().unwrap().prepared.contains_key(&key) {
            return Err(Status::unavailable("the source is not prepared here"));
        }
        let path = self.root.join(&key.0).join(key.1.to_string());
        std::fs::create_dir_all(&path).unwrap();
        Ok(Response::new(MountResponse {
            path: path.display().to_string(),
        }))
    }

    async fn release(
        &self,
        request: Request<ReleaseRequest>,
    ) -> Result<Response<ReleaseResponse>, Status> {
        let key = of(request.get_ref().source.as_ref());
        self.take(format!("Release {} {}", key.0, key.1))?;
        self.state.lock().unwrap().prepared.remove(&key);
        Ok(Response::new(ReleaseResponse {}))
    }

    async fn sweep(&self, _: Request<SweepRequest>) -> Result<Response<SweepResponse>, Status> {
        Ok(Response::new(SweepResponse { released: 0 }))
    }
}

/// The provider (module docs); it serves until it is dropped.
pub struct FakeProvider {
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl FakeProvider {
    /// Serves the provider on a socket bound at `socket`, its directories under `root`.
    pub fn serve(socket: &Path, root: &Path) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let front = Front {
            root: root.to_owned(),
            state: Arc::clone(&state),
        };
        let service = MountProviderServer::new(front);
        let listener = UnixListener::bind(socket).unwrap();
        // The connections are the accept loop's: when the loop is aborted, they end with it.
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let service = service.clone();
                let _ = connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let mut service = service.clone();
                        async move { service.call(request).await }
                    });
                    let _ = http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self { state, task }
    }

    /// Answers the next call with `status`, whatever it asks; after the answers set before.
    pub fn answer_next(&self, status: Status) {
        self.state.lock().unwrap().next.push_back(status);
    }

    /// The calls so far, each `<method> <run> <index>[ <ref>]`.
    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    /// The sources prepared and not released, each `(run, index)`.
    pub fn prepared(&self) -> Vec<(String, u32)> {
        self.state
            .lock()
            .unwrap()
            .prepared
            .keys()
            .cloned()
            .collect()
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Serves `answer()` to every request on a socket bound at `socket`, until the runtime ends.
pub fn serve_raw(socket: &Path, answer: fn() -> hyper::Response<Full<Bytes>>) {
    let listener = UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = service_fn(move |_request| async move {
                    Ok::<_, std::convert::Infallible>(answer())
                });
                let _ = http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

/// Takes every connection on a socket bound at `socket` and never answers on it, until the
/// runtime ends: a provider that hangs.
pub fn serve_silent(socket: &Path) {
    let listener = UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            held.push(stream);
        }
    });
}

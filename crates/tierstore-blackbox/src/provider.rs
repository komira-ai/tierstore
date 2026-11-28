//! A client of a node's mount-provider socket, which calls it as the job agent does: gRPC (HTTP/2)
//! on the Unix socket, one connection kept and made again after it fails, the three calls of a
//! job's input (Prepare, Mount, Release) and the answer of each as the agent reads it.
//!
//! The agent takes an answer `INVALID_ARGUMENT` or `FAILED_PRECONDITION` as a refusal that fails
//! the job's start, and any other failure, or no answer at all, as "unavailable": it waits and
//! asks again. A [`Refusal`] keeps the code, so that a case can tell the two apart.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::client::conn::http2::{self, SendRequest};
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tierstore_trees_api::provider::v1::{
    MountMode, MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest,
    ReleaseResponse, Source,
};
use tokio::net::UnixStream;

/// The code of an answer that is not a success, as the agent tells them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    /// `UNAVAILABLE`: the agent waits and asks again.
    Unavailable,
    /// `FAILED_PRECONDITION`: the job's start fails.
    FailedPrecondition,
    /// `INVALID_ARGUMENT`: the job's start fails.
    InvalidArgument,
    /// Any other gRPC code, by its number: the agent waits and asks again.
    Other(i32),
    /// No answer: no connection, a connection that ended, or bytes that are no answer.
    NoAnswer,
}

/// A call that did not succeed: its code and the provider's reason, or why there was no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The code.
    pub code: Code,
    /// The reason.
    pub message: String,
}

impl Refusal {
    fn no_answer(message: impl std::fmt::Display) -> Self {
        Self {
            code: Code::NoAnswer,
            message: message.to_string(),
        }
    }

    /// Whether the agent would take this answer as a refusal that fails the job's start.
    #[must_use]
    pub fn fails_the_start(&self) -> bool {
        matches!(self.code, Code::FailedPrecondition | Code::InvalidArgument)
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

/// The source `index` of the run `run` of the agent `agent`, naming no attempt, as the agent on
/// main names its sources.
#[must_use]
pub fn source(agent: &str, run: &str, index: u32) -> Source {
    Source {
        agent: agent.to_owned(),
        run: run.to_owned(),
        index,
        attempt: 0,
    }
}

/// How long each call waits for its answer: the job agent's bounds of its calls (Prepare 10 s,
/// Mount 30 s, past the provider's own 25 s wait for a tree, Release 5 s). A call not answered
/// within its bound is no answer, as the agent takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// The bound of a Prepare.
    pub prepare: Duration,
    /// The bound of a Mount.
    pub mount: Duration,
    /// The bound of a Release.
    pub release: Duration,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            prepare: Duration::from_secs(10),
            mount: Duration::from_secs(30),
            release: Duration::from_secs(5),
        }
    }
}

/// A client of the socket at a path (module docs).
pub struct Provider {
    path: PathBuf,
    bounds: Bounds,
    sender: Option<SendRequest<Full<Bytes>>>,
}

impl Provider {
    /// A client of the socket at `path` whose calls wait as long as `bounds` say; it connects at
    /// its first call.
    #[must_use]
    pub fn new(path: &Path, bounds: Bounds) -> Self {
        Self {
            path: path.to_owned(),
            bounds,
            sender: None,
        }
    }

    /// Asks the provider to prepare `source` with the tree whose list is `tree`, read-only.
    ///
    /// # Errors
    /// The provider's refusal, or no answer.
    pub async fn prepare(&mut self, source: &Source, tree: &str) -> Result<(), Refusal> {
        let request = PrepareRequest {
            source: Some(source.clone()),
            r#ref: tree.to_owned(),
            mode: MountMode::ReadOnly as i32,
            host: String::new(),
        };
        self.call::<PrepareResponse>("Prepare", &request, self.bounds.prepare)
            .await
            .map(drop)
    }

    /// Asks for the directory of `source`.
    ///
    /// # Errors
    /// The provider's refusal, or no answer.
    pub async fn mount(&mut self, source: &Source) -> Result<PathBuf, Refusal> {
        let request = MountRequest {
            source: Some(source.clone()),
        };
        let answer = self
            .call::<MountResponse>("Mount", &request, self.bounds.mount)
            .await?;
        Ok(PathBuf::from(answer.path))
    }

    /// Tells the provider `source` is done with.
    ///
    /// # Errors
    /// The provider's refusal, or no answer.
    pub async fn release(&mut self, source: &Source) -> Result<(), Refusal> {
        let request = ReleaseRequest {
            source: Some(source.clone()),
        };
        self.call::<ReleaseResponse>("Release", &request, self.bounds.release)
            .await
            .map(drop)
    }

    /// Calls `method` with `message` on the kept connection, made first if there is none, waiting
    /// at most `bound` for the answer; a call that gets no answer drops the connection, so the
    /// next call makes a new one.
    async fn call<A: Message + Default>(
        &mut self,
        method: &str,
        message: &impl Message,
        bound: Duration,
    ) -> Result<A, Refusal> {
        let asked = async {
            let sender = match self.sender.take() {
                Some(sender) => sender,
                None => connect(&self.path).await?,
            };
            let (sender, answer) = exchange(sender, method, message).await;
            self.sender = sender;
            answer
        };
        tokio::time::timeout(bound, asked)
            .await
            .unwrap_or_else(|_| Err(Refusal::no_answer(format!("no answer within {bound:?}"))))
    }
}

/// A connection to the socket at `path`.
async fn connect(path: &Path) -> Result<SendRequest<Full<Bytes>>, Refusal> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(|why| Refusal::no_answer(format!("{}: {why}", path.display())))?;
    let (sender, connection) = http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
        .await
        .map_err(|why| Refusal::no_answer(format!("the HTTP/2 handshake: {why}")))?;
    tokio::spawn(connection);
    Ok(sender)
}

/// One call on `sender`: the sender to keep (none when the connection failed) and the answer.
async fn exchange<A: Message + Default>(
    mut sender: SendRequest<Full<Bytes>>,
    method: &str,
    message: &impl Message,
) -> (Option<SendRequest<Full<Bytes>>>, Result<A, Refusal>) {
    let bytes = message.encode_to_vec();
    let mut frame = Vec::with_capacity(5 + bytes.len());
    frame.push(0);
    frame.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    frame.extend_from_slice(&bytes);
    let request = hyper::Request::post(format!(
        "http://provider{}",
        tierstore_trees_api::provider::path(method)
    ))
    .header("content-type", "application/grpc")
    .header("te", "trailers")
    .body(Full::new(Bytes::from(frame)));
    let request = match request {
        Ok(request) => request,
        Err(why) => return (Some(sender), Err(Refusal::no_answer(why))),
    };
    let answered = async {
        let response = sender.send_request(request).await?;
        let (parts, body) = response.into_parts();
        let collected = body.collect().await?;
        Ok::<_, hyper::Error>((parts.headers, collected))
    };
    match answered.await {
        Ok((headers, collected)) => {
            let trailers = collected.trailers().cloned();
            (
                Some(sender),
                answer_of(&headers, trailers.as_ref(), &collected.to_bytes()),
            )
        }
        Err(why) => (None, Err(Refusal::no_answer(why))),
    }
}

/// The answer of a call whose response had `headers`, `trailers` and the body `data`.
fn answer_of<A: Message + Default>(
    headers: &hyper::HeaderMap,
    trailers: Option<&hyper::HeaderMap>,
    data: &[u8],
) -> Result<A, Refusal> {
    let status = tonic::Status::from_header_map(headers)
        .or_else(|| trailers.and_then(tonic::Status::from_header_map))
        .ok_or_else(|| Refusal::no_answer("an answer with no grpc-status"))?;
    let code = match status.code() {
        tonic::Code::Ok => {
            let message = data
                .get(5..)
                .ok_or_else(|| Refusal::no_answer("an answer shorter than its frame"))?;
            return A::decode(message).map_err(Refusal::no_answer);
        }
        tonic::Code::Unavailable => Code::Unavailable,
        tonic::Code::FailedPrecondition => Code::FailedPrecondition,
        tonic::Code::InvalidArgument => Code::InvalidArgument,
        other => Code::Other(other as i32),
    };
    Err(Refusal {
        code,
        message: status.message().to_owned(),
    })
}

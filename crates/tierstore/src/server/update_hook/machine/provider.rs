//! The node's mount provider, asked over its Unix socket as jobscheduler's agent asks it: one gRPC
//! call per connection, over HTTP/2, framed by hand (`tierstore_trees_api::provider`), on a
//! runtime of the call's own (the hook has no other); and the read of a tree it placed, through
//! the mount, with `std::fs`.
//!
//! Each call has the bound jobscheduler's agent gives it (`Prepare` 10 s, `Mount` 30 s, `Release`
//! 5 s), the connection included; a call that passes it, a socket nobody listens on, and an answer
//! that is not one of the protocol are [`ProviderFault::Silent`]; a gRPC status other than OK is
//! [`ProviderFault::Answered`], with the provider's message.
//!
//! The read walks the directory without following a link, reads every file whole, and gives up
//! past [`MAX_PATHS`] paths or a file over [`MAX_FILE_BYTES`] (a tree of the check is far
//! smaller), or after its own bound: it runs on a thread of its own, so that a mount that never
//! answers a read leaves the step free to report it. Paths are taken under the root, as every file
//! of the host is.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tierstore_trees_api::prost::Message;
use tierstore_trees_api::provider::v1::{
    MountMode, MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest,
    ReleaseResponse, Source,
};
use tokio::net::UnixStream;
use tonic::{Code, Status};

use super::super::ports::{MountProvider, ProviderCode, ProviderFault, TreeNode};
use super::host::under;
use crate::server::provider::check::{AGENT, RUN};

/// The longest a `Prepare` may take, as jobscheduler's agent bounds it.
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest a `Mount` may take: the provider answers "not ready" within 25 s.
pub const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a `Release` may take.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest the read of a tree may take.
pub const READ_TREE_TIMEOUT: Duration = Duration::from_secs(30);
/// The most paths a tree read through the mount may have.
pub const MAX_PATHS: usize = 64;
/// The most bytes a file of a tree read through the mount may have.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// The bounds of the calls and of the read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderTimes {
    /// `Prepare`.
    pub prepare: Duration,
    /// `Mount`.
    pub mount: Duration,
    /// `Release`.
    pub release: Duration,
    /// The read of a tree.
    pub read: Duration,
}

impl Default for ProviderTimes {
    fn default() -> Self {
        Self {
            prepare: PREPARE_TIMEOUT,
            mount: MOUNT_TIMEOUT,
            release: RELEASE_TIMEOUT,
            read: READ_TREE_TIMEOUT,
        }
    }
}

/// A client of the mount providers of the host under `root`.
pub struct ProviderSocket {
    root: PathBuf,
    times: ProviderTimes,
}

impl ProviderSocket {
    /// A client whose calls and reads are bounded by `times`.
    #[must_use]
    pub fn new(root: PathBuf, times: ProviderTimes) -> ProviderSocket {
        ProviderSocket { root, times }
    }

    /// Calls `method` with `message` on the socket `socket`, within `bound`.
    fn call<A: Message + Default>(
        &self,
        socket: &Path,
        method: &str,
        message: &impl Message,
        bound: Duration,
    ) -> Result<A, ProviderFault> {
        let at = under(&self.root, socket);
        let silent = |why: String| ProviderFault::Silent(format!("{method}: {why}"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| silent(format!("no runtime: {error}")))?;
        let frame = framed(message).map_err(silent)?;
        let path = tierstore_trees_api::provider::path(method);
        let answered = runtime.block_on(async {
            tokio::time::timeout(bound, ask(&at, &path, frame))
                .await
                .unwrap_or_else(|_| Err(format!("no answer within {} s", bound.as_secs())))
        });
        let (status, data) = answered.map_err(silent)?;
        if status.code() != Code::Ok {
            return Err(ProviderFault::Answered {
                code: code_of(status.code()),
                reason: status.message().to_owned(),
            });
        }
        unframed(&data).map_err(silent)
    }

    /// The check's source `index`.
    fn source(index: u32) -> Option<Source> {
        Some(Source {
            agent: AGENT.to_owned(),
            run: RUN.to_owned(),
            index,
        })
    }
}

/// The gRPC frame of `message`: no compression, its length, its bytes.
pub(super) fn framed(message: &impl Message) -> Result<Bytes, String> {
    let bytes = message.encode_to_vec();
    let len = u32::try_from(bytes.len()).map_err(|_| "a request too large".to_owned())?;
    let mut frame = Vec::with_capacity(bytes.len() + 5);
    frame.push(0);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&bytes);
    Ok(Bytes::from(frame))
}

/// The message of the one frame `data` holds.
pub(super) fn unframed<A: Message + Default>(data: &[u8]) -> Result<A, String> {
    let Some((head, body)) = data.split_first_chunk::<5>() else {
        return Err(format!("an answer of {} bytes, not a frame", data.len()));
    };
    let [flag, len @ ..] = *head;
    let len = u32::from_be_bytes(len) as usize;
    if flag != 0 || body.len() != len {
        return Err(format!(
            "an answer framed as {len} bytes (flag {flag}) with {} bytes",
            body.len()
        ));
    }
    A::decode(body).map_err(|error| format!("an answer that is not the message: {error}"))
}

/// Sends `frame` as the call `path` on a new connection to the socket `at`: the call's status and
/// the bytes of its answer.
async fn ask(at: &Path, path: &str, frame: Bytes) -> Result<(Status, Bytes), String> {
    let stream = UnixStream::connect(at)
        .await
        .map_err(|error| format!("{}: {error}", at.display()))?;
    let (mut sender, connection) = http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
        .await
        .map_err(|error| format!("no HTTP/2 connection: {error}"))?;
    tokio::spawn(connection);
    let request = hyper::Request::post(format!("http://provider{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(frame))
        .map_err(|error| format!("no request: {error}"))?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| format!("no answer: {error}"))?;
    let (parts, body) = response.into_parts();
    let collected = body
        .collect()
        .await
        .map_err(|error| format!("the answer ended: {error}"))?;
    let status = Status::from_header_map(&parts.headers)
        .or_else(|| collected.trailers().and_then(Status::from_header_map))
        .ok_or_else(|| format!("an answer with no grpc-status (HTTP {})", parts.status))?;
    Ok((status, collected.to_bytes()))
}

/// The port's name of a gRPC status.
pub(super) fn code_of(code: Code) -> ProviderCode {
    match code {
        Code::InvalidArgument => ProviderCode::InvalidArgument,
        Code::FailedPrecondition => ProviderCode::FailedPrecondition,
        Code::Unavailable => ProviderCode::Unavailable,
        other => ProviderCode::Other(other as i32),
    }
}

impl MountProvider for ProviderSocket {
    fn prepare(&self, socket: &Path, index: u32, reference: &str) -> Result<(), ProviderFault> {
        let request = PrepareRequest {
            source: Self::source(index),
            r#ref: reference.to_owned(),
            mode: MountMode::ReadOnly as i32,
            host: String::new(),
        };
        let _: PrepareResponse = self.call(socket, "Prepare", &request, self.times.prepare)?;
        Ok(())
    }

    fn mount(&self, socket: &Path, index: u32) -> Result<PathBuf, ProviderFault> {
        let request = MountRequest {
            source: Self::source(index),
        };
        let mounted: MountResponse = self.call(socket, "Mount", &request, self.times.mount)?;
        Ok(PathBuf::from(mounted.path))
    }

    fn release(&self, socket: &Path, index: u32) -> Result<(), ProviderFault> {
        let request = ReleaseRequest {
            source: Self::source(index),
        };
        let _: ReleaseResponse = self.call(socket, "Release", &request, self.times.release)?;
        Ok(())
    }

    fn read_tree(&self, dir: &Path) -> Result<BTreeMap<String, TreeNode>, String> {
        let at = under(&self.root, dir);
        let (sender, read) = mpsc::channel();
        // The thread is left to the end of the process if the mount never answers it.
        std::thread::spawn(move || sender.send(walk(&at)));
        read.recv_timeout(self.times.read).unwrap_or_else(|_| {
            Err(format!(
                "the read did not end within {} s",
                self.times.read.as_secs()
            ))
        })
    }
}

/// Every path below `root`, by its path from `root` with `/` between names.
fn walk(root: &Path) -> Result<BTreeMap<String, TreeNode>, String> {
    let mut nodes = BTreeMap::new();
    let mut dirs = vec![(String::new(), root.to_path_buf())];
    while let Some((name, dir)) = dirs.pop() {
        let listed = fs::read_dir(&dir).map_err(|error| shown(&name, "listed", &error))?;
        for entry in listed {
            let entry = entry.map_err(|error| shown(&name, "listed", &error))?;
            let file_name = entry.file_name();
            let leaf = file_name
                .to_str()
                .ok_or_else(|| format!("{name:?} holds a name that is not UTF-8"))?;
            let path = if name.is_empty() {
                leaf.to_owned()
            } else {
                format!("{name}/{leaf}")
            };
            if nodes.len() == MAX_PATHS {
                return Err(format!("more than {MAX_PATHS} paths"));
            }
            let kind = entry
                .file_type()
                .map_err(|error| shown(&path, "looked at", &error))?;
            let node = if kind.is_dir() {
                dirs.push((path.clone(), entry.path()));
                TreeNode::Dir
            } else if kind.is_symlink() {
                let target =
                    fs::read_link(entry.path()).map_err(|error| shown(&path, "read", &error))?;
                let target = target
                    .into_os_string()
                    .into_string()
                    .map_err(|_| format!("{path} links to a target that is not UTF-8"))?;
                TreeNode::Link(target)
            } else if kind.is_file() {
                TreeNode::File(read_file(&path, &entry.path())?)
            } else {
                return Err(format!("{path} is neither a file, a directory nor a link"));
            };
            nodes.insert(path, node);
        }
    }
    Ok(nodes)
}

/// The bytes of the file `at`, named `path`, up to [`MAX_FILE_BYTES`].
fn read_file(path: &str, at: &Path) -> Result<Vec<u8>, String> {
    let file = fs::File::open(at).map_err(|error| shown(path, "opened", &error))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| shown(path, "read", &error))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("{path} is larger than {MAX_FILE_BYTES} bytes"));
    }
    Ok(bytes)
}

/// `path` could not be `done`, with the system's words.
fn shown(path: &str, done: &str, error: &std::io::Error) -> String {
    let path = if path.is_empty() {
        "the directory"
    } else {
        path
    };
    format!("{path} could not be {done}: {error}")
}

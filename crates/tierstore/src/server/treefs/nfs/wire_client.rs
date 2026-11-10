//! A client of the NFS and MOUNT protocols for the tests, written by hand from RFC 1813 and RFC
//! 5531 and sharing nothing with the server's code: it frames a call, encodes its arguments, and
//! decodes the parts of the replies the tests look at.

use std::net::SocketAddr;

use nfsserve::tcp::{NFSTcp as _, NFSTcpListener};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use super::fs::TreeNfs;

pub(super) const MOUNT: u32 = 100_005;
pub(super) const NFS: u32 = 100_003;

pub(super) const GETATTR: u32 = 1;
pub(super) const SETATTR: u32 = 2;
pub(super) const LOOKUP: u32 = 3;
pub(super) const ACCESS: u32 = 4;
pub(super) const READLINK: u32 = 5;
pub(super) const READ: u32 = 6;
pub(super) const WRITE: u32 = 7;
pub(super) const CREATE: u32 = 8;
pub(super) const MKDIR: u32 = 9;
pub(super) const SYMLINK: u32 = 10;
pub(super) const MKNOD: u32 = 11;
pub(super) const REMOVE: u32 = 12;
pub(super) const RMDIR: u32 = 13;
pub(super) const RENAME: u32 = 14;
pub(super) const LINK: u32 = 15;
pub(super) const READDIRPLUS: u32 = 17;
pub(super) const FSINFO: u32 = 19;

/// Serves `fs` on a free loopback port; the address, and the task that serves.
pub(super) async fn serve(fs: TreeNfs) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = NFSTcpListener::bind("127.0.0.1:0", fs).await.unwrap();
    let address = SocketAddr::new(listener.get_listen_ip(), listener.get_listen_port());
    let task = tokio::spawn(async move {
        let _ = listener.handle_forever().await;
    });
    (address, task)
}

/// Words of XDR, big-endian and padded to four bytes, as a call's arguments are.
#[derive(Default)]
pub(super) struct Enc(pub Vec<u8>);

impl Enc {
    pub(super) fn u32(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(super) fn u64(mut self, value: u64) -> Self {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Variable-length opaque data: its length, the bytes, and the padding.
    pub(super) fn opaque(mut self, bytes: &[u8]) -> Self {
        self = self.u32(u32::try_from(bytes.len()).unwrap());
        self.0.extend_from_slice(bytes);
        self.0.resize(self.0.len().next_multiple_of(4), 0);
        self
    }

    /// Fixed-length opaque data (a cookie verifier).
    pub(super) fn fixed(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }
}

/// The credential of a call.
#[derive(Clone, Copy)]
pub(super) enum Cred {
    /// `AUTH_NONE`.
    None,
    /// `AUTH_SYS` of a user and a group.
    Sys(u32, u32),
}

/// The reply to a call, past the RPC header.
pub(super) struct Reply {
    /// The RPC accept status: 0 for success, 3 for a procedure that is not there.
    pub accepted: u32,
    /// The results.
    pub body: Vec<u8>,
}

/// The cursor over a reply's results.
pub(super) struct Dec<'a> {
    data: &'a [u8],
    at: usize,
}

/// The attributes of an entry, as far as the tests look at them.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Attr {
    pub kind: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub size: u64,
    pub fileid: u64,
    pub mtime: u32,
}

/// An entry of a READDIRPLUS reply.
#[derive(Debug)]
pub(super) struct Entry {
    pub fileid: u64,
    pub name: String,
    pub cookie: u64,
    pub attr: Option<Attr>,
    pub handle: Option<Vec<u8>>,
}

impl<'a> Dec<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    pub(super) fn u32(&mut self) -> u32 {
        let word = u32::from_be_bytes(self.data[self.at..self.at + 4].try_into().unwrap());
        self.at += 4;
        word
    }

    pub(super) fn u64(&mut self) -> u64 {
        let word = u64::from_be_bytes(self.data[self.at..self.at + 8].try_into().unwrap());
        self.at += 8;
        word
    }

    pub(super) fn opaque(&mut self) -> Vec<u8> {
        let length = self.u32() as usize;
        let bytes = self.data[self.at..self.at + length].to_vec();
        self.at += length.next_multiple_of(4);
        bytes
    }

    pub(super) fn skip(&mut self, bytes: usize) {
        self.at += bytes;
    }

    /// The attributes of an entry (`fattr3`).
    pub(super) fn attr(&mut self) -> Attr {
        let (kind, mode, nlink, uid, _gid) =
            (self.u32(), self.u32(), self.u32(), self.u32(), self.u32());
        let (size, _used) = (self.u64(), self.u64());
        self.skip(8);
        let _fsid = self.u64();
        let fileid = self.u64();
        let (_atime, mtime) = ((self.u32(), self.u32()), self.u32());
        self.skip(4 + 8);
        Attr {
            kind,
            mode,
            nlink,
            uid,
            size,
            fileid,
            mtime,
        }
    }

    /// An optional `fattr3`.
    pub(super) fn post_op_attr(&mut self) -> Option<Attr> {
        (self.u32() == 1).then(|| self.attr())
    }

    /// The entries of a READDIRPLUS reply after its header, and whether it reached the end.
    pub(super) fn entries(&mut self) -> (Vec<Entry>, bool) {
        let mut entries = Vec::new();
        while self.u32() == 1 {
            let fileid = self.u64();
            let name = String::from_utf8(self.opaque()).unwrap();
            let cookie = self.u64();
            let attr = self.post_op_attr();
            let handle = (self.u32() == 1).then(|| self.opaque());
            entries.push(Entry {
                fileid,
                name,
                cookie,
                attr,
                handle,
            });
        }
        (entries, self.u32() == 1)
    }

    /// Whether every byte was read.
    pub(super) fn done(&self) -> bool {
        self.at == self.data.len()
    }
}

/// A connection to a server.
pub(super) struct Client {
    stream: TcpStream,
    xid: u32,
}

impl Client {
    pub(super) async fn connect(address: SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(address).await.unwrap(),
            xid: 100,
        }
    }

    /// Calls `procedure` of `program` (version 3) with `args` under `cred`.
    pub(super) async fn call(
        &mut self,
        program: u32,
        procedure: u32,
        cred: Cred,
        args: Enc,
    ) -> Reply {
        self.send(program, procedure, cred, args).await;
        self.read().await
    }

    /// Sends the call, and does not wait for its reply: [`Client::read`] reads it.
    pub(super) async fn send(&mut self, program: u32, procedure: u32, cred: Cred, args: Enc) {
        self.xid += 1;
        let (flavor, body) = match cred {
            Cred::None => (0, Enc::default()),
            Cred::Sys(uid, gid) => (
                1,
                Enc::default()
                    .u32(7)
                    .opaque(b"test")
                    .u32(uid)
                    .u32(gid)
                    .u32(0),
            ),
        };
        let message = Enc::default()
            .u32(self.xid)
            .u32(0)
            .u32(2)
            .u32(program)
            .u32(3)
            .u32(procedure)
            .u32(flavor)
            .opaque(&body.0)
            .u32(0)
            .u32(0);
        let mut record = message.0;
        record.extend_from_slice(&args.0);
        let header = (1u32 << 31) | u32::try_from(record.len()).unwrap();
        self.stream.write_all(&header.to_be_bytes()).await.unwrap();
        self.stream.write_all(&record).await.unwrap();
    }

    /// Whether the server ends the connection cleanly: its side is shut down and nothing more
    /// comes (`true`), and not with a reset (`false`).
    pub(super) async fn ended_cleanly(&mut self) -> bool {
        let mut byte = [0u8; 1];
        matches!(self.stream.read(&mut byte).await, Ok(0))
    }

    /// The reply of the last call sent.
    pub(super) async fn read(&mut self) -> Reply {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).await.unwrap();
        let length = (u32::from_be_bytes(header) & !(1 << 31)) as usize;
        let mut bytes = vec![0u8; length];
        self.stream.read_exact(&mut bytes).await.unwrap();
        let mut reply = Dec::new(&bytes);
        assert_eq!(reply.u32(), self.xid, "the reply answers the call");
        assert_eq!(reply.u32(), 1, "it is a reply");
        assert_eq!(reply.u32(), 0, "the call was accepted");
        let _verifier = (reply.u32(), reply.opaque());
        let accepted = reply.u32();
        Reply {
            accepted,
            body: bytes[reply.at..].to_vec(),
        }
    }

    /// The handle of the export `path` (MOUNT, procedure 1).
    pub(super) async fn mount(&mut self, path: &str) -> Vec<u8> {
        let reply = self
            .call(
                MOUNT,
                1,
                Cred::Sys(0, 0),
                Enc::default().opaque(path.as_bytes()),
            )
            .await;
        assert_eq!(reply.accepted, 0);
        let mut body = Dec::new(&reply.body);
        assert_eq!(body.u32(), 0, "the mount is accepted");
        body.opaque()
    }
}

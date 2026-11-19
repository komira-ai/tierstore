//! A fake device shared by the tsfuse tests.
#![allow(dead_code, reason = "each test binary uses a part of the helpers")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tsfuse::ReplyDevice;

#[cfg(target_os = "linux")]
pub mod sock;

/// A decoded reply message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
    pub payload: Vec<u8>,
}

/// Records every reply as the concatenation of its parts; can be told to fail writes.
#[derive(Default)]
pub struct FakeDevice {
    msgs: Mutex<Vec<Vec<u8>>>,
    fail: AtomicBool,
}

impl FakeDevice {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn failing() -> Arc<Self> {
        let d = Self::new();
        d.fail.store(true, Ordering::SeqCst);
        d
    }

    pub fn raw(&self) -> Vec<Vec<u8>> {
        self.msgs.lock().unwrap().clone()
    }

    pub fn msgs(&self) -> Vec<Msg> {
        self.raw()
            .into_iter()
            .map(|m| Msg {
                len: u32::from_le_bytes(m[0..4].try_into().unwrap()),
                error: i32::from_le_bytes(m[4..8].try_into().unwrap()),
                unique: u64::from_le_bytes(m[8..16].try_into().unwrap()),
                payload: m[16..].to_vec(),
            })
            .collect()
    }
}

impl ReplyDevice for FakeDevice {
    fn send(&self, parts: &[&[u8]]) -> io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::other("device gone"));
        }
        self.msgs.lock().unwrap().push(parts.concat());
        Ok(())
    }
}

/// A request: `fuse_in_header` (uid, gid and pid 0) then `body`.
pub fn request(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
    let header = tsfuse::abi::InHeader {
        len: (tsfuse::abi::InHeader::SIZE + body.len()) as u32,
        opcode,
        unique,
        nodeid,
        ..tsfuse::abi::InHeader::default()
    };
    [header.as_bytes(), body].concat()
}

/// An INIT request (unique 1) of protocol `major.minor` offering `flags` (the high half in
/// `flags2`), with a readahead of 64 KiB.
pub fn init_request(major: u32, minor: u32, flags: u64) -> Vec<u8> {
    let init = tsfuse::abi::InitIn {
        major,
        minor,
        max_readahead: 65536,
        flags: flags as u32,
        flags2: (flags >> 32) as u32,
        ..tsfuse::abi::InitIn::default()
    };
    request(26, 1, 0, init.as_bytes())
}

/// A GETATTR of the root.
pub fn getattr(unique: u64) -> Vec<u8> {
    request(3, unique, 1, &[0; 16])
}

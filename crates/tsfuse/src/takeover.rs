//! Takeover (5.2, item 5): what a supervisor keeps so that a FUSE server can die or be replaced
//! without its callers seeing an error.
//!
//! The kernel ends the requests read on a `/dev/fuse` descriptor (the mount's or a clone) when
//! the last descriptor of that open file closes (`ECONNABORTED` for their callers), and keeps
//! them on its processing queue while any descriptor of it stays open
//! (`docs/design/tsfuse-kernel-findings.md`, K5). So a supervisor keeps a [`Handover`]: a
//! descriptor of **every** device of the connection, the mount's and every clone, with the terms
//! INIT agreed. The next server receives it ([`Handover::receive`], `SCM_RIGHTS`), and
//! [`crate::Session::resume`] serves it without INIT after sending `FUSE_NOTIFY_RESEND`: every
//! request the dead server read and did not answer comes back marked with
//! [`crate::abi::UNIQUE_RESEND`] and is answered once, under the unique as read again, on the
//! device that read it again (K4, K6).
//!
//! A handover is also a plain list of descriptors ([`Handover::into_fds`]): the terms travel in
//! a sealed memfd before the devices, so a store that keeps only descriptors (the service
//! manager's) can keep one. The terms are a small versioned record ([`TERMS_V1_LEN`] bytes):
//! later versions only append to it, and a reader reads the first [`TERMS_V1_LEN`] bytes of any
//! version from 1 on, so a previous release resumes from a newer one's handover and the other
//! way round.

use std::io::{self, IoSlice, IoSliceMut, Write as _};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::{MemfdFlags, SealFlags};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};

use crate::abi::{InitFlags, InitOut, Negotiated};

/// The most descriptors one `SCM_RIGHTS` message carries (`SCM_MAX_FD`).
const SCM_MAX_FD: usize = 253;

/// The most devices a handover holds: one message carries the terms and every device.
pub const MAX_DEVICES: usize = SCM_MAX_FD - 1;

/// The first bytes of the terms record and the whole data of a handover message.
const MAGIC: [u8; 8] = *b"tsfuseHO";

/// The version of the terms record this library writes.
const TERMS_VERSION: u32 = 1;

/// The length of version 1 of the terms record: the magic, the version, the device count, the
/// minor version and 4 reserved bytes, the offered, granted and missing flags, and the INIT
/// reply (`fuse_init_out`).
pub const TERMS_V1_LEN: usize = 48 + InitOut::SIZE;

/// The seals a terms memfd carries: nobody can change it once written.
const SEALS: SealFlags = SealFlags::SEAL
    .union(SealFlags::SHRINK)
    .union(SealFlags::GROW)
    .union(SealFlags::WRITE);

/// Why a handover could not be made, kept, passed or read.
#[derive(Debug, thiserror::Error)]
pub enum HandoverError {
    /// A handover with no device.
    #[error("a FUSE handover needs at least one device descriptor")]
    NoDevice,
    /// More devices than one message carries.
    #[error("a FUSE handover holds at most {MAX_DEVICES} device descriptors, not {devices}")]
    TooManyDevices {
        /// The devices given.
        devices: usize,
    },
    /// A `dup` of a device failed.
    #[error("duplicating a FUSE device descriptor failed: {0}")]
    Dup(#[source] io::Error),
    /// Writing or reading the terms memfd failed.
    #[error("the memfd of the FUSE handover's terms failed: {0}")]
    Terms(#[source] io::Error),
    /// The first descriptor is not a sealed memfd, so it is not read (a device would lose the
    /// request the read takes).
    #[error("the first descriptor of a FUSE handover is not its sealed terms memfd")]
    NotTerms,
    /// The terms are shorter than their first version.
    #[error(
        "the FUSE handover's terms are {len} bytes, shorter than the {TERMS_V1_LEN} of version 1"
    )]
    Short {
        /// The bytes read.
        len: usize,
    },
    /// The terms do not start with the magic.
    #[error("the FUSE handover's terms do not start with its magic")]
    Magic,
    /// The terms name version 0, which no release writes.
    #[error("the FUSE handover's terms name version {0}; versions start at 1")]
    Version(u32),
    /// The terms name another number of devices than came with them.
    #[error("the FUSE handover's terms name {recorded} devices and {received} came with them")]
    DeviceCount {
        /// The devices the terms name.
        recorded: u32,
        /// The device descriptors received.
        received: usize,
    },
    /// Sending or receiving on the socket failed.
    #[error("passing a FUSE handover on its socket failed: {0}")]
    Socket(#[source] io::Error),
    /// The socket's peer closed it without sending a handover.
    #[error("the socket of a FUSE handover closed before one was passed")]
    Closed,
    /// The message's descriptors did not fit (`MSG_CTRUNC`): some were lost.
    #[error("a FUSE handover message carried more descriptors than one handover holds")]
    Truncated,
    /// The message is not a handover.
    #[error("a message of {len} bytes on the socket of a FUSE handover is not one")]
    Message {
        /// The bytes of the message.
        len: usize,
    },
}

/// Every device descriptor of one FUSE connection, the mount's first, and the terms its INIT
/// agreed: what a supervisor keeps so that a server can be replaced with no request lost.
///
/// [`crate::Session::handover`] and [`crate::Serving::handover`] make one (a `dup` of each
/// device); [`crate::Session::resume`] serves one.
#[derive(Debug)]
pub struct Handover {
    pub(crate) negotiated: Negotiated,
    pub(crate) devices: Vec<OwnedFd>,
}

impl Handover {
    /// The handover of `devices` (the first is the one a resumed session sends RESEND on),
    /// initialised with `negotiated`.
    ///
    /// # Errors
    ///
    /// [`HandoverError::NoDevice`] or [`HandoverError::TooManyDevices`].
    pub fn new(negotiated: Negotiated, devices: Vec<OwnedFd>) -> Result<Self, HandoverError> {
        match devices.len() {
            0 => Err(HandoverError::NoDevice),
            n if n > MAX_DEVICES => Err(HandoverError::TooManyDevices { devices: n }),
            _ => Ok(Self {
                negotiated,
                devices,
            }),
        }
    }

    /// The terms INIT agreed.
    #[must_use]
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    /// The number of devices.
    #[must_use]
    pub fn devices(&self) -> usize {
        self.devices.len()
    }

    /// Another handover of the same open devices (a `dup` of each): what a supervisor keeps
    /// while it passes one on.
    ///
    /// # Errors
    ///
    /// [`HandoverError::Dup`].
    pub fn try_clone(&self) -> Result<Self, HandoverError> {
        Ok(Self {
            negotiated: self.negotiated,
            devices: dup_all(&self.devices)?,
        })
    }

    /// The handover as descriptors: a sealed memfd holding the terms, then every device in
    /// order. [`Self::from_fds`] reads it back.
    ///
    /// # Errors
    ///
    /// [`HandoverError::Terms`] when the memfd cannot be made, written or sealed.
    pub fn into_fds(self) -> Result<Vec<OwnedFd>, HandoverError> {
        let terms = write_terms(&encode_terms(&self.negotiated, self.devices.len()))
            .map_err(HandoverError::Terms)?;
        let mut fds = Vec::with_capacity(self.devices.len() + 1);
        fds.push(terms);
        fds.extend(self.devices);
        Ok(fds)
    }

    /// The handover of `fds` as [`Self::into_fds`] made them. The first descriptor is read only
    /// once it is known to be a sealed memfd.
    ///
    /// # Errors
    ///
    /// [`HandoverError::NoDevice`] for no descriptor; [`HandoverError::NotTerms`],
    /// [`HandoverError::Terms`], [`HandoverError::Short`], [`HandoverError::Magic`] or
    /// [`HandoverError::Version`] for the terms; [`HandoverError::DeviceCount`] when they name
    /// another number of devices than follow them.
    pub fn from_fds(fds: Vec<OwnedFd>) -> Result<Self, HandoverError> {
        let mut fds = fds.into_iter();
        let terms = fds.next().ok_or(HandoverError::NoDevice)?;
        let (negotiated, recorded) = decode_terms(&read_terms(&terms)?)?;
        let devices: Vec<OwnedFd> = fds.collect();
        if usize::try_from(recorded).ok() != Some(devices.len()) {
            return Err(HandoverError::DeviceCount {
                recorded,
                received: devices.len(),
            });
        }
        Self::new(negotiated, devices)
    }

    /// Passes the handover on `socket` (a connected unix socket, best `SOCK_SEQPACKET`) as one
    /// message: [`Self::into_fds`] of a `dup`, by `SCM_RIGHTS`. This handover keeps its
    /// descriptors.
    ///
    /// # Errors
    ///
    /// [`HandoverError::Dup`], [`HandoverError::Terms`] or [`HandoverError::Socket`].
    pub fn send(&self, socket: BorrowedFd<'_>) -> Result<(), HandoverError> {
        let fds = self.try_clone()?.into_fds()?;
        let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(AsFd::as_fd).collect();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(SCM_MAX_FD))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        // A handover holds at most `MAX_DEVICES` devices, so the rights always fit.
        let _fits = control.push(SendAncillaryMessage::ScmRights(&borrowed));
        rustix::net::sendmsg(
            socket,
            &[IoSlice::new(&MAGIC)],
            &mut control,
            SendFlags::NOSIGNAL,
        )
        .map_err(|e| HandoverError::Socket(e.into()))?;
        Ok(())
    }

    /// Waits for the handover [`Self::send`] passes on `socket` and takes it. Descriptors that
    /// came with a message that is not a handover are closed.
    ///
    /// # Errors
    ///
    /// [`HandoverError::Socket`], [`HandoverError::Closed`], [`HandoverError::Truncated`],
    /// [`HandoverError::Message`], or an error of [`Self::from_fds`].
    pub fn receive(socket: BorrowedFd<'_>) -> Result<Self, HandoverError> {
        let mut data = [0u8; 2 * MAGIC.len()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(SCM_MAX_FD))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let mut iov = [IoSliceMut::new(&mut data)];
        let got = rustix::net::recvmsg(socket, &mut iov, &mut control, RecvFlags::CMSG_CLOEXEC)
            .map_err(|e| HandoverError::Socket(e.into()))?;
        // Owned at once, so every descriptor received is closed on any error below.
        let fds: Vec<OwnedFd> = control
            .drain()
            .filter_map(|m| match m {
                RecvAncillaryMessage::ScmRights(rights) => Some(rights),
                _ => None,
            })
            .flatten()
            .collect();
        if got.flags.contains(ReturnFlags::CTRUNC) {
            return Err(HandoverError::Truncated);
        }
        match (got.bytes, fds.is_empty()) {
            (0, true) => Err(HandoverError::Closed),
            (len, _) if data[..len] != MAGIC => Err(HandoverError::Message { len }),
            _ => Self::from_fds(fds),
        }
    }
}

fn dup_all(fds: &[OwnedFd]) -> Result<Vec<OwnedFd>, HandoverError> {
    fds.iter()
        .map(|fd| rustix::io::fcntl_dupfd_cloexec(fd, 0))
        .collect::<rustix::io::Result<_>>()
        .map_err(|e| HandoverError::Dup(e.into()))
}

/// The terms record, version 1, little-endian: `magic [u8; 8]`, `version u32`, `devices u32`,
/// `minor u32`, 4 reserved bytes (0), `offered u64`, `granted u64`, `missing u64`, then the INIT
/// reply as written to the kernel.
pub(crate) fn encode_terms(negotiated: &Negotiated, devices: usize) -> Vec<u8> {
    // `Handover::new` holds at most `MAX_DEVICES`, far below `u32::MAX`.
    let devices = u32::try_from(devices).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(TERMS_V1_LEN);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&TERMS_VERSION.to_le_bytes());
    out.extend_from_slice(&devices.to_le_bytes());
    out.extend_from_slice(&negotiated.minor.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    for flags in [negotiated.offered, negotiated.granted, negotiated.missing] {
        out.extend_from_slice(&flags.0.to_le_bytes());
    }
    out.extend_from_slice(negotiated.reply.as_bytes());
    out
}

/// The terms and the device count of a record of any version from 1 on: its first
/// [`TERMS_V1_LEN`] bytes; what a later version appended is not read.
pub(crate) fn decode_terms(bytes: &[u8]) -> Result<(Negotiated, u32), HandoverError> {
    let Some(v1) = bytes.get(..TERMS_V1_LEN) else {
        return Err(HandoverError::Short { len: bytes.len() });
    };
    if v1[..8] != MAGIC {
        return Err(HandoverError::Magic);
    }
    let u32_at = |at: usize| u32::from_le_bytes([v1[at], v1[at + 1], v1[at + 2], v1[at + 3]]);
    let u64_at = |at: usize| u64::from(u32_at(at)) | (u64::from(u32_at(at + 4)) << 32);
    let version = u32_at(8);
    if version == 0 {
        return Err(HandoverError::Version(version));
    }
    // `v1` holds `InitOut::SIZE` bytes from 48, so the reply always reads.
    let reply = InitOut::read_prefix(&v1[48..])
        .map(|(reply, _)| reply)
        .unwrap_or_default();
    let negotiated = Negotiated {
        minor: u32_at(16),
        offered: InitFlags(u64_at(24)),
        granted: InitFlags(u64_at(32)),
        missing: InitFlags(u64_at(40)),
        reply,
    };
    Ok((negotiated, u32_at(12)))
}

/// A sealed memfd holding `bytes`.
fn write_terms(bytes: &[u8]) -> io::Result<OwnedFd> {
    let flags = MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING | MemfdFlags::NOEXEC_SEAL;
    let mut file = std::fs::File::from(rustix::fs::memfd_create("tsfuse-handover", flags)?);
    file.write_all(bytes)?;
    rustix::fs::fcntl_add_seals(&file, SEALS)?;
    Ok(file.into())
}

/// The first [`TERMS_V1_LEN`] bytes of the terms memfd `fd`, read only when `fd` carries the
/// seals [`write_terms`] adds.
fn read_terms(fd: &OwnedFd) -> Result<Vec<u8>, HandoverError> {
    match rustix::fs::fcntl_get_seals(fd) {
        Ok(seals) if seals.contains(SEALS) => {}
        _ => return Err(HandoverError::NotTerms),
    }
    let mut buf = vec![0u8; TERMS_V1_LEN];
    let n = rustix::io::pread(fd, &mut buf, 0).map_err(|e| HandoverError::Terms(e.into()))?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "a unit test: a failed unwrap is the failure it reports"
    )]

    use super::*;
    use crate::abi::{InitConfig, InitIn, InitOutcome};

    /// The terms of a tree mount on a kernel of protocol 7.45 offering `ASYNC_READ`,
    /// `HAS_RESEND` and `INIT_EXT`.
    fn negotiated() -> Negotiated {
        let flags = InitFlags::ASYNC_READ
            .union(InitFlags::HAS_RESEND)
            .union(InitFlags::INIT_EXT);
        let init = InitIn {
            major: 7,
            minor: 45,
            max_readahead: 65536,
            flags: (flags.0 & 0xffff_ffff) as u32,
            flags2: (flags.0 >> 32) as u32,
            ..InitIn::default()
        };
        match InitConfig::tree().negotiate(&init).unwrap() {
            InitOutcome::Accepted(n) => n,
            InitOutcome::MajorAhead(_) => unreachable!("7.45 is our major"),
        }
    }

    /// Version 1 of the record, byte for byte: the format a later release must keep reading.
    const V1: [&str; 7] = [
        "747366757365484f0100000003000000",
        "2d000000000000000100004080000000",
        "01000040800000008060c60100000000",
        "070000002d0000000000010001000040",
        "00040003001000000100000000000000",
        "80000000000000000000000000000000",
        "00000000000000000000000000000000",
    ];

    fn unhex(rows: &[&str]) -> Vec<u8> {
        let hex: String = rows.concat();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn version_1_of_the_terms_is_these_bytes() {
        // The magic; version 1, 3 devices, minor 45, 4 reserved bytes; offered and granted
        // (ASYNC_READ, INIT_EXT, HAS_RESEND); missing (the tree's other required flags); then
        // `fuse_init_out`: 7.45, readahead 64 KiB, flags, background 1024 and 768, max_write
        // 4096, time_gran 1, max_pages 0, flags2 HAS_RESEND, and zeros.
        let want = unhex(&V1);
        assert_eq!(want.len(), TERMS_V1_LEN);
        assert_eq!(TERMS_V1_LEN, 112);
        assert_eq!(encode_terms(&negotiated(), 3), want);
        assert_eq!(decode_terms(&want).unwrap(), (negotiated(), 3));
        let missing = InitConfig::tree()
            .kind
            .required()
            .difference(InitFlags::ASYNC_READ.union(InitFlags::HAS_RESEND));
        assert_eq!(decode_terms(&want).unwrap().0.missing, missing);
    }

    /// A later version appends; this reader reads its first 112 bytes and ignores the rest.
    #[test]
    fn a_later_version_with_appended_fields_reads_as_its_version_1_prefix() {
        let mut bytes = encode_terms(&negotiated(), 2);
        bytes[8] = 2;
        bytes.extend_from_slice(&[0xee; 40]);
        assert_eq!(decode_terms(&bytes).unwrap(), (negotiated(), 2));
    }

    #[test]
    fn terms_that_are_short_lack_the_magic_or_name_version_0_are_refused() {
        let bytes = encode_terms(&negotiated(), 1);
        let short = decode_terms(&bytes[..TERMS_V1_LEN - 1]).unwrap_err();
        assert_eq!(
            short.to_string(),
            "the FUSE handover's terms are 111 bytes, shorter than the 112 of version 1"
        );
        let mut magic = bytes.clone();
        magic[0] = b'x';
        let magic = decode_terms(&magic).unwrap_err();
        assert_eq!(
            magic.to_string(),
            "the FUSE handover's terms do not start with its magic"
        );
        let mut zero = bytes;
        zero[8] = 0;
        let zero = decode_terms(&zero).unwrap_err();
        assert_eq!(
            zero.to_string(),
            "the FUSE handover's terms name version 0; versions start at 1"
        );
    }

    #[test]
    fn the_terms_memfd_is_sealed_and_reads_back() {
        let fd = write_terms(&encode_terms(&negotiated(), 1)).unwrap();
        let seals = rustix::fs::fcntl_get_seals(&fd).unwrap();
        assert!(seals.contains(SEALS), "{seals:?}");
        assert_eq!(
            rustix::io::write(&fd, b"x").unwrap_err(),
            rustix::io::Errno::PERM
        );
        let read = read_terms(&fd).unwrap();
        assert_eq!(decode_terms(&read).unwrap(), (negotiated(), 1));
    }

    #[test]
    fn a_memfd_without_the_seals_or_a_socket_is_not_read_as_terms() {
        let unsealed = rustix::fs::memfd_create("t", MemfdFlags::CLOEXEC).unwrap();
        assert!(matches!(
            read_terms(&unsealed).unwrap_err(),
            HandoverError::NotTerms
        ));
        let (a, _b) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let err = read_terms(&OwnedFd::from(a)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the first descriptor of a FUSE handover is not its sealed terms memfd"
        );
    }
}

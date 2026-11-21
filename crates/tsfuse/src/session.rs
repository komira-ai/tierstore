//! A FUSE session (5.2): the connection's descriptors (the mount's and its clones), the terms
//! INIT agreed, and the readers that serve it.
//!
//! - [`Session::init`] runs INIT on the descriptor a mount made; [`Session::resume`] takes the
//!   [`Handover`] of a connection another process initialised (takeover, 5.2 item 5): every
//!   descriptor and the terms it recorded. It runs no INIT, and sends `FUSE_NOTIFY_RESEND` before
//!   any reader runs, so the requests the previous server read and did not answer come back and
//!   are answered once.
//! - [`Session::handover`] and [`Serving::handover`] give the [`Handover`] a supervisor keeps: a
//!   `dup` of every descriptor, the mount's and every clone, with the terms.
//! - [`Session::clone_readers`] binds clones (`FUSE_DEV_IOC_CLONE`) until the session has as
//!   many descriptors as readers; each has its own processing queue.
//! - [`Session::serve`] starts one reader per descriptor, each on a dedicated thread with its own
//!   current-thread runtime (5.4: a ready read runs `pread`, splice or an image page fault inline
//!   and may block that thread, never another reader). Each reader owns one read buffer, sized
//!   from the negotiated `max_write` (8 KiB for a tree mount): [`Fs::handle`] returns before the
//!   next read, and a parked operation keeps the few fields it needs, not the buffer.
//! - [`Stopper::stop`] ends every reader and leaves every descriptor open and the mount in place,
//!   so a supervisor that holds them can hand them to the next process. [`Serving::hand_over`]
//!   does that for a server about to exit: it stops and joins every reader, and from then on a
//!   parked request whose replier is dropped is left to the next server instead of being
//!   answered `EIO`. An unmount or an abort ends the readers with [`ReaderEnd::Unmounted`] (one that
//!   takes a request as the connection ends, with [`ReaderEnd::Aborted`]).

use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::abi::{
    InitConfig, InitError, InitFlags, InitOutcome, MIN_READ_BUFFER, Negotiated, Operation, WriteIn,
};
use crate::dev::DevFd;
use crate::fs::{Fs, IN_HEADER_LEN, Request};
use crate::opcode::Opcode;
use crate::reply::{Errno, ReplyDevice, encode_out_header};
use crate::takeover::{Handover, HandoverError};
use crate::tokens::Tokens;
use crate::transport::{DevReader, Every, ReaderEnd, Received, Stop, read_once, serve};
use crate::uring::{
    Fallback, Host, Rings, Shared, TransportMode, UringConfig, UringMemory, kernel_rings, plan,
};

/// How [`Session::init_uring`] makes the rings of a session: the real ones, or a test's.
pub(crate) type MakeRings<'a> =
    &'a mut dyn FnMut(&Arc<DevFd>, UringMemory) -> Result<Rings, Fallback>;

/// What a session asks of io_uring at INIT.
pub(crate) struct AskUring<'a> {
    pub(crate) config: &'a UringConfig,
    pub(crate) host: Host,
    pub(crate) make: MakeRings<'a>,
}

/// `FUSE_NOTIFY_RESEND`: the notify code of [`Session::resend`].
const NOTIFY_RESEND: i32 = 7;

/// Why a session could not be set up or served.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Reading the device failed.
    #[error("reading the FUSE device failed: {0}")]
    Read(#[source] io::Error),
    /// Writing a reply to the device failed.
    #[error("writing to the FUSE device failed: {0}")]
    Write(#[source] io::Error),
    /// The device ended before INIT was answered.
    #[error("the FUSE device ended before INIT: {0:?}")]
    EndedBeforeInit(ReaderEnd),
    /// The first request was not INIT: the connection was initialised by an earlier process,
    /// which only [`Session::resume`] can serve. The request was answered `EIO`.
    #[error(
        "the FUSE connection is already initialised: its first request (unique {unique}) is opcode {opcode}, not INIT"
    )]
    AlreadyInitialised {
        /// The request's opcode.
        opcode: u32,
        /// The request's id.
        unique: u64,
    },
    /// INIT was refused (answered `EPROTO`).
    #[error(transparent)]
    Init(#[from] InitError),
    /// INIT's body is shorter than its arguments (answered `EIO`).
    #[error("the FUSE INIT request has a body of {body_len} bytes, too short for its arguments")]
    MalformedInit {
        /// The bytes of the body.
        body_len: usize,
    },
    /// [`Session::resume`] could not send `FUSE_NOTIFY_RESEND` (the connection is gone).
    #[error("sending FUSE_NOTIFY_RESEND failed: {0}")]
    Resend(#[source] io::Error),
    /// Binding a clone failed.
    #[error("cloning the FUSE device (reader {index}) failed: {source}")]
    Clone {
        /// The index the clone would have had.
        index: usize,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// A reader's thread could not be started.
    #[error("starting FUSE reader {index} failed: {source}")]
    Start {
        /// The reader's index.
        index: usize,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// A reader stopped with an error.
    #[error("FUSE reader {index} failed: {source}")]
    Reader {
        /// The reader's index.
        index: usize,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// A reader's thread panicked (in [`Fs::handle`]).
    #[error("FUSE reader {index} panicked")]
    Panicked {
        /// The reader's index.
        index: usize,
    },
    /// [`Session::resume`] of a connection granted io_uring: a request a ring entry of the
    /// previous server held is never resent (5.2 item 5, K14), so the connection cannot be taken
    /// over.
    #[error("a FUSE connection served on io_uring cannot be taken over")]
    UringNotResumable,
}

/// The descriptors of one FUSE connection and the terms it was initialised with.
#[derive(Debug)]
pub struct Session {
    devices: Vec<Arc<DevFd>>,
    negotiated: Negotiated,
    tokens: Tokens,
    mode: TransportMode,
    /// The rings, when INIT asked for io_uring; registered by [`Self::serve`].
    rings: Option<Rings>,
}

impl Session {
    /// Runs INIT on `device`, the descriptor a mount made (left in blocking mode):
    /// reads the first request, answers it with the reply `config` negotiates and returns the
    /// session. A kernel of a newer major version is answered with ours and its next INIT is
    /// read. A required flag the kernel lacks does not refuse the session: it runs without it,
    /// and [`Negotiated::missing`] names it.
    ///
    /// # Errors
    ///
    /// [`SessionError::AlreadyInitialised`] when the first request is not INIT (it is answered
    /// `EIO`); [`SessionError::Init`] for a kernel too old (answered `EPROTO`);
    /// [`SessionError::MalformedInit`] (answered `EIO`); [`SessionError::EndedBeforeInit`];
    /// [`SessionError::Read`] or [`SessionError::Write`].
    pub fn init(device: DevFd, config: &InitConfig) -> Result<Self, SessionError> {
        Self::init_with(device, config, None)
    }

    /// As [`Self::init`], asking the kernel for `FUSE_OVER_IO_URING` when the session can use it
    /// (5.2 item 4): `fuse.enable_uring` is on, the kernel offers it, the possible CPUs can be
    /// read, the entries `uring` asks for register at most [`UringConfig::memory_limit`] with the
    /// payload the INIT terms fix, and a ring can be made for every queue; the rings are made
    /// before INIT is answered. Otherwise the session serves on `/dev/fuse`, and
    /// [`Self::transport`] says why ([`Fallback`]). An io_uring tree mount asks for `max_pages`
    /// 32 in `config` (see [`UringConfig`]).
    ///
    /// # Errors
    ///
    /// As [`Self::init`]: a fallback is not an error.
    pub fn init_uring(
        device: DevFd,
        config: &InitConfig,
        uring: &UringConfig,
    ) -> Result<Self, SessionError> {
        let ask = AskUring {
            config: uring,
            host: Host::read(),
            make: &mut kernel_rings,
        };
        Self::init_with(device, config, Some(ask))
    }

    /// INIT, with io_uring asked for as `uring` says.
    pub(crate) fn init_with(
        device: DevFd,
        config: &InitConfig,
        mut uring: Option<AskUring<'_>>,
    ) -> Result<Self, SessionError> {
        let device = Arc::new(device);
        let mut buf = vec![0u8; MIN_READ_BUFFER];
        loop {
            let len = read_blocking(&device, &mut buf)?;
            let bytes = &buf[..len];
            let Some(req) = Request::parse(bytes) else {
                return Err(SessionError::Read(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a FUSE request of {len} bytes has no valid header"),
                )));
            };
            let unique = req.header.unique;
            let answer = |error: Option<Errno>, payload: &[u8]| {
                let header = encode_out_header(unique, error, payload.len());
                device
                    .send(&[&header, payload])
                    .map_err(SessionError::Write)
            };
            if req.header.op() != Some(Opcode::Init) {
                answer(Some(Errno::EIO), &[])?;
                return Err(SessionError::AlreadyInitialised {
                    opcode: req.header.opcode,
                    unique,
                });
            }
            // Once the opcode is INIT, only a body shorter than an old kernel's 16 bytes fails
            // to parse as one.
            let Ok(Operation::Init(init)) = Operation::parse(&req) else {
                answer(Some(Errno::EIO), &[])?;
                return Err(SessionError::MalformedInit {
                    body_len: req.body.len(),
                });
            };
            match config.negotiate(&init) {
                Ok(InitOutcome::Accepted(mut negotiated)) => {
                    let ready = match uring.as_mut() {
                        None => Err(Fallback::NotAsked),
                        Some(ask) => plan(ask.config, &ask.host, &negotiated)
                            .and_then(|memory| (ask.make)(&device, memory)),
                    };
                    let (mode, rings) = match ready {
                        Ok(rings) => {
                            let over = InitFlags::OVER_IO_URING;
                            negotiated.granted = negotiated.granted.union(over);
                            negotiated.reply.flags2 |= (over.0 >> 32) as u32;
                            (TransportMode::Uring(rings.memory), Some(rings))
                        }
                        Err(fallback) => (TransportMode::DevFuse(fallback), None),
                    };
                    answer(None, negotiated.reply.as_bytes())?;
                    return Ok(Self {
                        devices: vec![device],
                        negotiated,
                        tokens: Tokens::new(),
                        mode,
                        rings,
                    });
                }
                Ok(InitOutcome::MajorAhead(ours)) => answer(None, ours.as_bytes())?,
                Err(e) => {
                    answer(Some(e.errno()), &[])?;
                    return Err(e.into());
                }
            }
        }
    }

    /// The session of a connection an earlier process initialised, from its `handover`: every
    /// descriptor it held, in order, and the terms its INIT agreed. No INIT is read, and the
    /// session serves exactly these descriptors (it binds no clone of its own).
    ///
    /// When the kernel offered `FUSE_HAS_RESEND`, [`Self::resend`] runs here, before any reader
    /// can read: every request a previous server read and did not answer, on any of the
    /// descriptors, goes back to the connection's input queue marked with
    /// [`crate::abi::UNIQUE_RESEND`], and is read and answered like a new one. A request this
    /// session reads is never resent to it, so none is carried out twice by this server. A kernel
    /// without it (before Linux 6.9) resends nothing: requests queued and not yet read are
    /// served, and one the previous server read waits until the connection ends.
    ///
    /// # Errors
    ///
    /// [`SessionError::UringNotResumable`] for a connection granted io_uring (its descriptors
    /// are dropped, the connection is left as it is); [`SessionError::Resend`] when the
    /// notification cannot be written.
    pub fn resume(handover: Handover) -> Result<Self, SessionError> {
        let session = Self::attach(handover)?;
        if session.negotiated.granted.contains(InitFlags::HAS_RESEND) {
            session.resend().map_err(SessionError::Resend)?;
        }
        Ok(session)
    }

    /// As [`Self::resume`], but sends no `FUSE_NOTIFY_RESEND`: for a server that knows no
    /// request of the connection is held unanswered by a server that is gone (the one it takes
    /// over from answered all it read before it let go, or this process itself stopped its
    /// readers and serves again), or that sends [`Self::resend`] itself and goes on when the
    /// kernel refuses it.
    ///
    /// # Errors
    ///
    /// [`SessionError::UringNotResumable`] for a connection granted io_uring (its descriptors
    /// are dropped, the connection is left as it is).
    pub fn attach(handover: Handover) -> Result<Self, SessionError> {
        let Handover {
            negotiated,
            devices,
        } = handover;
        if negotiated.granted.contains(InitFlags::OVER_IO_URING) {
            return Err(SessionError::UringNotResumable);
        }
        Ok(Self {
            devices: devices
                .into_iter()
                .map(|fd| Arc::new(DevFd::from_fd(fd)))
                .collect(),
            negotiated,
            tokens: Tokens::new(),
            mode: TransportMode::DevFuse(Fallback::NotAsked),
            rings: None,
        })
    }

    /// The terms INIT agreed.
    #[must_use]
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    /// The transport [`Self::serve`] will use: io_uring if INIT asked for it, which a refused
    /// REGISTER can still turn into `/dev/fuse` ([`Serving::transport`] has the outcome).
    #[must_use]
    pub fn transport(&self) -> TransportMode {
        self.mode
    }

    /// The number of descriptors, and so of `/dev/fuse` readers [`Self::serve`] starts.
    #[must_use]
    pub fn readers(&self) -> usize {
        self.devices.len()
    }

    /// Leaves every INTERRUPT unanswered from now on: no request of the session is answered
    /// `EINTR`, and each is answered by the file system alone, as when the kernel's interrupts
    /// are off. For a file system that answers its requests inline and whose answers must not be
    /// replaced (a commit an INTERRUPT would report as not made while it goes on); a file system
    /// that parks its requests until a wait ends keeps INTERRUPT (5.2 item 3). Call it before
    /// [`Self::serve`].
    pub fn ignore_interrupts(&self) {
        self.tokens.ignore_interrupts();
    }

    /// The session's table of registered requests.
    #[must_use]
    pub fn tokens(&self) -> &Tokens {
        &self.tokens
    }

    /// Clones the first descriptor until the session has `readers` descriptors (none when it
    /// has as many or more already).
    ///
    /// # Errors
    ///
    /// [`SessionError::Clone`]; the clones bound before it are kept.
    pub fn clone_readers(&mut self, readers: NonZeroUsize) -> Result<(), SessionError> {
        while self.devices.len() < readers.get() {
            let index = self.devices.len();
            let clone = self.devices[0]
                .clone_reader()
                .map_err(|source| SessionError::Clone { index, source })?;
            self.devices.push(Arc::new(clone));
        }
        Ok(())
    }

    /// The handover a supervisor keeps (5.2, item 5): a `dup` of every descriptor, the mount's
    /// first and then every clone, with the terms INIT agreed. Take it after
    /// [`Self::clone_readers`], so that it holds every clone: a request read on a clone the
    /// supervisor does not hold ends with `ECONNABORTED` when the server dies.
    ///
    /// # Errors
    ///
    /// [`HandoverError::Dup`], or [`HandoverError::TooManyDevices`] past
    /// [`crate::MAX_DEVICES`].
    pub fn handover(&self) -> Result<Handover, HandoverError> {
        handover_of(&self.devices, self.negotiated)
    }

    /// Sends `FUSE_NOTIFY_RESEND` on the first descriptor: the kernel moves the requests in the
    /// processing queue of every descriptor of the connection back to its pending queue, marked
    /// with [`crate::abi::UNIQUE_RESEND`] (a header-only notification: unique 0, the notify code
    /// in the error field).
    ///
    /// # Errors
    ///
    /// The device's write error.
    pub fn resend(&self) -> io::Result<()> {
        let mut header = encode_out_header(0, None, 0);
        header[4..8].copy_from_slice(&NOTIFY_RESEND.to_le_bytes());
        self.devices[0].send(&[&header])
    }

    /// Starts one reader per descriptor, each on its own thread, dispatching to `fs`. On
    /// io_uring it also starts one reader per queue, after its `/dev/fuse` readers (the hybrid
    /// delivery of 5.2 item 4), registers every queue's entries and returns once they are
    /// registered, or once a refused REGISTER has left the session on `/dev/fuse` alone.
    ///
    /// # Errors
    ///
    /// [`SessionError::Start`] when a thread cannot be started; the readers started before it
    /// are stopped.
    pub fn serve(self, fs: Arc<dyn Fs>) -> Result<Serving, SessionError> {
        let stop = Arc::new(Stop::default());
        let buf_len = read_buffer_len(&self.negotiated);
        let every: Vec<Arc<dyn ReplyDevice>> = self
            .devices
            .iter()
            .cloned()
            .map(|d| -> Arc<dyn ReplyDevice> { d })
            .collect();
        let every = Arc::new(Every(every));
        let mut readers = Vec::with_capacity(self.devices.len());
        for (index, device) in self.devices.iter().enumerate() {
            let (device, fs, tokens, reader_stop, every) = (
                device.clone(),
                fs.clone(),
                self.tokens.clone(),
                stop.clone(),
                every.clone(),
            );
            let reader = Reader {
                fs,
                tokens,
                stop: reader_stop,
                every,
                buf_len,
            };
            let spawned = std::thread::Builder::new()
                .name(format!("tsfuse-reader-{index}"))
                .spawn(move || reader.run(device));
            // On a failure, the readers started so far see the stop and end on their own.
            let handle = spawned.map_err(|source| stop.failed_start(index, source))?;
            readers.push(handle);
        }
        let (mode, queues) = match self.rings {
            None => (self.mode, Vec::new()),
            Some(rings) => {
                let first = readers.len();
                let started = crate::uring::serve::start(
                    rings,
                    &fs,
                    &self.tokens,
                    &stop,
                    &every,
                    buf_len,
                    first,
                )?;
                readers.extend(started.handles);
                (started.mode, started.queues)
            }
        };
        Ok(Serving {
            devices: self.devices,
            negotiated: self.negotiated,
            tokens: self.tokens,
            stop,
            readers,
            mode,
            queues,
        })
    }
}

/// The reader buffer for the negotiated terms: the kernel refuses a read into fewer than
/// `max(8 KiB, header + fuse_write_in + max_write)` bytes.
pub(crate) fn read_buffer_len(negotiated: &Negotiated) -> usize {
    let max_write = negotiated.reply.max_write as usize;
    MIN_READ_BUFFER.max(IN_HEADER_LEN + WriteIn::SIZE + max_write)
}

fn handover_of(devices: &[Arc<DevFd>], negotiated: Negotiated) -> Result<Handover, HandoverError> {
    let fds = devices
        .iter()
        .map(|d| d.try_clone_fd())
        .collect::<io::Result<_>>()
        .map_err(HandoverError::Dup)?;
    Handover::new(negotiated, fds)
}

/// One request read from `device`, which is put in blocking mode first (INIT is read before any
/// reader runs; [`DevReader::new`] makes it non-blocking again).
fn read_blocking(device: &DevFd, buf: &mut [u8]) -> Result<usize, SessionError> {
    device.set_nonblocking(false).map_err(SessionError::Read)?;
    loop {
        match read_once(device, buf) {
            Some(Ok(Received::Request(n))) => return Ok(n),
            Some(Ok(Received::Ended(end))) => return Err(SessionError::EndedBeforeInit(end)),
            Some(Err(e)) => return Err(SessionError::Read(e)),
            // A blocking read does not end with EAGAIN; read again if it does.
            None => {}
        }
    }
}

/// What a reader thread shares with the session.
struct Reader {
    fs: Arc<dyn Fs>,
    tokens: Tokens,
    stop: Arc<Stop>,
    every: Arc<Every>,
    buf_len: usize,
}

impl Reader {
    /// The body of a reader thread: its runtime, its transport, its buffer.
    fn run(self, device: Arc<DevFd>) -> io::Result<ReaderEnd> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()?;
        runtime.block_on(async {
            let transport = DevReader::new(device)?;
            let mut buf = vec![0u8; self.buf_len];
            let (fs, tokens) = (&*self.fs, &self.tokens);
            serve(transport, fs, tokens, &self.stop, &self.every, &mut buf).await
        })
    }
}

/// The readers of a session being served.
#[derive(Debug)]
pub struct Serving {
    devices: Vec<Arc<DevFd>>,
    negotiated: Negotiated,
    tokens: Tokens,
    stop: Arc<Stop>,
    /// The `/dev/fuse` readers, then (on io_uring) one per queue.
    readers: Vec<JoinHandle<io::Result<ReaderEnd>>>,
    mode: TransportMode,
    queues: Vec<Arc<Shared>>,
}

impl Serving {
    /// The handle that stops every reader.
    #[must_use]
    pub fn stopper(&self) -> Stopper {
        Stopper {
            stop: self.stop.clone(),
        }
    }

    /// The transport the session serves on (`fuse_transport_mode`).
    #[must_use]
    pub fn transport(&self) -> TransportMode {
        self.mode
    }

    /// The session's table of registered requests.
    #[cfg(test)]
    pub(crate) fn tokens(&self) -> &Tokens {
        &self.tokens
    }

    /// The ring entries holding a request the file system has not answered yet, over every
    /// queue (`uring_entries_parked`); 0 on `/dev/fuse`.
    #[must_use]
    pub fn uring_entries_held(&self) -> u64 {
        self.queues.iter().map(|q| q.held()).sum()
    }

    /// The handover a supervisor keeps, in reader order (see [`Session::handover`]). It is made
    /// for a session on io_uring too, but [`Session::resume`] refuses it
    /// ([`SessionError::UringNotResumable`]): io_uring has no takeover (5.2 item 5).
    ///
    /// # Errors
    ///
    /// As [`Session::handover`].
    pub fn handover(&self) -> Result<Handover, HandoverError> {
        handover_of(&self.devices, self.negotiated)
    }

    /// Hands the connection over to the next server (5.2, item 5): from now on a replier
    /// dropped without being completed writes nothing (its request stays on its device's
    /// processing queue, and the next server's [`Session::resume`] has it resent), then every
    /// reader is stopped and joined. Call it before this server exits, with the supervisor
    /// holding the [`Handover`]; a request read and not answered is then answered by the next
    /// server, never `EIO` by this one. A parked operation this server completes after the call
    /// is still written: before the next server's RESEND it answers the request, and after it
    /// the kernel refuses the write (`ENOENT`, [`crate::Completion::DeviceError`]) and the
    /// next server answers the resent request.
    ///
    /// # Errors
    ///
    /// As [`Self::join`].
    pub fn hand_over(self) -> Result<Vec<ReaderEnd>, SessionError> {
        self.tokens.hand_over();
        self.stop.stop();
        self.join()
    }

    /// Waits for every reader to end: how each ended, in reader order (the `/dev/fuse` readers,
    /// then the queues).
    ///
    /// # Errors
    ///
    /// The first reader that failed: [`SessionError::Reader`] (its runtime, its registration
    /// with it, or a read failed) or [`SessionError::Panicked`]; every reader is joined first.
    pub fn join(self) -> Result<Vec<ReaderEnd>, SessionError> {
        let ended: Vec<Result<ReaderEnd, SessionError>> = self
            .readers
            .into_iter()
            .enumerate()
            .map(|(index, handle)| match handle.join() {
                Ok(Ok(end)) => Ok(end),
                Ok(Err(source)) => Err(SessionError::Reader { index, source }),
                Err(_) => Err(SessionError::Panicked { index }),
            })
            .collect();
        ended.into_iter().collect()
    }
}

/// Stops every reader of a session: each finishes the request it is routing and ends with
/// [`ReaderEnd::Stopped`]. The descriptors stay open and the mount stays. Stopping again does
/// nothing.
#[derive(Debug, Clone)]
pub struct Stopper {
    stop: Arc<Stop>,
}

impl Stopper {
    /// Stops the readers.
    pub fn stop(&self) {
        self.stop.stop();
    }
}

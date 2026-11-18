//! A FUSE session (5.2): the connection's descriptors (the mount's and its clones), the terms
//! INIT agreed, and the readers that serve it.
//!
//! - [`Session::init`] runs INIT on the descriptor a mount made; [`Session::resume`] takes the
//!   descriptors of a connection another process initialised (takeover, T-F6), with the terms it
//!   recorded, and runs no INIT.
//! - [`Session::clone_readers`] binds clones (`FUSE_DEV_IOC_CLONE`) until the session has as
//!   many descriptors as readers; each has its own processing queue.
//! - [`Session::serve`] starts one reader per descriptor, each on a dedicated thread with its own
//!   current-thread runtime (5.4: a ready read runs `pread`, splice or an image page fault inline
//!   and may block that thread, never another reader). Each reader owns one read buffer, sized
//!   from the negotiated `max_write` (8 KiB for a tree mount): [`Fs::handle`] returns before the
//!   next read, and a parked operation keeps the few fields it needs, not the buffer.
//! - [`Stopper::stop`] ends every reader and leaves every descriptor open and the mount in place,
//!   so a supervisor that holds them can hand them to the next process. An unmount ends the
//!   readers with [`ReaderEnd::Unmounted`], an abort with [`ReaderEnd::Aborted`].

use std::io;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::abi::{
    InitConfig, InitError, InitOutcome, MIN_READ_BUFFER, Negotiated, Operation, WriteIn,
};
use crate::dev::DevFd;
use crate::fs::{Fs, IN_HEADER_LEN, Request};
use crate::opcode::Opcode;
use crate::reply::{Errno, ReplyDevice, encode_out_header};
use crate::tokens::Tokens;
use crate::transport::{DevReader, Every, ReaderEnd, Received, Stop, read_once, serve};

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
    /// [`Session::resume`] was given no descriptor.
    #[error("a FUSE session needs at least one device descriptor")]
    NoDevice,
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
}

/// The descriptors of one FUSE connection and the terms it was initialised with.
#[derive(Debug)]
pub struct Session {
    devices: Vec<Arc<DevFd>>,
    negotiated: Negotiated,
    tokens: Tokens,
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
                Ok(InitOutcome::Accepted(negotiated)) => {
                    answer(None, negotiated.reply.as_bytes())?;
                    return Ok(Self {
                        devices: vec![Arc::new(device)],
                        negotiated,
                        tokens: Tokens::new(),
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

    /// The session of a connection an earlier process initialised: `devices` are every
    /// descriptor it held (the first is the one [`Self::resend`] writes to) and `negotiated` the
    /// terms its INIT agreed. No INIT is read; requests queued before the call are served.
    ///
    /// # Errors
    ///
    /// [`SessionError::NoDevice`] when `devices` is empty.
    pub fn resume(devices: Vec<DevFd>, negotiated: Negotiated) -> Result<Self, SessionError> {
        if devices.is_empty() {
            return Err(SessionError::NoDevice);
        }
        Ok(Self {
            devices: devices.into_iter().map(Arc::new).collect(),
            negotiated,
            tokens: Tokens::new(),
        })
    }

    /// The terms INIT agreed.
    #[must_use]
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    /// The number of descriptors, and so of readers [`Self::serve`] starts.
    #[must_use]
    pub fn readers(&self) -> usize {
        self.devices.len()
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

    /// A `dup` of every descriptor, in order: what a supervisor holds so that a restart loses no
    /// in-flight request (5.2, item 5).
    ///
    /// # Errors
    ///
    /// The `dup` error.
    pub fn dup_devices(&self) -> io::Result<Vec<OwnedFd>> {
        dup_all(&self.devices)
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

    /// Starts one reader per descriptor, each on its own thread, dispatching to `fs`.
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
        Ok(Serving {
            devices: self.devices,
            stop,
            readers,
        })
    }
}

/// The reader buffer for the negotiated terms: the kernel refuses a read into fewer than
/// `max(8 KiB, header + fuse_write_in + max_write)` bytes.
pub(crate) fn read_buffer_len(negotiated: &Negotiated) -> usize {
    let max_write = negotiated.reply.max_write as usize;
    MIN_READ_BUFFER.max(IN_HEADER_LEN + WriteIn::SIZE + max_write)
}

fn dup_all(devices: &[Arc<DevFd>]) -> io::Result<Vec<OwnedFd>> {
    devices.iter().map(|d| d.try_clone_fd()).collect()
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
    stop: Arc<Stop>,
    readers: Vec<JoinHandle<io::Result<ReaderEnd>>>,
}

impl Serving {
    /// The handle that stops every reader.
    #[must_use]
    pub fn stopper(&self) -> Stopper {
        Stopper {
            stop: self.stop.clone(),
        }
    }

    /// A `dup` of every descriptor, in reader order (see [`Session::dup_devices`]).
    ///
    /// # Errors
    ///
    /// The `dup` error.
    pub fn dup_devices(&self) -> io::Result<Vec<OwnedFd>> {
        dup_all(&self.devices)
    }

    /// Waits for every reader to end: how each ended, in reader order.
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

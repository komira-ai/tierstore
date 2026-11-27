//! The hand-over of the mount from a running `tierstore fs` to its successor, `handoff-fs-v1`
//! (`docs/design/virtual-build-fs.md` section 7.3). It is built on the frames of the hand-off of
//! the listening sockets (`server/handoff/wire.rs`) and is a conversation of its own, on the
//! running instance's own socket, `fs.handoff_socket`:
//!
//! ```text
//! new                                                       running
//!  | take-fs 1 / ready ─────────────────────────────────▶    |
//!  | ◀── offer-fs / fuse <mountpoint> / control <path>   (+ the /dev/fuse and the control socket)
//!  | the running instance is its user? the paths are those of its own settings?
//!  | the device is the FUSE device, the control socket listens?
//!  | the state can be read, the descriptor is in the store   |
//!  | ack ───────────────────────────────────────────────▶    |
//!  |                                  each reader answers the request it holds and stops;
//!  |                                  the control socket stops accepting; the counts are written
//!  | ◀── done                                                |
//!  | makes its state from the runtime directory; reads nothing of the device yet
//!  | ready ─────────────────────────────────────────────▶    | removes its descriptor store's
//!  |                                                          | entry; never reads the device again
//!  | ◀── bye                                                  | ends with status 0
//!  | reads the device, accepts on the control socket          |
//! ```
//!
//! Or `refused` and a reason, for a request not understood or a taker that is not the user of the
//! running instance; `nak` from the taker, at any point before `ack`, leaves the running instance
//! as it was. A taker that is gone before `ready` (or does not send it
//! within `fs.handoff_wait`) has not taken the mount: the running instance reads requests again,
//! accepts on its control socket again and binds its hand-over socket again. The running instance
//! gives up nothing before `done` and the successor holds the descriptor in the service manager's
//! store before `ack`, so at every moment someone holds the connection; the running instance reads
//! the device until `ack` and never after `ready`, and the successor only after `bye`, so no
//! request is read by both.
//!
//! The request's second line, `ready`, asks for the `bye`. A running instance of a release before
//! it answers `refused` and "the request is not understood", and the taker asks again with
//! `take-fs 1` alone: then the running instance ends at `ready` with no answer, as it did, and the
//! taker reads the device once it has sent `ready`. A taker of such a release asks `take-fs 1`
//! alone and is answered as before.
//!
//! The running instance's socket file is removed when it says `done`; the successor binds its own
//! after `bye`. Mode 0600; a taker of another user is refused, and a taker refuses a running
//! instance of another user.

use std::io::ErrorKind;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener as StdListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};

use crate::server::handoff::wire::{self, Frame};
use crate::server::handoff::{Claim, Holding, Mount, check_control};
use crate::server::{ACCEPT_PAUSE, pause_after};

use super::run::Running;
use super::sd::{FUSE_NAME, Manager};

/// What the frames of a conversation are made of.
const TAKE: &str = "take-fs 1";
/// The request's second line, and the taker's frame once it serves.
const READY: &str = "ready";
/// The running instance's answer to `ready`, after which the taker reads the device.
const BYE: &str = "bye";
/// Why a request is refused when it is not one the running instance understands.
const NOT_UNDERSTOOD: &str = "the request is not understood";
/// Why a request is refused while a taker of the one hand-over is at the mount.
const BUSY: &str = "another taker is taking the mount";
/// Why a request is refused once the mount was given up on the one hand-over.
const GONE: &str = "this instance holds no mount";

/// Why a hand-over did not happen.
#[derive(Debug, thiserror::Error)]
pub(in crate::server) enum HandoverError {
    /// A socket could not be used.
    #[error("hand-over: {what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },
    /// The running instance did not take the request.
    #[error("hand-over: the running instance refused: {0}")]
    Refused(String),
    /// The running instance sent something that is not the protocol.
    #[error("hand-over: the running instance sent {0}")]
    Protocol(&'static str),
    /// The running instance serves other paths than these settings name.
    #[error("hand-over: the running instance serves {offered:?}, these settings name {wanted:?}")]
    Mismatch {
        /// What the running instance offered.
        offered: Vec<String>,
        /// What these settings name.
        wanted: Vec<String>,
    },
    /// What the successor had to do before it could take the mount could not be done.
    #[error("hand-over: not taken: {0}")]
    Unprepared(String),
    /// The running instance did not finish in time.
    #[error("hand-over: no end of the hand-over at {} in time", .0.display())]
    Timeout(PathBuf),
    /// The running instance could not serve again after a hand-over that did not complete.
    #[error("hand-over: the instance could not serve again: {0}")]
    Stuck(std::io::Error),
}

impl HandoverError {
    /// A system error, with what was being done.
    fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> Self {
        move |source| Self::Io { what, source }
    }
}

/// Where the paths of the conversation are, and how long it may last.
pub(super) struct Paths<'a> {
    /// The running instance's hand-over socket.
    pub socket: &'a Path,
    /// `fs.mountpoint`.
    pub mountpoint: &'a Path,
    /// `fs.control_socket`.
    pub control: &'a Path,
    /// `fs.handoff_wait`.
    pub wait: Duration,
    /// The check of the offered device's kind: [`check_device`].
    pub device: fn(BorrowedFd<'_>) -> std::io::Result<()>,
}

/// The FUSE character device (`/dev/fuse`): its major and minor numbers, which the kernel fixes
/// (`MISC_MAJOR`, `FUSE_MINOR`).
const FUSE_DEVICE: (u32, u32) = (10, 229);

/// Whether a file of `mode` and device number `rdev` is the FUSE device.
pub(super) fn is_fuse(mode: u32, rdev: u64) -> bool {
    let character =
        rustix::fs::FileType::from_raw_mode(mode) == rustix::fs::FileType::CharacterDevice;
    character && (rustix::fs::major(rdev), rustix::fs::minor(rdev)) == FUSE_DEVICE
}

/// Checks that `fd` is the FUSE device, as the offer of a mount says it is.
///
/// # Errors
/// `InvalidData` when it is not; what the system said when it cannot be asked.
pub(in crate::server) fn check_device(fd: BorrowedFd<'_>) -> std::io::Result<()> {
    let stat = rustix::fs::fstat(fd)?;
    if is_fuse(stat.st_mode, stat.st_rdev) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "the offered device is not the FUSE device",
        ))
    }
}

/// The lines of an offer of the mount at `mountpoint` and the control socket at `control`.
pub(super) fn offer_lines(mountpoint: &Path, control: &Path) -> Vec<String> {
    vec![
        "offer-fs".to_owned(),
        format!("fuse {}", mountpoint.display()),
        format!("control {}", control.display()),
    ]
}

/// What a resume that cannot serve the mount again is told: the error.
pub(super) type Stuck = Arc<dyn Fn(&std::io::Error) + Send + Sync>;

/// The mount of `running`, served with `settings`, as the one hand-over offers it
/// (`server/handoff/`: `take 1`, `ready`, `mount`): shared, quiesced and resumed as `take-fs 1`
/// does it, within `settings.handoff_wait`, and at `ready` given up for good, with the
/// descriptor's entry of `manager`'s store removed. `claim` is the one the instance's `take-fs 1`
/// offer has too ([`Offer::with_claim`]). A resume that cannot serve the mount again calls `stuck`
/// with the error: the start stops the process there, so that its unit is started again and adopts
/// the descriptor from its store.
pub(in crate::server) fn offered(
    running: &Arc<Running>,
    settings: &super::config::FsSettings,
    manager: Manager,
    (stuck, claim): (Stuck, Arc<Claim>),
) -> Mount {
    // Weak: the offer that holds these does not keep the mount's state alive, so a process that
    // ends its service of the mount (a hang, a stop) takes it whole whatever the offer is doing.
    let weak = Arc::downgrade(running);
    let (sharing, quiescing, resuming, leaving) = (weak.clone(), weak.clone(), weak.clone(), weak);
    Mount {
        mountpoint: settings.mountpoint.clone(),
        control: settings.control_socket.clone(),
        share: Arc::new(move || alive(&sharing)?.share()),
        quiesce: Arc::new(move || drop(alive(&quiescing).map(|running| running.quiesce()))),
        resume: Arc::new(move || {
            let resumed = alive(&resuming).map(|running| running.resume());
            stuck_on(resumed.unwrap_or(Ok(())), &*stuck);
        }),
        give_up: Arc::new(move || {
            drop(alive(&leaving).map(|running| running.give_up()));
            if let Err(why) = manager.unstore(FUSE_NAME) {
                tracing::warn!("tierstore: the descriptor's store entry was not removed: {why}");
            }
        }),
        wait: settings.handoff_wait,
        claim,
    }
}

/// The mount's state, while the process still serves it.
///
/// # Errors
/// `NotFound`: the process has ended its service of the mount.
fn alive(running: &std::sync::Weak<Running>) -> std::io::Result<Arc<Running>> {
    running.upgrade().ok_or_else(|| {
        std::io::Error::new(ErrorKind::NotFound, "the mount is no longer served here")
    })
}

/// Calls `stuck` with the error of a resume that failed: the mount cannot be served again.
pub(super) fn stuck_on(resumed: std::io::Result<()>, stuck: &dyn Fn(&std::io::Error)) {
    if let Err(why) = resumed {
        tracing::error!("tierstore: the mount cannot be served again: {why}");
        stuck(&why);
    }
}

/// The mount taken from a running instance: its descriptors, and the connection to it.
pub(super) struct Taken {
    /// The `/dev/fuse` descriptor.
    pub device: OwnedFd,
    /// The control socket's listening socket.
    pub control: StdListener,
    /// The connection to the running instance.
    pub notice: Notice,
}

/// The connection to the instance that handed the mount over, and whether it answers `ready` with
/// `bye`.
pub(super) struct Notice(UnixStream, bool);

impl Notice {
    /// Tells the running instance that this one serves the mount: it ends. Where it answers `bye`
    /// (it knows the step), this returns once it has: from then on it never reads the device, and
    /// the successor may. A running instance of a release before the step ends with no answer.
    ///
    /// # Errors
    /// What the system said; for an instance that answers, a connection that ended or a frame
    /// other than `bye` (it gave up first and serves on: the successor must not serve beside it).
    pub(super) async fn ready(&self) -> Result<(), HandoverError> {
        let io = HandoverError::io;
        wire::send(&self.0, &[READY.to_owned()], &[])
            .await
            .map_err(io("telling the running instance it can end"))?;
        if !self.1 {
            return Ok(());
        }
        let answer = wire::recv(&self.0)
            .await
            .map_err(io("waiting for the running instance's bye"))?;
        if answer.lines == [BYE] {
            Ok(())
        } else {
            Err(HandoverError::Protocol(
                "something other than bye after ready",
            ))
        }
    }
}

/// What a taker does with the descriptor before it says `ack`. It is not generic, so that the whole
/// exchange is one function, as the coverage of the branches of a generic one is counted for each
/// instantiation.
pub(super) type Prepare<'a> = Box<dyn FnMut(&OwnedFd) -> Result<(), String> + 'a>;

/// Takes the mount from the instance that answers at `paths.socket`. `prepare` runs once the offer
/// is checked and before the running instance is told to stop: it puts the descriptor in the
/// service manager's store and reads the state the successor starts from, and what it gives is
/// not returned. If it fails the running instance is told `nak` and goes on.
///
/// `Ok(None)` when no instance answers there: there is no file, or a file no one listens on.
///
/// # Errors
/// [`HandoverError`]: the running instance refused or did not keep to the protocol, serves other
/// paths than `paths` names, did not finish within `paths.wait`, or `prepare` failed.
pub(super) async fn take(
    paths: &Paths<'_>,
    prepare: Prepare<'_>,
) -> Result<Option<Taken>, HandoverError> {
    take_as(paths, prepare, rustix::process::geteuid().as_raw()).await
}

/// [`take`] from a running instance that must be the user `uid` (the tests name another).
pub(super) async fn take_as(
    paths: &Paths<'_>,
    mut prepare: Prepare<'_>,
    uid: u32,
) -> Result<Option<Taken>, HandoverError> {
    let exchanging = async {
        match exchange(paths, &mut prepare, (uid, true)).await? {
            Asked::Refused(why) if why == [NOT_UNDERSTOOD] => {
                // A release before the `ready` line: asked as it understands.
                exchange(paths, &mut prepare, (uid, false)).await
            }
            asked => Ok(asked),
        }
    };
    match tokio::time::timeout(paths.wait, exchanging).await {
        Ok(Ok(Asked::Taken(taken))) => Ok(taken),
        Ok(Ok(Asked::Refused(why))) => Err(HandoverError::Refused(why.join(" "))),
        Ok(Err(why)) => Err(why),
        Err(_) => Err(HandoverError::Timeout(paths.socket.to_owned())),
    }
}

/// How one request ended: the mount taken (or none, where nothing answers), or a refusal and its
/// reason.
enum Asked {
    Taken(Option<Taken>),
    Refused(Vec<String>),
}

/// One exchange with the running instance, from a taker that requires it to be the user `uid`
/// and, with `bye`, asks for the `bye` step.
async fn exchange(
    paths: &Paths<'_>,
    prepare: &mut Prepare<'_>,
    (uid, bye): (u32, bool),
) -> Result<Asked, HandoverError> {
    let stream = match UnixStream::connect(paths.socket).await {
        Ok(stream) => stream,
        Err(why)
            if matches!(
                why.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(Asked::Taken(None));
        }
        Err(why) => return Err(HandoverError::io("connecting to the hand-over socket")(why)),
    };
    let io = HandoverError::io;
    let peer = stream
        .peer_cred()
        .map_err(io("asking the running instance's user"))?;
    if peer.uid() != uid {
        let why = format!("the running instance is the user {}", peer.uid());
        let stranger = std::io::Error::new(ErrorKind::PermissionDenied, why);
        return Err(io("checking the running instance's user")(stranger));
    }
    let mut request = vec![TAKE.to_owned()];
    if bye {
        request.push(READY.to_owned());
    }
    wire::send(&stream, &request, &[])
        .await
        .map_err(io("asking for the mount"))?;
    let Frame { lines, mut fds } = wire::recv(&stream).await.map_err(io("reading the offer"))?;
    let (head, rest) = lines
        .split_first()
        .map_or(("", &[][..]), |(h, r)| (h.as_str(), r));
    if head == "refused" {
        return Ok(Asked::Refused(rest.to_vec()));
    }
    if head != "offer-fs" || fds.len() != 2 {
        return Err(HandoverError::Protocol(
            "something that is not an offer of the mount",
        ));
    }
    let wanted = offer_lines(paths.mountpoint, paths.control);
    if rest != &wanted[1..] {
        nak(&stream).await?;
        return Err(HandoverError::Mismatch {
            offered: rest.to_vec(),
            wanted: wanted[1..].to_vec(),
        });
    }
    let control = fds.remove(1);
    let device = fds.remove(0);
    let kinds = (paths.device)(device.as_fd()).and_then(|()| check_control(control.as_fd()));
    if let Err(why) = kinds {
        nak(&stream).await?;
        return Err(HandoverError::Unprepared(why.to_string()));
    }
    if let Err(why) = prepare(&device) {
        nak(&stream).await?;
        return Err(HandoverError::Unprepared(why));
    }
    wire::send(&stream, &["ack".to_owned()], &[])
        .await
        .map_err(io("accepting the offer"))?;
    let done = wire::recv(&stream)
        .await
        .map_err(io("waiting for the end of the hand-over"))?;
    if done.lines != ["done"] {
        return Err(HandoverError::Protocol(
            "something other than done after ack",
        ));
    }
    let taken = Taken {
        device,
        control: StdListener::from(control),
        notice: Notice(stream, bye),
    };
    Ok(Asked::Taken(Some(taken)))
}

/// Refuses the offer: the running instance goes on.
async fn nak(stream: &UnixStream) -> Result<(), HandoverError> {
    wire::send(stream, &["nak".to_owned()], &[])
        .await
        .map_err(HandoverError::io("refusing the offer"))
}

/// The running instance's side: its hand-over socket, and the offer of the mount to the one taker
/// at a time that asks.
pub(super) struct Offer {
    /// The socket, or none when it could not be made again after a hand-over that failed.
    listener: Option<UnixListener>,
    path: PathBuf,
    paths: (PathBuf, PathBuf),
    wait: Duration,
    /// The user a taker must be.
    uid: u32,
    /// The service manager whose store holds the descriptor: its entry is removed at `ready`.
    manager: Manager,
    /// Which conversation has the mount: this one, or the one hand-over of the same process.
    claim: Arc<Claim>,
}

impl Offer {
    /// Makes the socket at `path` (a file left by an instance that is gone is replaced), mode
    /// 0600, to offer the mount at `mountpoint` and the control socket at `control`.
    ///
    /// # Errors
    /// What the system said when the socket cannot be made.
    pub(super) fn bind(
        path: &Path,
        mountpoint: &Path,
        control: &Path,
        wait: Duration,
    ) -> Result<Self, HandoverError> {
        Ok(Self {
            listener: Some(socket(path)?),
            path: path.to_owned(),
            paths: (mountpoint.to_owned(), control.to_owned()),
            wait,
            uid: rustix::process::geteuid().as_raw(),
            manager: Manager::default(),
            claim: Arc::default(),
        })
    }

    /// The same, sharing `claim` with the one hand-over of the same process
    /// (`server/handoff/offer.rs`: `Offer::with_mount`): one taker at a time is at the mount, on
    /// either conversation; once it is given up on that one, a request here is refused, and the
    /// socket's path, which the successor binds after its `bye`, is not removed by this offer.
    pub(super) fn with_claim(mut self, claim: Arc<Claim>) -> Self {
        self.claim = claim;
        self
    }

    /// The same, removing the descriptor's entry of `manager`'s store at `ready`: from then on the
    /// mount is the successor's, and a start of this unit that adopted the entry would read the
    /// device beside it.
    pub(super) fn with_manager(mut self, manager: Manager) -> Self {
        self.manager = manager;
        self
    }

    /// The same, for a taker of the user `uid`: the tests give another one.
    #[cfg(test)]
    pub(super) fn of_user(mut self, uid: u32) -> Self {
        self.uid = uid;
        self
    }

    /// Answers takers, one at a time, until one has taken the mount; then the socket file is gone
    /// and this returns. A taker that does not complete leaves the instance as it was.
    ///
    /// # Errors
    /// [`HandoverError::Stuck`]: a hand-over that did not complete left the instance unable to
    /// serve again, so that it must end and be started again from the descriptor store.
    pub(super) async fn serve(mut self, running: Arc<Running>) -> Result<(), HandoverError> {
        loop {
            let accepted = match &self.listener {
                Some(listener) => listener.accept().await,
                None => std::future::pending().await,
            };
            let failed = accepted.is_err();
            let said = accepted.map_err(|why| {
                tracing::warn!("tierstore fs: the hand-over socket cannot accept: {why}");
            });
            pause_after(failed, ACCEPT_PAUSE).await;
            if let Ok((stream, _)) = said
                && self.session(&stream, &running).await?
            {
                // The path is the successor's now: the drop must not remove it.
                self.listener = None;
                return Ok(());
            }
        }
    }

    /// One taker's conversation: whether it took the mount.
    async fn session(
        &mut self,
        stream: &UnixStream,
        running: &Arc<Running>,
    ) -> Result<bool, HandoverError> {
        let mut claimed = false;
        let negotiating = self.negotiate(stream, running, &mut claimed);
        let negotiated = tokio::time::timeout(self.wait, negotiating).await;
        let bye = match negotiated {
            Ok(Ok(Some(bye))) => bye,
            other => {
                match other {
                    Ok(Err(why)) => {
                        tracing::warn!("tierstore fs: a hand-over request failed: {why}")
                    }
                    Err(_) => {
                        tracing::warn!(
                            "tierstore fs: a hand-over request was not completed in time"
                        );
                    }
                    Ok(_) => {}
                }
                if claimed {
                    self.claim.release();
                }
                return Ok(false);
            }
        };
        // The taker holds the descriptor in the service manager's store, and has said so.
        let quiescing = Arc::clone(running);
        tokio::task::spawn_blocking(move || quiescing.quiesce())
            .await
            .map_err(std::io::Error::other)
            .map_err(HandoverError::Stuck)?;
        // The successor binds the path once it serves; the file is gone first. A file that is not
        // there is no fault.
        let _ = std::fs::remove_file(&self.path);
        let confirmed = async {
            wire::send(stream, &["done".to_owned()], &[]).await?;
            wire::recv(stream).await
        };
        let ready = tokio::time::timeout(self.wait, confirmed).await;
        if matches!(ready, Ok(Ok(Frame { lines, .. })) if lines == [READY]) {
            // The mount is the successor's for good: this process never reads the device again,
            // and its unit, started again, must not adopt it from the store.
            running.give_up();
            if let Err(why) = self.manager.unstore(FUSE_NAME) {
                tracing::warn!("tierstore fs: the descriptor's store entry was not removed: {why}");
            }
            self.claim.give_up();
            if bye && let Err(why) = wire::send(stream, &[BYE.to_owned()], &[]).await {
                tracing::warn!("tierstore fs: bye was not sent: {why}");
            }
            return Ok(true);
        }
        tracing::warn!("tierstore fs: the taker did not say it serves the mount; serving again");
        self.resume(running).await?;
        self.claim.release();
        Ok(false)
    }

    /// Reads requests and accepts clients again, and makes the hand-over socket again.
    async fn resume(&mut self, running: &Arc<Running>) -> Result<(), HandoverError> {
        let again = Arc::clone(running);
        tokio::task::spawn_blocking(move || again.resume())
            .await
            .map_err(std::io::Error::other)
            .and_then(|resumed| resumed)
            .map_err(HandoverError::Stuck)?;
        self.listener = match socket(&self.path) {
            Ok(listener) => Some(listener),
            Err(why) => {
                tracing::warn!("tierstore fs: the hand-over socket is not made again: {why}");
                None
            }
        };
        Ok(())
    }

    /// Reads the request, offers the mount, and returns whether the taker answered `ack`; notes in
    /// `claimed` that the mount was claimed for this taker.
    async fn negotiate(
        &self,
        stream: &UnixStream,
        running: &Arc<Running>,
        claimed: &mut bool,
    ) -> std::io::Result<Option<bool>> {
        let request = wire::recv(stream).await?;
        let bye = match request.lines.as_slice() {
            [take] if take == TAKE => false,
            [take, ready] if take == TAKE && ready == READY => true,
            _ => return refuse(stream, NOT_UNDERSTOOD).await,
        };
        if stream.peer_cred()?.uid() != self.uid {
            return refuse(stream, "the taker is not the user of this instance").await;
        }
        match self.claim.claim() {
            Holding::Here => *claimed = true,
            Holding::Taking => return refuse(stream, BUSY).await,
            Holding::Gone => return refuse(stream, GONE).await,
        }
        let (device, control) = running.share()?;
        let lines = offer_lines(&self.paths.0, &self.paths.1);
        wire::send(stream, &lines, &[device.as_fd(), control.as_fd()]).await?;
        Ok((wire::recv(stream).await?.lines == ["ack"]).then_some(bye))
    }
}

/// Answers `refused` with `why`; the conversation is over and nothing was handed.
async fn refuse(stream: &UnixStream, why: &str) -> std::io::Result<Option<bool>> {
    wire::send(stream, &["refused".to_owned(), why.to_owned()], &[]).await?;
    Ok(None)
}

/// A socket at `path`, mode 0600; a file at the path is replaced.
fn socket(path: &Path) -> Result<UnixListener, HandoverError> {
    match std::fs::remove_file(path) {
        Err(why) if why.kind() != ErrorKind::NotFound => {
            return Err(HandoverError::io("removing the old hand-over socket")(why));
        }
        _ => {}
    }
    let listener =
        UnixListener::bind(path).map_err(HandoverError::io("binding the hand-over socket"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(HandoverError::io("restricting the hand-over socket"))?;
    Ok(listener)
}

impl Drop for Offer {
    /// An instance that ends without handing over removes its socket file, so that the next start
    /// finds none; one that handed over has removed it already, and one whose mount was given up on
    /// the one hand-over leaves the path to the successor, which binds it after its `bye`.
    fn drop(&mut self) {
        if self.listener.is_some() && self.claim.holding() != Holding::Gone {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[path = "handover_tests.rs"]
mod tests;

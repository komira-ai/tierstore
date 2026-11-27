//! The `fs` interface of the one process (`docs/design/one-service.md`, sections 2.3 and 3.5):
//! the mount a build node's `tierstore` serves itself when its file has an `[[interface]]` of type
//! `fs`, with a core that calls the node's cache and tree store (`in_process.rs`) instead of
//! asking another process. Linux only: the mount is a FUSE mount; macOS serves the same interface by
//! its NFS front (`nfs/interface.rs`).
//!
//! The start (`cache_cli.rs`, `cache_cli_mount.rs`) opens the cache and the tree store and then gets
//! the mount, before it binds or takes a socket of a front, in the order of `tierstore fs`
//! (`launch.rs`):
//!
//! 1. **Take** it, with the sockets and the cache lock, from a running instance of this layout in
//!    one conversation on `server.handoff_socket` (`take 1`, `ready`, `mount`; the one hand-over of
//!    `docs/design/one-service.md` section 3.1, [`Mounted`]):
//!    the device is put in the store before `ack`, read only after the instance's `bye`, and the
//!    cache takes the write role at its `written`. An instance that holds no mount (the S3 process
//!    of the two-process layout, or one of this layout whose mount a `take-fs 1` took) answers
//!    `refused`, and is asked for its sockets and lock alone; the mount is then taken from the
//!    instance that answers `take-fs 1` on `handoff.sock` in the runtime directory (`handover.rs`):
//!    the `tierstore fs` process of the two-process layout ([`Service::take`]). In that switch from
//!    two processes to one, the start checks the sockets' offer, holds it unanswered while it takes
//!    the mount, and accepts it once the mount's instance has said `done` and before that one is
//!    told `ready` (section 4.2): both offers are checked before either is accepted, a mount that
//!    is not taken refuses the sockets' offer, and sockets that cannot be taken leave the mount to
//!    its instance ([`Taking::abandon`]).
//! 2. Else **adopt** the descriptor the service manager kept under the name `fuse`.
//! 3. Else **mount afresh**: a mount of this file system that the kernel's table of mounts shows
//!    at the mount point has no server, and is aborted and detached, with no look at the mount
//!    point.
//!
//! A running instance that accepts on the socket and does not answer fails the start after
//! `params.handoff_wait`, before anything is detached or mounted. The mount is served, and the
//! instance it was taken from told `ready`, before the start binds a socket of a front; the start
//! then binds `handoff.sock` itself, and the service manager is told `READY=1` once the fronts serve
//! too.
//!
//! While it serves, the process offers its mount on both conversations: in the one hand-over on
//! `server.handoff_socket` ([`Served::offered`]), and on `take-fs 1` on `handoff.sock`, which a
//! start of the previous release and the `tierstore fs` of the two-process layout (a rollback of
//! the switch) ask. The two share a claim (`handoff/mount.rs`), so that one taker at a time takes
//! the mount; once it is given up on either, the process serves S3 and OCI alone until a successor
//! takes their sockets.
//!
//! A start that fails once the mount is served (the hand-over socket, or a socket of a front, cannot
//! be bound or taken) stops reading the connection and leaves the mount where it is, for the next
//! start: it never unmounts it, since the instance it was taken from may serve it again.
//!
//! A stop drains the fronts first and serves the mount throughout, since a fill a job waits for may
//! need the upstream; then the readers stop and the mount is left to the service manager's store
//! (a process with no manager unmounts, as `tierstore fs` does). A process whose mount a successor
//! took goes on serving S3 and OCI until the successor takes their sockets, and leaves its state of
//! the mount to the end of the process. A mount that cannot be served on (the connection a restart
//! adopted was aborted because requests stayed unanswered, or a hand-over that did not complete
//! left the readers stopped) is let go at once, as `tierstore fs` lets it go, since no request can
//! be answered on it; it stops the fronts as a signal does, and the process ends in failure, so
//! that the unit starts again.

use std::sync::{Arc, Mutex, PoisonError};

use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::server::handoff::{self, Claim, MountWant, Predecessor, TakenMount};
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::trees::TreeStore;

use super::config::FsSettings;
use super::fills::Calls;
use super::handover::{self, HandoverError, Offer};
use super::launch::{self, End, Platform, Started, ended, hang, leave_to_kernel};
use super::run::{Prepared, Running};
use super::sd::Manager;
use super::start::StartError;
use super::steps::Steps;

/// What the start of a build node asks of the platform: its service manager, and the `fs`
/// interface of its file, if any.
pub(in crate::server) struct Service {
    fs: Option<FsSettings>,
    platform: Platform,
}

impl Service {
    /// The service manager this process was started by, if any, and the `fs` interface `fs`.
    pub(in crate::server) fn of_process(fs: Option<FsSettings>) -> Self {
        Self {
            fs,
            platform: Platform::of_process(),
        }
    }

    /// The same, on `platform`.
    #[cfg(test)]
    pub(super) fn on(fs: Option<FsSettings>, platform: Platform) -> Self {
        Self { fs, platform }
    }

    /// Whether the file has an `fs` interface.
    pub(in crate::server) fn has_mount(&self) -> bool {
        self.fs.is_some()
    }

    /// The start of the `fs` interface, if the file has one, for the one hand-over
    /// ([`Mounted`]); else this, as it was.
    ///
    /// # Errors
    /// The file has no `fs` interface: the service, given back.
    pub(in crate::server) fn mounted(self) -> Result<Mounted, Box<Self>> {
        match self.fs {
            Some(settings) => Ok(Mounted {
                settings,
                platform: self.platform,
            }),
            None => Err(Box::new(self)),
        }
    }

    /// Gets the mount of the `fs` interface, if the file has one: taken, adopted or made (see the
    /// module), with a core that calls `cache` and `trees`; [`Taking::serve`] then serves it. A
    /// mount that was taken is not read yet: the instance it was taken from has said `done` and not
    /// yet `bye`, so a start that goes no further leaves it serving again ([`Taking::abandon`]).
    ///
    /// # Errors
    /// [`StartError`]: why the mount is not got; nothing was detached or mounted when a running
    /// instance refused, did not keep to the protocol or did not answer in time.
    pub(in crate::server) async fn take(
        self,
        (cache, trees): (LocalCache, Option<TreeStore>),
    ) -> Result<Taking, StartError> {
        let Self { fs, platform } = self;
        let manager = platform.manager.clone();
        let Some(settings) = fs else {
            return Ok(Taking {
                manager,
                mount: None,
                ready: None,
            });
        };
        let runtime = Handle::current();
        let reach = Calls {
            cache,
            trees,
            runtime: runtime.clone(),
        };
        let mine = settings.clone();
        // The start waits for the cache's answers (the attachments are made again through it),
        // which are worked out on the runtime: it runs on a thread of its own.
        let started = tokio::task::spawn_blocking(move || {
            let mut steps = Steps::new(Arc::clone(&platform.clock));
            let started = runtime.block_on(launch::start((&mine, &reach), &platform, &mut steps));
            tracing::info!(
                "tierstore fs: the start of the mount took {} ms",
                steps.total()
            );
            started
        })
        .await
        .unwrap_or_else(|panicked| std::panic::resume_unwind(panicked.into_panic()))?;
        Ok(Taking {
            manager,
            mount: Some((settings, started)),
            ready: None,
        })
    }
}

/// The start of a file's `fs` interface, as the one hand-over takes the mount
/// ([`Service::mounted`]).
pub(in crate::server) struct Mounted {
    settings: FsSettings,
    platform: Platform,
}

impl Mounted {
    /// What this start asks a running instance for of the mount in the one hand-over: its mount
    /// point and control socket, the wait of its quiesce (`params.handoff_wait`), the check of the
    /// device's kind, and, before `ack`, the check that the runtime state loads and the device put
    /// in the service manager's store, so that from `ack` on someone always holds the connection.
    pub(in crate::server) fn want(&self) -> MountWant<'_> {
        let settings = &self.settings;
        let manager = &self.platform.manager;
        MountWant {
            mountpoint: &settings.mountpoint,
            control: &settings.control_socket,
            wait: settings.handoff_wait,
            device: self.platform.kernel.device,
            prepare: Box::new(move |device| launch::prepare(settings, manager, device)),
        }
    }

    /// The service again, for a start that gets the mount otherwise ([`Service::take`]).
    pub(in crate::server) fn service(self) -> Service {
        Service {
            fs: Some(self.settings),
            platform: self.platform,
        }
    }

    /// Takes over the mount a running instance handed over in the one hand-over (`mount`, once it
    /// said `done`), with a core that calls `cache` and `trees`: the runtime state is read again
    /// and the connection made ready to serve, reading nothing of the device; [`Taking::serve`]
    /// says `ready` on `ready` and reads it after the `bye`. A take-over that fails drops `ready`:
    /// the running instance, which has not handed over, resumes and serves on.
    ///
    /// # Errors
    /// [`StartError`]: the runtime state or the connection cannot be made ready.
    pub(in crate::server) async fn take_handed(
        self,
        (cache, trees): (LocalCache, Option<TreeStore>),
        mount: TakenMount,
        ready: handoff::Ready,
    ) -> Result<Taking, StartError> {
        let Self { settings, platform } = self;
        let manager = platform.manager.clone();
        let runtime = Handle::current();
        let reach = Calls {
            cache,
            trees,
            runtime: runtime.clone(),
        };
        let mine = settings.clone();
        let TakenMount { device, control } = mount;
        // The take-over waits for the cache's answers (the attachments are made again through it),
        // which are worked out on the runtime: it runs on a thread of its own.
        let running = tokio::task::spawn_blocking(move || {
            let mut steps = Steps::new(Arc::clone(&platform.clock));
            let prepared = Prepared::load(&mine)?;
            let running = Running::take_over((&mine, &reach), prepared, device, control)?;
            let _ = steps.done("taking the mount over");
            Ok::<_, StartError>(running)
        })
        .await
        .unwrap_or_else(|panicked| std::panic::resume_unwind(panicked.into_panic()))?;
        Ok(Taking {
            manager,
            mount: Some((
                settings,
                Started {
                    running: Arc::new(running),
                    notice: None,
                    hung: None,
                },
            )),
            ready: Some(ready),
        })
    }
}

/// A mount got and not yet read where it was taken ([`Service::take`], [`Mounted::take_handed`]).
pub(in crate::server) struct Taking {
    manager: Manager,
    mount: Option<(FsSettings, Started)>,
    /// The connection of the one hand-over the mount was taken in, on which `ready` is said.
    ready: Option<handoff::Ready>,
}

impl Taking {
    /// Serves the mount [`Service::take`] or [`Mounted::take_handed`] got: a mount that was taken
    /// is read once the instance it came from has said `bye`, and the hand-over socket of the mount
    /// is bound. Taken in the one hand-over, the `ready` said there is this one, and the connection
    /// after its `bye` is the caller's ([`Served::predecessor`]): the cache takes the write role at
    /// that instance's `written`.
    ///
    /// # Errors
    /// The instance the mount was taken from gave up first and serves on, or the hand-over socket
    /// cannot be bound: the mount is left where it is ([`abandon`]).
    pub(in crate::server) async fn serve(self) -> Result<Served, StartError> {
        let Self {
            manager,
            mount,
            ready,
        } = self;
        let claim = Arc::new(Claim::default());
        let stuck = Arc::new(Stuck::default());
        let Some((settings, started)) = mount else {
            return Ok(Served {
                manager,
                mount: None,
                claim,
                stuck,
                predecessor: None,
            });
        };
        let Started {
            running,
            notice,
            hung,
        } = started;
        // A mount taken over is read only once the instance it came from has said `bye`, or
        // ended (a release before the step): it reads the device no more.
        let mut predecessor = None;
        let said = match (&notice, ready) {
            (Some(notice), _) => Some(notice.ready().await.map_err(StartError::from)),
            (None, Some(ready)) => Some(
                ready
                    .send_keeping()
                    .await
                    .map(|after| predecessor = Some(after))
                    .map_err(StartError::from),
            ),
            // Adopted or mounted afresh: no instance is told, and the readers run.
            (None, None) => None,
        };
        let told = match said {
            None => Ok(()),
            Some(Ok(())) => running
                .start_reading()
                .map_err(super::start::io("reading the connection taken over")),
            Some(Err(why)) => {
                // That instance gave up first and serves on, on its control socket's file.
                running.give_up();
                Err(why)
            }
        };
        let offer = told.and_then(|()| {
            Offer::bind(
                &settings.handoff_socket,
                &settings.mountpoint,
                &settings.control_socket,
                settings.handoff_wait,
            )
            .map(|offer| {
                offer
                    .with_manager(manager.clone())
                    .with_claim(Arc::clone(&claim))
            })
            .map_err(StartError::from)
        });
        let offer = match offer {
            Ok(offer) => offer,
            Err(why) => {
                abandon(running).await;
                return Err(why);
            }
        };
        Ok(Served {
            manager,
            mount: Some(Mount {
                settings,
                running,
                offer,
                hung,
            }),
            claim,
            stuck,
            predecessor,
        })
    }

    /// Leaves the mount as [`abandon`] does, without a `ready` to the instance it was taken from,
    /// which then serves it again: what a start does when it cannot go on before `ready` (the
    /// sockets of the other process of a switch could not be taken).
    pub(in crate::server) async fn abandon(self) {
        let Self { mount, ready, .. } = self;
        if let Some((_, started)) = mount {
            let Started {
                running, notice, ..
            } = started;
            // The control socket's file is that instance's: it serves on it again.
            running.give_up();
            // The connection to it ends last: until then it holds the mount quiesced.
            abandon(running).await;
            drop(notice);
        }
        drop(ready);
    }
}

/// Stops reading the connection of `running`, off the runtime's threads, and leaves the mount
/// where it is: to the service manager's store, which holds the descriptor, or, without a
/// manager, to the next start, which aborts and detaches a mount no one serves. It is never
/// unmounted: the instance it was taken from may serve it again (a taker gone before `ready`
/// leaves that instance serving). What a start does with a mount it served and cannot go on with.
async fn abandon(running: Arc<Running>) {
    let leaving = tokio::task::spawn_blocking(move || match Arc::try_unwrap(running) {
        Ok(alone) => alone.leave(),
        Err(_shared) => tracing::warn!("tierstore fs: the mount is still being handed over"),
    });
    leaving
        .await
        .unwrap_or_else(|panicked| std::panic::resume_unwind(panicked.into_panic()));
}

/// The mount a start serves.
struct Mount {
    settings: FsSettings,
    running: Arc<Running>,
    /// The answer to a successor that asks for the mount (`take-fs 1`).
    offer: Offer,
    /// Receives when the watch of a restart has aborted the connection.
    hung: Option<tokio::sync::oneshot::Receiver<()>>,
}

/// What a start of a build node got from the platform: the service manager, and the mount of the
/// `fs` interface, if the file has one.
pub(in crate::server) struct Served {
    manager: Manager,
    mount: Option<Mount>,
    /// Which of the two conversations that offer the mount has it.
    claim: Arc<Claim>,
    /// Told when a resume of the one hand-over cannot serve the mount again.
    stuck: Arc<Stuck>,
    /// The connection to the instance the mount was taken from in the one hand-over, after its
    /// `bye`, until [`Served::predecessor`] takes it.
    predecessor: Option<Predecessor>,
}

/// What a resume of the one hand-over that cannot serve the mount again tells [`Served::serve`],
/// which then stops the process as a `take-fs 1` that cannot serve again does.
#[derive(Default)]
struct Stuck {
    why: Mutex<Option<std::io::Error>>,
    told: Notify,
}

impl Stuck {
    /// Tells it `why`.
    fn tell(&self, why: &std::io::Error) {
        let copy = std::io::Error::new(why.kind(), why.to_string());
        *self.why.lock().unwrap_or_else(PoisonError::into_inner) = Some(copy);
        self.told.notify_one();
    }

    /// Waits until it is told, and gives what it was told.
    async fn told(&self) -> std::io::Error {
        self.told.notified().await;
        let why = self
            .why
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        why.unwrap_or_else(|| std::io::Error::other("the mount cannot be served again"))
    }
}

impl Served {
    /// Tells the service manager that the unit is ready (`READY=1`); a manager that cannot be told
    /// is said, and the process goes on.
    pub(in crate::server) fn ready(&self) {
        if let Err(why) = self.manager.ready() {
            tracing::warn!("tierstore: cannot tell the service manager it is ready: {why}");
        }
    }

    /// The connection to the instance the mount was taken from in the one hand-over, after its
    /// `bye`: the cache takes the write role with the lock it handed over at that instance's
    /// `written` (`handoff::follow`), and the connection is held until this process ends. None for
    /// a mount taken on `take-fs 1`, adopted or made, and once taken.
    pub(in crate::server) fn predecessor(&mut self) -> Option<Predecessor> {
        self.predecessor.take()
    }

    /// The mount as the one hand-over offers it on `server.handoff_socket` (`Offer::with_mount`),
    /// sharing the claim of the `take-fs 1` offer [`Served::serve`] answers; none without a mount.
    pub(in crate::server) fn offered(&self) -> Option<handoff::Mount> {
        let mount = self.mount.as_ref()?;
        let stuck = Arc::clone(&self.stuck);
        let told: handover::Stuck = Arc::new(move |why| stuck.tell(why));
        Some(handover::offered(
            &mount.running,
            &mount.settings,
            self.manager.clone(),
            (told, Arc::clone(&self.claim)),
        ))
    }

    /// Leaves the mount, as [`abandon`] does, when the start fails after it was served (a socket of
    /// a front that cannot be bound or taken); nothing without a mount. The connection to the
    /// instance it was taken from in the one hand-over ends: that instance, the cache lock's other
    /// holder, unlocks it.
    pub(in crate::server) async fn abandon(self) {
        if let Some(mount) = self.mount {
            drop(mount.offer);
            abandon(mount.running).await;
        }
        drop(self.predecessor);
    }

    /// Serves the mount until `drained` is notified, when the fronts have drained, and then leaves
    /// it (section 3.5); meanwhile answers a successor that asks for it on `take-fs 1`, and ends as
    /// handed over once a successor has it, on that conversation or on the one hand-over
    /// ([`Served::offered`]). `failed` is notified when the mount cannot be served on, so that the
    /// fronts stop as at a signal. Returns at once without a mount.
    ///
    /// # Errors
    /// [`StartError::Aborted`] for a connection aborted because requests stayed unanswered;
    /// [`StartError::Handover`] for a hand-over that left the readers stopped; what the system said
    /// when the mount could not be unmounted.
    pub(in crate::server) async fn serve(
        self,
        drained: Arc<Notify>,
        failed: Arc<Notify>,
    ) -> Result<(), StartError> {
        let Self {
            manager,
            mount,
            claim,
            stuck,
            predecessor,
        } = self;
        // A connection the caller did not take is held while the mount is served: its end tells
        // the instance the mount came from that this one is gone.
        let _held = predecessor;
        let Some(Mount {
            running,
            offer,
            hung,
            ..
        }) = mount
        else {
            return Ok(());
        };
        let end = tokio::select! {
            () = claim.given_up() => End::Handed,
            why = stuck.told() => {
                failed.notify_one();
                leave_to_kernel(running);
                return Err(HandoverError::Stuck(why).into());
            }
            handed = offer.serve(Arc::clone(&running)) => match handed {
                Ok(()) => End::Handed,
                Err(why) => {
                    // The store keeps the descriptor for the next start, as at the end of a
                    // `tierstore fs` process that could not serve again.
                    failed.notify_one();
                    leave_to_kernel(running);
                    return Err(why.into());
                }
            },
            () = hang(hung) => {
                failed.notify_one();
                End::Hung
            }
            () = drained.notified() => End::Signal,
        };
        if matches!(end, End::Handed) {
            tracing::info!(
                "tierstore fs: a successor serves the mount; S3 and OCI are served until it takes them"
            );
        }
        let leaving = tokio::task::spawn_blocking(move || ended(end, running, &manager));
        leaving
            .await
            .unwrap_or_else(|panicked| std::panic::resume_unwind(panicked.into_panic()))
    }
}

#[cfg(test)]
#[path = "interface_tests.rs"]
mod tests;

//! What a start tells the node's tierstore over the fill socket: the trees this mount has attached,
//! so that the cache drops every other pin of the mount (`pins-live`), and the placements, so that
//! the tree store ends every other hold of it (`places-live`). Together they are the announcement.
//!
//! The announcement is not part of the start. After a boot the tierstore of the node reads its
//! whole cache before it binds the fill socket, minutes for a cache of hundreds of thousands of
//! objects, and a start that waited for the socket would not be ready within the service
//! manager's start timeout. So a start marks the announcement owed ([`Core::tell_live_later`]) and
//! a thread of its own sends it once the socket answers, asking again until it does.
//!
//! Until then every request that changes what the node holds for this mount (`pin`, `unpin`,
//! `place`, `unplace`) waits for the announcement first ([`Core::settled`]): a pin or a hold made
//! after the start is never undone by an announcement that comes after it. One announcement is
//! sent at a time, with no lock held across its round trips: a request that finds one being sent
//! waits for it and takes its outcome, and one that finds it owed and none being sent sends it.
//! So a request that finds the socket away waits for one attempt, at most `fs.fetch_timeout` of
//! pauses, and fails as it would have without the announcement. The announcement says what is
//! attached when it is sent, not when the start was made.
//!
//! A process whose successor is taking over (`Core::hold`) does not send the announcement: the
//! successor sends its own, and the hold waits for one that is being sent. The thread rests while
//! the hold is on, so a hand-over that does not complete leaves the announcement to it again.

use std::sync::{Arc, Condvar, Mutex, Weak};

use crate::TreeDigest;

use super::super::fills::AskError;
use super::super::locked;
use super::Core;
use super::placed::placement_of;

/// Where the announcement is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Told, or never owed (a core no start made).
    Told,
    /// Owed, and the last attempt, if any, failed so.
    Owed(Option<AskError>),
    /// A thread is sending it.
    Sending,
}

/// The announcement of a core, and what wakes those that wait for one being sent.
#[derive(Debug)]
pub(super) struct Announcement {
    stage: Mutex<Stage>,
    changed: Condvar,
}

impl Default for Announcement {
    fn default() -> Self {
        Self {
            stage: Mutex::new(Stage::Told),
            changed: Condvar::new(),
        }
    }
}

impl Announcement {
    /// Waits until no announcement is being sent.
    pub(super) fn wait_sent(&self) {
        let stage = locked(&self.stage);
        drop(
            self.changed
                .wait_while(stage, |stage| *stage == Stage::Sending)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

impl Core {
    /// Tells the fill socket the trees and the placements attached now (see the module), and
    /// waits for both answers.
    ///
    /// # Errors
    /// [`AskError`]: the socket did not take a request, or did not answer one, in
    /// `fs.fetch_timeout`.
    pub(in crate::server::treefs) fn tell_live(&self) -> Result<(), AskError> {
        let attached = self.state().attachments.trees();
        let trees: Vec<String> = attached
            .iter()
            .filter(|digest| placement_of(digest).is_none())
            .map(TreeDigest::to_string)
            .collect();
        let _ = self.fills.pins_live(&self.settings.mount_id, &trees)?;
        self.places_live(&attached)
    }

    /// Returns once the announcement is told: at once if it is, after the attempt being sent if
    /// one is, else after sending it. A request that changes what the node holds for this mount
    /// calls this first.
    ///
    /// # Errors
    /// [`AskError::Unavailable`] while a successor is taking over, else what the attempt this
    /// waited for or made failed with: the announcement is still owed.
    pub(in crate::server::treefs) fn settled(&self) -> Result<(), AskError> {
        let live = &self.live;
        let mut stage = locked(&live.stage);
        if *stage == Stage::Sending {
            stage = live
                .changed
                .wait_while(stage, |stage| *stage == Stage::Sending)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // The attempt waited for is the answer: a failed one is not made again at once.
            if let Stage::Owed(why) = *stage {
                return Err(why.unwrap_or(AskError::Unavailable));
            }
            return Ok(());
        }
        if *stage == Stage::Told {
            return Ok(());
        }
        // The hold is stored before it waits for a sending to end (`Core::hold`): a sending
        // begun after this look is never one the successor's announcement could precede.
        if self.is_held() {
            return Err(AskError::Unavailable);
        }
        *stage = Stage::Sending;
        drop(stage);
        let told = self.tell_live();
        *locked(&live.stage) = match told {
            Ok(()) => Stage::Told,
            Err(why) => Stage::Owed(Some(why)),
        };
        live.changed.notify_all();
        if told.is_ok() {
            tracing::info!("tierstore fs: the fill socket has the mount's pins and placements");
        }
        told
    }

    /// Marks the announcement owed and starts the thread that sends it. The thread asks until the
    /// socket answers, resting between attempts and while a successor is taking over, and ends
    /// once it is told or the core is dropped.
    ///
    /// # Errors
    /// What the system said when the thread could not be started.
    pub(in crate::server::treefs) fn tell_live_later(self: &Arc<Self>) -> std::io::Result<()> {
        *locked(&self.live.stage) = Stage::Owed(None);
        let core = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("fs-live".to_owned())
            .spawn(move || announce(&core))?;
        Ok(())
    }
}

/// Sends the announcement of `core` until it is told (see [`Core::tell_live_later`]).
fn announce(core: &Weak<Core>) {
    while let Some(core) = core.upgrade() {
        match core.settled() {
            Ok(()) => return,
            Err(_) if core.is_held() => core.fills.rest(),
            Err(why) => {
                tracing::warn!(
                    "tierstore fs: the fill socket does not have the mount's pins yet ({why}); \
                     asking again"
                );
                core.fills.rest();
            }
        }
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;

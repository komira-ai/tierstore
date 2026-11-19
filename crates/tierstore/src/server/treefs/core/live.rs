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
//! `place`, `unplace`) waits for the announcement first, and sends it itself if it is still owed
//! ([`Core::settled`]): a pin or a hold made after the start is never undone by an announcement
//! that comes after it. A request that finds the socket away for the whole of `fs.fetch_timeout`
//! fails as it would have without the announcement. The announcement says what is attached when it
//! is sent, not when the start was made.
//!
//! A process whose successor is taking over (`Core::hold`) does not send the announcement: the
//! successor sends its own, and the hold waits for one that is being sent.

use std::sync::{Arc, Weak};

use crate::TreeDigest;

use super::super::fill_client::AskError;
use super::super::locked;
use super::Core;
use super::placed::placement_of;

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

    /// Sends the announcement if it is still owed; a request that changes what the node holds for
    /// this mount calls this first.
    ///
    /// # Errors
    /// [`AskError::Unavailable`] while a successor is taking over, else what
    /// [`Core::tell_live`] failed with: the announcement is still owed.
    pub(in crate::server::treefs) fn settled(&self) -> Result<(), AskError> {
        let mut owed = locked(&self.owed);
        if *owed {
            if self.is_held() {
                return Err(AskError::Unavailable);
            }
            self.tell_live()?;
            *owed = false;
            tracing::info!("tierstore fs: the fill socket has the mount's pins and placements");
        }
        Ok(())
    }

    /// Marks the announcement owed and starts the thread that sends it. The thread asks until the
    /// socket answers, and ends then, or when a successor is taking over or the core is dropped.
    ///
    /// # Errors
    /// What the system said when the thread could not be started.
    pub(in crate::server::treefs) fn tell_live_later(self: &Arc<Self>) -> std::io::Result<()> {
        *locked(&self.owed) = true;
        let core = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("fs-live".to_owned())
            .spawn(move || announce(&core))?;
        Ok(())
    }
}

/// Sends the announcement of `core` until it is answered (see [`Core::tell_live_later`]).
fn announce(core: &Weak<Core>) {
    while let Some(core) = core.upgrade() {
        match core.settled() {
            Ok(()) => return,
            Err(_) if core.is_held() => return,
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

//! [`ClientBudget`]: the controller behind a lock, the wake-up of waiting charges, and the
//! injected timer, as the frozen [`Budget`].

use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::future::select;
use tierstore_fetch::timer::Timer;
use tierstore_fetch_api::budget::{Budget, Charge, Observation, Permit};
use tierstore_fetch_api::ids::Class;
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals};
use tokio::sync::Notify;

use crate::config::{BudgetConfig, ConfigError};
use crate::controller::{Blocked, Controller, Waiting};
use crate::quantities::Quantities;
use crate::snapshot::Snapshot;

struct Shared {
    controller: Mutex<Controller>,
    /// Woken whenever permits return or a limit moves.
    changed: Notify,
    timer: Arc<dyn Timer>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Controller> {
        self.controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> u64 {
        self.timer.now_ms()
    }
}

/// The budget of a build node (4.3).
///
/// Every time it reads comes from the injected timer: the hint's age, the windows, the
/// cooldowns and the pacing gap. Nothing it does depends on the machine's clock.
#[derive(Clone)]
pub struct ClientBudget {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for ClientBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBudget").finish_non_exhaustive()
    }
}

/// A waiting charge's registration; dropping it (a cancelled acquire) removes it.
struct Registration<'a> {
    shared: &'a Shared,
    charge: &'a Charge,
    now: Waiting,
}

impl Registration<'_> {
    fn set(&mut self, c: &mut Controller, to: Waiting) {
        c.rewait(self.charge, self.now, to);
        self.now = to;
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        if self.now != Waiting::default() {
            let mut c = self.shared.lock();
            c.rewait(self.charge, self.now, Waiting::default());
            drop(c);
            self.shared.changed.notify_waiters();
        }
    }
}

impl ClientBudget {
    /// A budget for `config`, reading time from `timer`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when `config` does not validate.
    pub fn new(config: BudgetConfig, timer: Arc<dyn Timer>) -> Result<Self, ConfigError> {
        config.validate()?;
        let controller = Controller::new(config, timer.now_ms());
        Ok(Self {
            shared: Arc::new(Shared {
                controller: Mutex::new(controller),
                changed: Notify::new(),
                timer,
            }),
        })
    }

    /// The quantities of 4.3 now: at the active clients (the jobscheduler's hint while fresh,
    /// else `N`) and the current `P` of the largest size class.
    #[must_use]
    pub fn quantities(&self) -> Quantities {
        self.shared.lock().quantities(self.shared.now())
    }

    /// The controller's state now.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().snapshot(self.shared.now())
    }

    /// Feeds the load a `LOCATE` answer gave for `device` (10.6: the answering node's view of
    /// the device's queue wait): a queue-wait sample of that device. A device the client has no
    /// traffic with keeps no state, and a `stalled` or `indexing` device is not a load signal.
    pub fn observe_locate(&self, device: DeviceRef, signals: &DriveSignals) {
        self.shared.lock().locate(device, signals);
    }
}

#[async_trait::async_trait]
impl Budget for ClientBudget {
    async fn acquire(&self, charge: Charge) -> Permit {
        let shared = &*self.shared;
        let mut reg = Registration {
            shared,
            charge: &charge,
            now: Waiting::default(),
        };
        loop {
            let mut changed = pin!(shared.changed.notified());
            changed.as_mut().enable();
            let until = {
                let mut c = shared.lock();
                match c.try_take(&charge, shared.now()) {
                    Ok(()) => {
                        reg.set(&mut c, Waiting::default());
                        break;
                    }
                    Err(blocked) => {
                        if charge.class == Class::Demand {
                            let to = Waiting {
                                devices: true,
                                ceiling: blocked == Blocked::Ceiling,
                            };
                            reg.set(&mut c, to);
                        }
                        match blocked {
                            Blocked::Device { until } => until,
                            Blocked::Ceiling | Blocked::Node => None,
                        }
                    }
                }
            };
            match until {
                Some(t) => {
                    let _ = select(changed, shared.timer.sleep_until(t)).await;
                }
                None => changed.await,
            }
        }
        drop(reg);
        let owner = self.shared.clone();
        Permit::new(move || {
            owner.lock().release(&charge);
            owner.changed.notify_waiters();
        })
    }

    fn observe(&self, observation: Observation) {
        let now = self.shared.now();
        self.shared.lock().observe(&observation, now);
        self.shared.changed.notify_waiters();
    }

    fn part_size(&self, pack_size: u64) -> u64 {
        self.shared.lock().part_size(pack_size)
    }
}

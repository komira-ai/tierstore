//! The second unit of the fake machine, `tierstore-fs@a` and `tierstore-fs@b`: its instances, its
//! start markers and the hand-over of the mount between them, in the world of the parent module.
//!
//! `start(x)` models systemd and `tierstore fs`: without the marker the unit file's
//! `ConditionPathExists=` names the unit stays inactive; otherwise it runs the release whose binary
//! the hook put at [`SERVICE_BINARY`]. When the other instance holds the mount the new one takes
//! it and is active (a notify unit is active once it said `READY=1`, which it says after the
//! take), and the old one is deactivating for a chosen number of `state` looks and then inactive,
//! by itself; with no holder it makes the mount afresh and is active. [`FsTake`] makes the take
//! fail. Every call is counted and logged with the prefix `fs_units.` or `fs_markers.`, so the
//! exact lists of `actions()` of the first unit are not changed by it; `fs_actions()` lists
//! these.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

use crate::server::update_hook::{
    Family, HANDOFF_FS_CAPABILITY, Instance, MARKER_DIR, Markers, PortError, PortKind, Record,
    UnitState, Units,
};

use super::{NEW, NEWER, OLD, SystemdFake, Unit, World, count_in};

/// The unit file the package ships for `tierstore fs`.
const TEMPLATE: &str =
    include_str!("../../../../../../packaging/hostkeeper/units/tierstore-fs@.service");

/// The start marker of the fs instance `instance`: the path `tierstore-fs@.service`'s
/// `ConditionPathExists=` names with `%i`.
pub fn fs_marker_path(instance: Instance) -> PathBuf {
    PathBuf::from(MARKER_DIR).join(Family::Fs.marker_file(instance))
}

/// The path the fs unit file's `ConditionPathExists=` names, with `%i` as the file spells it.
pub fn fs_unit_condition() -> &'static str {
    TEMPLATE
        .lines()
        .find_map(|line| line.strip_prefix("ConditionPathExists="))
        .expect("the fs unit file has a ConditionPathExists")
}

/// The program of the fs unit file's `ExecStart=`.
pub fn fs_unit_program() -> &'static str {
    TEMPLATE
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))
        .and_then(|command| command.split_whitespace().next())
        .expect("the fs unit file has an ExecStart")
}

/// What a new fs instance does when it asks the running one for the mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsTake {
    /// It takes the mount and says `READY=1`.
    Works,
    /// It cannot take it: the start fails, the old instance resumes, and the unit waits to be
    /// started again (`Restart=on-failure`): `activating`.
    Refused,
    /// The start returns, but the unit is `failed`: the old instance resumed.
    Dud,
}

/// The state of the second unit.
pub struct FsWorld {
    units: BTreeMap<Instance, Unit>,
    markers: BTreeSet<PathBuf>,
    /// The `state` looks that still see the old instance running after it gave the mount away.
    drain_looks: u32,
    take: FsTake,
}

impl FsWorld {
    pub fn new() -> FsWorld {
        FsWorld {
            units: Instance::ALL
                .into_iter()
                .map(|i| (i, Unit::off()))
                .collect(),
            markers: BTreeSet::new(),
            drain_looks: 0,
            take: FsTake::Works,
        }
    }
}

impl World {
    fn fs_condition_path(instance: Instance) -> PathBuf {
        PathBuf::from(fs_unit_condition().replace("%i", instance.letter()))
    }

    /// systemd starting the fs instance `instance`.
    pub(super) fn start_fs(&mut self, instance: Instance) -> Result<(), PortError> {
        if !self
            .fs
            .markers
            .contains(&World::fs_condition_path(instance))
        {
            self.log.push(format!("skipped fs {instance}: no marker"));
            return Ok(());
        }
        if self.fs.units[&instance].state == UnitState::Active {
            return Ok(());
        }
        let release = self.installed.clone();
        let holder = Instance::ALL
            .into_iter()
            .find(|other| *other != instance && self.fs.units[other].state == UnitState::Active);
        let started = Unit {
            state: UnitState::Active,
            release,
            serving: true,
            left: 0,
        };
        let Some(old) = holder else {
            self.fs.units.insert(instance, started);
            return Ok(());
        };
        match self.fs.take {
            FsTake::Works => {
                let old_release = self.fs.units[&old].release.clone();
                self.fs.units.insert(instance, started);
                self.fs.units.insert(
                    old,
                    Unit {
                        state: UnitState::Deactivating,
                        release: old_release,
                        serving: false,
                        left: self.fs.drain_looks,
                    },
                );
                Ok(())
            }
            FsTake::Refused => {
                self.fs.units.insert(
                    instance,
                    Unit {
                        state: UnitState::Activating,
                        ..Unit::off()
                    },
                );
                Err(PortError::failed(
                    PortKind::Units,
                    "the unit did not become active",
                ))
            }
            FsTake::Dud => {
                self.fs.units.insert(
                    instance,
                    Unit {
                        state: UnitState::Failed,
                        ..Unit::off()
                    },
                );
                Ok(())
            }
        }
    }

    pub(super) fn reboot_fs(&mut self) {
        for instance in Instance::ALL {
            self.fs.units.insert(instance, Unit::off());
        }
        for instance in Instance::ALL {
            // A start that fails at boot leaves the unit as the failure did.
            let _ = self.start_fs(instance);
        }
    }

    pub(super) fn fs_snapshot(&self) -> Vec<(Instance, UnitState, Option<PathBuf>)> {
        Instance::ALL
            .into_iter()
            .map(|i| {
                (
                    i,
                    self.fs.units[&i].state,
                    self.fs.units[&i].release.clone(),
                )
            })
            .collect()
    }

    pub(super) fn fs_marker_set(&self) -> Vec<Instance> {
        Instance::ALL
            .into_iter()
            .filter(|i| self.fs.markers.contains(&fs_marker_path(*i)))
            .collect()
    }
}

/// The ports of the second unit, over the world of the first.
pub struct FsPorts {
    w: Rc<RefCell<World>>,
}

impl FsPorts {
    pub(super) fn new(w: Rc<RefCell<World>>) -> FsPorts {
        FsPorts { w }
    }
}

impl Units for FsPorts {
    fn start(&self, instance: Instance) -> Result<(), PortError> {
        let late = count_in(
            &self.w,
            format!("fs_units.start {instance}"),
            PortKind::Units,
        )?;
        self.w.borrow_mut().start_fs(instance)?;
        SystemdFake::after(late, PortKind::Units)
    }

    fn stop(&self, instance: Instance) -> Result<(), PortError> {
        let late = count_in(
            &self.w,
            format!("fs_units.stop {instance}"),
            PortKind::Units,
        )?;
        self.w.borrow_mut().fs.units.insert(instance, Unit::off());
        SystemdFake::after(late, PortKind::Units)
    }

    fn state(&self, instance: Instance) -> Result<UnitState, PortError> {
        count_in(
            &self.w,
            format!("fs_units.state {instance}"),
            PortKind::Units,
        )?;
        let mut w = self.w.borrow_mut();
        let unit = w.fs.units.get_mut(&instance).expect("both instances");
        if unit.state == UnitState::Deactivating {
            if unit.left == 0 {
                *unit = Unit::off();
            } else {
                unit.left -= 1;
            }
        }
        Ok(unit.state)
    }
}

impl Markers for FsPorts {
    fn present(&self, instance: Instance) -> Result<bool, PortError> {
        Ok(self
            .w
            .borrow()
            .fs
            .markers
            .contains(&fs_marker_path(instance)))
    }

    fn set(&self, instance: Instance) -> Result<(), PortError> {
        let late = count_in(
            &self.w,
            format!("fs_markers.set {instance}"),
            PortKind::Markers,
        )?;
        self.w
            .borrow_mut()
            .fs
            .markers
            .insert(fs_marker_path(instance));
        SystemdFake::after(late, PortKind::Markers)
    }

    fn clear(&self, instance: Instance) -> Result<(), PortError> {
        let late = count_in(
            &self.w,
            format!("fs_markers.clear {instance}"),
            PortKind::Markers,
        )?;
        self.w
            .borrow_mut()
            .fs
            .markers
            .remove(&fs_marker_path(instance));
        SystemdFake::after(late, PortKind::Markers)
    }
}

impl SystemdFake {
    /// The ports of the second unit, both its units and its markers.
    pub fn fs_ports(&self) -> &FsPorts {
        &self.fs
    }

    /// A host that runs `OLD` on instance a with `tierstore fs` running too, on instance a with
    /// its marker and the record saying so. Every release lists `handoff-fs-v1`.
    pub fn fs_installed() -> SystemdFake {
        let fake = SystemdFake::installed();
        for release in [OLD, NEW, NEWER] {
            fake.list_fs_hand_over(release, true);
        }
        {
            let mut w = fake.w.borrow_mut();
            w.fs.markers.insert(World::fs_condition_path(Instance::A));
            w.start_fs(Instance::A).expect("a fresh start");
            let mut record =
                Record::decode(w.record.as_deref().expect("a record")).expect("a record");
            record.fs_live = Some(Instance::A);
            w.record = Some(record.encode());
            w.log.clear();
        }
        fake
    }

    /// Whether `release` lists `handoff-fs-v1`.
    pub fn list_fs_hand_over(&self, release: &str, listed: bool) {
        let mut w = self.w.borrow_mut();
        let list = w
            .capabilities
            .get_mut(&PathBuf::from(release))
            .expect("a release of this host");
        list.retain(|word| word != HANDOFF_FS_CAPABILITY);
        if listed {
            list.push(HANDOFF_FS_CAPABILITY.to_owned());
        }
    }

    /// What a new fs instance does when it asks for the mount.
    pub fn set_fs_take(&self, take: FsTake) {
        self.w.borrow_mut().fs.take = take;
    }

    /// How many `state` looks a fs instance that gave the mount away is still seen running.
    pub fn set_fs_drain_looks(&self, looks: u32) {
        self.w.borrow_mut().fs.drain_looks = looks;
    }

    /// Puts the fs instance `instance` in a state, running `release`.
    pub fn set_fs_unit(&self, instance: Instance, state: UnitState, release: Option<&str>) {
        self.w.borrow_mut().fs.units.insert(
            instance,
            Unit {
                state,
                release: release.map(PathBuf::from),
                serving: state == UnitState::Active,
                left: 0,
            },
        );
    }

    /// Puts the fs instance `instance` in `deactivating`, running `release`, for `looks` looks of
    /// `state` that still see it so.
    pub fn set_fs_deactivating(&self, instance: Instance, release: &str, looks: u32) {
        self.w.borrow_mut().fs.units.insert(
            instance,
            Unit {
                state: UnitState::Deactivating,
                release: Some(PathBuf::from(release)),
                serving: false,
                left: looks,
            },
        );
    }

    /// The state of the fs instance `instance`, without a look that counts.
    pub fn peek_fs(&self, instance: Instance) -> UnitState {
        self.w.borrow().fs.units[&instance].state
    }

    /// The release the fs instance `instance` runs.
    pub fn fs_release(&self, instance: Instance) -> Option<PathBuf> {
        self.w.borrow().fs.units[&instance].release.clone()
    }

    /// The fs instances whose process may run.
    pub fn fs_running(&self) -> Vec<Instance> {
        let w = self.w.borrow();
        Instance::ALL
            .into_iter()
            .filter(|i| w.fs.units[i].state.running())
            .collect()
    }

    /// The fs instances that have their marker.
    pub fn fs_markers(&self) -> Vec<Instance> {
        Instance::ALL
            .into_iter()
            .filter(|i| Markers::present(&self.fs, *i).expect("a look at a marker"))
            .collect()
    }

    /// The calls that change an fs unit or marker, in order.
    pub fn fs_actions(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|line| {
                [
                    "fs_units.start",
                    "fs_units.stop",
                    "fs_markers.set",
                    "fs_markers.clear",
                ]
                .iter()
                .any(|prefix| line.starts_with(prefix))
            })
            .collect()
    }

    /// Every call that looks at an fs unit.
    pub fn fs_looks(&self) -> usize {
        self.log()
            .iter()
            .filter(|line| line.starts_with("fs_units.state"))
            .count()
    }
}

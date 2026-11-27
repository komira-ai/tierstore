//! The second unit of the earlier layout in the fake machine, `tierstore-fs@a` and
//! `tierstore-fs@b`: the state of its units and the presence of its start markers, which the hook
//! only looks at. A call that would change either (start, stop, set a marker, clear one) is
//! logged with the prefix `fs_units.` or `fs_markers.` and changes nothing; `fs_actions()` lists
//! those calls, which a step of this release never makes. A look at a unit is counted and logged
//! as `fs_units.state`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::server::update_hook::{Instance, Markers, PortError, PortKind, UnitState, Units};

use super::{SystemdFake, World, count_in};

/// The state of the second unit.
pub struct FsWorld {
    units: BTreeMap<Instance, UnitState>,
    markers: BTreeSet<Instance>,
}

impl FsWorld {
    pub fn new() -> FsWorld {
        FsWorld {
            units: Instance::ALL
                .into_iter()
                .map(|i| (i, UnitState::Inactive))
                .collect(),
            markers: BTreeSet::new(),
        }
    }
}

impl World {
    pub(super) fn fs_snapshot(&self) -> Vec<(Instance, UnitState)> {
        self.fs
            .units
            .iter()
            .map(|(i, state)| (*i, *state))
            .collect()
    }

    pub(super) fn fs_marker_set(&self) -> Vec<Instance> {
        self.fs.markers.iter().copied().collect()
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

    /// A call that would change something: logged, and nothing changes.
    fn act(&self, what: String) -> Result<(), PortError> {
        count_in(&self.w, what, PortKind::Units).map(|_| ())
    }
}

impl Units for FsPorts {
    fn start(&self, instance: Instance) -> Result<(), PortError> {
        self.act(format!("fs_units.start {instance}"))
    }

    fn stop(&self, instance: Instance) -> Result<(), PortError> {
        self.act(format!("fs_units.stop {instance}"))
    }

    fn state(&self, instance: Instance) -> Result<UnitState, PortError> {
        count_in(
            &self.w,
            format!("fs_units.state {instance}"),
            PortKind::Units,
        )?;
        Ok(self.w.borrow().fs.units[&instance])
    }
}

impl Markers for FsPorts {
    fn present(&self, instance: Instance) -> Result<bool, PortError> {
        Ok(self.w.borrow().fs.markers.contains(&instance))
    }

    fn set(&self, instance: Instance) -> Result<(), PortError> {
        self.act(format!("fs_markers.set {instance}"))
    }

    fn clear(&self, instance: Instance) -> Result<(), PortError> {
        self.act(format!("fs_markers.clear {instance}"))
    }
}

impl SystemdFake {
    /// The ports of the second unit, both its units and its markers.
    pub fn fs_ports(&self) -> &FsPorts {
        &self.fs
    }

    /// A host that runs `OLD` on instance a, with the second unit of the earlier layout running
    /// too, on instance a, with its marker and the record saying so.
    pub fn fs_installed() -> SystemdFake {
        let fake = SystemdFake::installed();
        fake.set_fs_unit(Instance::A, UnitState::Active);
        fake.set_fs_marker(Instance::A, true);
        fake.set_fs_live(Some(Instance::A));
        fake
    }

    /// Puts the fs instance `instance` in a state.
    pub fn set_fs_unit(&self, instance: Instance, state: UnitState) {
        self.w.borrow_mut().fs.units.insert(instance, state);
    }

    /// Lays down or removes the start marker of the fs instance `instance`.
    pub fn set_fs_marker(&self, instance: Instance, present: bool) {
        let mut w = self.w.borrow_mut();
        if present {
            w.fs.markers.insert(instance);
        } else {
            w.fs.markers.remove(&instance);
        }
    }

    /// Sets `fs_live` of the record on disk.
    pub fn set_fs_live(&self, live: Option<Instance>) {
        let mut record = self.record();
        record.fs_live = live;
        self.set_record(&record);
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
}

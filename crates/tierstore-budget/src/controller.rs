//! The three-level controller (4.3) and its admission rule, as a pure state machine: every
//! method takes the time, nothing here sleeps or reads a clock. [`crate::ClientBudget`] wraps it
//! with a lock, a wake-up and the injected timer.
//!
//! - **Per drive d** ([`crate::device`]): decreased only by signals drive d produced: a `503`
//!   from d, or the median of d's first-byte time (per size class) or reported queue wait above
//!   twice d's own baseline for two windows. A LOCATE answer's load for d is a queue-wait sample
//!   of d.
//! - **Per storage node n**: decreased only when more than a third of n's drives with traffic are
//!   in a decrease state at once, or n's network time per MiB is above twice n's baseline for two
//!   windows.
//! - **Client ceiling**: decreased only when more than a third of all drives with traffic are in a
//!   decrease state, or the client's network time per MiB is above twice its baseline for two
//!   windows.
//!
//! A decrease is followed by a cooldown of one window with +-50% jitter. A level leaves its
//! decrease state at the first clean window after the cooldown, and steps back up only with a
//! backlog (a charge waited on that level's permits during the window).

use std::collections::BTreeMap;

use tierstore_fetch::plan::size_class;
use tierstore_fetch_api::budget::{Charge, Observation, Outcome};
use tierstore_fetch_api::ids::Class;
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals};
use tierstore_format::ids::NodeId;
use tierstore_metrics::labels::BudgetReason;

use crate::config::{BudgetConfig, CEILING_FLOOR};
use crate::device::{Device, DriveSignal};
use crate::level::Level;
use crate::quantities::{Quantities, reserve};
use crate::signal::Verdict;
use crate::snapshot::{DeviceView, NodeView, Snapshot};

/// Why a charge does not fit now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Blocked {
    /// The client ceiling (or, for `prepare` and `background`, the ceiling less the demand
    /// reserve).
    Ceiling,
    /// The storage node's level.
    Node,
    /// A device's permits (`until` is `None`) or its pacing gap (until that time).
    Device { until: Option<u64> },
}

/// What a waiting `demand` charge holds registered: one permit of each of its devices reserved
/// (`devices`), and, while the ceiling is what it waits for, priority over `prepare` and
/// `background` at the ceiling (`ceiling`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Waiting {
    pub devices: bool,
    pub ceiling: bool,
}

#[derive(Debug)]
pub(crate) struct Controller {
    cfg: BudgetConfig,
    /// The jobscheduler's hint and when it came.
    hint: Option<(u32, u64)>,
    client: Level,
    nodes: BTreeMap<NodeId, Level>,
    devices: BTreeMap<DeviceRef, Device>,
    /// Halvings of `P` from each class's steady value, shared by every size class.
    part_steps: u8,
    /// Demand charges waiting for the ceiling.
    demand_at_ceiling: u32,
    reason: BudgetReason,
    /// Decreases so far, the input of the cooldown's jitter.
    decreases: u64,
}

/// When a level's network window closes, and how it is judged.
#[derive(Debug, Clone, Copy)]
struct WindowRule {
    parts: u32,
    ms: u64,
    span: u64,
    floor: u32,
}

impl WindowRule {
    fn network(cfg: &BudgetConfig) -> Self {
        Self {
            parts: cfg.window_parts,
            ms: cfg.window_ms,
            span: cfg.baseline_ms,
            floor: cfg.network_floor_ms_per_mib,
        }
    }
}

/// Each device of a charge, with how many permits of it the charge takes.
fn multiplicity(devices: &[DeviceRef]) -> BTreeMap<DeviceRef, u32> {
    let mut m = BTreeMap::new();
    for d in devices {
        *m.entry(*d).or_insert(0) += 1;
    }
    m
}

/// splitmix64: the jitter's deterministic mix.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Controller {
    pub fn new(cfg: BudgetConfig, now: u64) -> Self {
        Self {
            hint: None,
            client: Level::new(None, now),
            nodes: BTreeMap::new(),
            devices: BTreeMap::new(),
            part_steps: 0,
            demand_at_ceiling: 0,
            reason: BudgetReason::Steady,
            decreases: 0,
            cfg,
        }
    }

    /// The active clients at `now`: the hint while it is fresh, else `N`.
    pub fn clients_active(&self, now: u64) -> u32 {
        match self.hint {
            Some((n, at)) if now.saturating_sub(at) <= self.cfg.hint_max_age_ms => n,
            _ => self.cfg.topology.clients,
        }
    }

    pub fn quantities(&self, now: u64) -> Quantities {
        Quantities::compute(
            &self.cfg,
            self.clients_active(now),
            self.part_size(u64::MAX),
        )
    }

    /// The client ceiling in force: the controller's limit (the share until the controller first
    /// moves it), at most the quantities' ceiling and at least the floor.
    pub fn ceiling(&self, now: u64) -> u32 {
        let q = self.quantities(now);
        self.client
            .limit
            .unwrap_or(q.share)
            .min(q.ceiling_parts)
            .max(CEILING_FLOOR)
    }

    /// `P` for a pack of `pack_size` bytes: its class's steady size halved once per step of the
    /// ladder, never below `P_floor`.
    pub fn part_size(&self, pack_size: u64) -> u64 {
        let class = usize::from(size_class(pack_size)).min(self.cfg.part_sizes.len() - 1);
        let steady = self.cfg.part_sizes.get(class).copied().unwrap_or(0);
        (steady >> self.part_steps).max(self.cfg.part_floor)
    }

    /// The ladder's last `P` step: the halvings that bring the largest steady size to the floor.
    fn max_part_steps(&self) -> u8 {
        let largest = self.cfg.part_sizes.iter().copied().max().unwrap_or(0);
        let mut steps = 0;
        while (largest >> steps) > self.cfg.part_floor {
            steps += 1;
        }
        steps
    }

    fn device_max(&self, d: DeviceRef) -> u32 {
        match d {
            DeviceRef::Drive(..) => self.cfg.drive_permits,
            DeviceRef::Hot(_) => self.cfg.hot_permits,
            DeviceRef::Legacy => self.cfg.legacy_permits.unwrap_or(u32::MAX),
        }
    }

    fn device(&mut self, d: DeviceRef, now: u64) -> &mut Device {
        let max = self.device_max(d);
        self.devices
            .entry(d)
            .or_insert_with(|| Device::new(max, now))
    }

    fn node(&mut self, n: NodeId, now: u64) -> &mut Level {
        let limit = self.cfg.node_permits;
        self.nodes
            .entry(n)
            .or_insert_with(|| Level::new(limit, now))
    }

    /// Takes `charge` if it fits every level now; else says what it waits for, and marks that
    /// level's window as backlogged.
    pub fn try_take(&mut self, charge: &Charge, now: u64) -> Result<(), Blocked> {
        let parts = charge.parts.max(1);
        let demand = charge.class == Class::Demand;
        let counts = multiplicity(&charge.devices);
        // Every device of the charge has state from here on, so a waiting demand's registration
        // (`rewait`) always finds the devices it reserves on.
        for &d in counts.keys() {
            self.device(d, now);
        }
        let ceiling = self.ceiling(now);
        let cap = match demand {
            true => ceiling,
            false => ceiling - reserve(&self.cfg, ceiling),
        };
        if !(demand || self.demand_at_ceiling == 0)
            || !Level::fits(self.client.in_flight, parts, Some(cap))
        {
            self.client.window.backlog = true;
            return Err(Blocked::Ceiling);
        }
        if let Some(n) = charge.node {
            let lvl = self.node(n, now);
            if !Level::fits(lvl.in_flight, parts, lvl.limit) {
                lvl.window.backlog = true;
                return Err(Blocked::Node);
            }
        }
        for (&d, &m) in &counts {
            let dev = self.device(d, now);
            let limit = match !demand && dev.demand_waiting > 0 {
                true => dev.permits - 1,
                false => dev.permits,
            };
            let blocked = if limit == 0 || !Level::fits(dev.in_flight, m, Some(limit)) {
                Some(Blocked::Device { until: None })
            } else {
                dev.paced_until()
                    .filter(|&t| now < t)
                    .map(|t| Blocked::Device { until: Some(t) })
            };
            if let Some(b) = blocked {
                dev.window.backlog = true;
                return Err(b);
            }
        }
        self.client.in_flight += parts;
        if let Some(n) = charge.node {
            self.node(n, now).in_flight += parts;
        }
        for (d, m) in counts {
            let dev = self.device(d, now);
            dev.in_flight += m;
            dev.last_start = Some(now);
        }
        Ok(())
    }

    /// Returns what [`Self::try_take`] took for `charge`.
    pub fn release(&mut self, charge: &Charge) {
        let parts = charge.parts.max(1);
        self.client.in_flight = self.client.in_flight.saturating_sub(parts);
        // Every level and device of a charge has state from the `try_take` that granted it.
        if let Some(n) = charge.node {
            self.nodes
                .entry(n)
                .and_modify(|lvl| lvl.in_flight = lvl.in_flight.saturating_sub(parts));
        }
        for (d, m) in multiplicity(&charge.devices) {
            self.devices
                .entry(d)
                .and_modify(|dev| dev.in_flight = dev.in_flight.saturating_sub(m));
        }
    }

    /// Moves a waiting `demand` charge's registration from `from` to `to`.
    pub fn rewait(&mut self, charge: &Charge, from: Waiting, to: Waiting) {
        if from.ceiling != to.ceiling {
            self.demand_at_ceiling = match to.ceiling {
                true => self.demand_at_ceiling + 1,
                false => self.demand_at_ceiling.saturating_sub(1),
            };
        }
        if from.devices != to.devices {
            // `try_take` gave every device of the charge state before the charge first waited.
            for d in multiplicity(&charge.devices).into_keys() {
                self.devices.entry(d).and_modify(|dev| {
                    dev.demand_waiting = match to.devices {
                        true => dev.demand_waiting + 1,
                        false => dev.demand_waiting.saturating_sub(1),
                    };
                });
            }
        }
    }

    /// Feeds the controller.
    pub fn observe(&mut self, obs: &Observation, now: u64) {
        match obs {
            Observation::Response {
                device,
                size_class,
                first_byte_ms,
                signals,
                outcome,
            } => self.response(*device, *size_class, *first_byte_ms, signals, *outcome, now),
            Observation::Network { node, ms_per_mib } => self.network(*node, *ms_per_mib, now),
            Observation::ClientsActive(n) => self.hint = Some((*n, now)),
        }
    }

    fn response(
        &mut self,
        d: DeviceRef,
        class: u8,
        first_byte_ms: Option<u32>,
        signals: &DriveSignals,
        outcome: Outcome,
        now: u64,
    ) {
        let load = signals.drive_state.is_none_or(|s| s.is_load_signal());
        if !load || matches!(outcome, Outcome::NotLoad | Outcome::Transport) {
            return;
        }
        let dev = self.device(d, now);
        dev.last_traffic = Some(now);
        dev.window.parts += 1;
        if outcome == Outcome::Busy {
            self.decrease_drive(d, BudgetReason::Drive503, now);
        } else {
            if let Some(ms) = first_byte_ms {
                dev.first_byte.entry(class).or_default().push(ms);
            }
            if let Some(ms) = signals.queue_wait_ms {
                dev.queue_wait.push(ms);
            }
        }
        self.close_drive(d, now);
    }

    /// A LOCATE answer's view of `d`'s queue: a queue-wait sample of `d`, if the client has
    /// traffic with `d` (a device it has none with has no state).
    pub fn locate(&mut self, d: DeviceRef, signals: &DriveSignals) {
        let load = signals.drive_state.is_none_or(|s| s.is_load_signal());
        if let (true, Some(dev), Some(ms)) = (load, self.devices.get_mut(&d), signals.queue_wait_ms)
        {
            dev.queue_wait.push(ms);
        }
    }

    fn cooldown(&mut self, now: u64) -> u64 {
        self.decreases += 1;
        let pct = 50 + mix(self.cfg.jitter_seed ^ self.decreases) % 101;
        now.saturating_add(self.cfg.window_ms * pct / 100)
    }

    fn decrease_drive(&mut self, d: DeviceRef, reason: BudgetReason, now: u64) {
        let cap = self.ceiling(now);
        let (start, max) = (self.cfg.pace_start_ms, self.cfg.pace_max_ms);
        if self.device(d, now).cooldown_until > now {
            self.reason = BudgetReason::Cooldown;
            return;
        }
        let until = self.cooldown(now);
        let dev = self.device(d, now);
        dev.step_down(cap, start, max);
        dev.decrease = true;
        dev.above = 0;
        dev.cooldown_until = until;
        self.reason = reason;
        self.evidence(d.node(), now);
    }

    fn close_drive(&mut self, d: DeviceRef, now: u64) {
        let cfg = &self.cfg;
        let (parts, ms, span, floor) = (
            cfg.window_parts,
            cfg.window_ms,
            cfg.baseline_ms,
            cfg.signal_floor_ms,
        );
        let pace_start = cfg.pace_start_ms;
        let dev = self.device(d, now);
        if !dev.window.due(now, parts, ms) {
            return;
        }
        let backlog = dev.window.backlog;
        let (verdict, which) = dev.close(now, span, floor);
        dev.window = crate::signal::Window::at(now);
        let cooling = now < dev.cooldown_until;
        match verdict {
            Verdict::Above => {
                dev.above += 1;
                // Inside the cooldown, `decrease_drive` holds (and says so).
                if dev.above >= 2 {
                    let reason = match which {
                        Some(DriveSignal::QueueWait) => BudgetReason::DriveQueueWait,
                        _ => BudgetReason::DriveFirstByte,
                    };
                    self.decrease_drive(d, reason, now);
                }
            }
            Verdict::Clean if !cooling => {
                dev.above = 0;
                let was = dev.decrease;
                dev.decrease = false;
                if backlog && dev.step() != crate::device::DriveStep::Full {
                    dev.step_up(pace_start);
                    self.reason = BudgetReason::Increase;
                }
                if was {
                    self.evidence(d.node(), now);
                }
            }
            Verdict::Clean => dev.above = 0,
            Verdict::Silent => {}
        }
    }

    /// Drives of `node` (every drive for `None`) with traffic, and how many are in a decrease
    /// state.
    fn drive_counts(&self, node: Option<NodeId>, now: u64) -> (usize, usize) {
        let traffic = self.cfg.traffic_ms;
        let with: Vec<&Device> = self
            .devices
            .iter()
            .filter(|(d, _)| node.is_none() || d.node() == node)
            .filter(|(_, dev)| {
                dev.last_traffic
                    .is_some_and(|t| t.saturating_add(traffic) >= now)
            })
            .map(|(_, dev)| dev)
            .collect();
        let dec = with.iter().filter(|dev| dev.decrease).count();
        (with.len(), dec)
    }

    fn drives_over_third(&self, node: Option<NodeId>, now: u64) -> bool {
        let (with, dec) = self.drive_counts(node, now);
        dec * 3 > with
    }

    /// The drive evidence of the node and client levels, after a drive's decrease state changed.
    fn evidence(&mut self, node: Option<NodeId>, now: u64) {
        if let Some(n) = node
            && self.drives_over_third(Some(n), now)
        {
            self.decrease_node(n, BudgetReason::NodeDrives, now);
        }
        if self.drives_over_third(None, now) {
            self.decrease_client(BudgetReason::ClientDrives, now);
        }
    }

    fn decrease_node(&mut self, n: NodeId, reason: BudgetReason, now: u64) {
        let ceiling = self.ceiling(now);
        let max_steps = self.max_part_steps();
        if self.node(n, now).cooldown_until > now {
            self.reason = BudgetReason::Cooldown;
            return;
        }
        let until = self.cooldown(now);
        let lvl = self.node(n, now);
        let eff = lvl.limit.unwrap_or(u32::MAX).min(ceiling);
        let parts = lvl.step_down(eff, 1);
        lvl.stepped_part = !parts;
        lvl.decrease = true;
        lvl.above = 0;
        lvl.cooldown_until = until;
        if !parts && self.part_steps < max_steps {
            self.part_steps += 1;
        }
        self.reason = reason;
    }

    fn decrease_client(&mut self, reason: BudgetReason, now: u64) {
        if self.client.cooldown_until > now {
            self.reason = BudgetReason::Cooldown;
            return;
        }
        let eff = self.ceiling(now);
        let until = self.cooldown(now);
        let parts = self.client.step_down(eff, CEILING_FLOOR);
        self.client.stepped_part = !parts;
        self.client.decrease = true;
        self.client.above = 0;
        self.client.cooldown_until = until;
        if !parts && self.part_steps < self.max_part_steps() {
            self.part_steps += 1;
        }
        self.reason = reason;
    }

    fn network(&mut self, node: Option<NodeId>, ms_per_mib: u32, now: u64) {
        self.client.network.push(ms_per_mib);
        self.client.window.parts += 1;
        if let Some(n) = node {
            let lvl = self.node(n, now);
            lvl.network.push(ms_per_mib);
            lvl.window.parts += 1;
            self.close_node(n, now);
        }
        self.close_client(now);
    }

    /// Closes a level's window if due: its network verdict, and whether it had a backlog.
    fn close_level(rule: WindowRule, lvl: &mut Level, now: u64) -> Option<(Verdict, bool)> {
        if !lvl.window.due(now, rule.parts, rule.ms) {
            return None;
        }
        let backlog = lvl.window.backlog;
        let v = lvl.network.close(now, rule.span, rule.floor);
        lvl.window = crate::signal::Window::at(now);
        lvl.above = match v {
            Verdict::Above => lvl.above + 1,
            _ => 0,
        };
        Some((v, backlog))
    }

    fn close_node(&mut self, n: NodeId, now: u64) {
        let ceiling = self.ceiling(now);
        let drives = self.drives_over_third(Some(n), now);
        let rule = WindowRule::network(&self.cfg);
        let node_permits = self.cfg.node_permits;
        // `P` comes back from a node's own backlog too (its parts are what the node's limit
        // holds, so the ceiling may never fill), once nothing else is in decrease.
        let others_clear =
            !self.client.decrease && self.nodes.iter().all(|(m, l)| *m == n || !l.decrease);
        let restore_part = self.part_steps > 0 && others_clear;
        let lvl = self.node(n, now);
        let Some((v, backlog)) = Self::close_level(rule, lvl, now) else {
            return;
        };
        let cooling = now < lvl.cooldown_until;
        if v == Verdict::Above {
            if lvl.above >= 2 {
                self.decrease_node(n, BudgetReason::NodeNetwork, now);
            }
        } else if !cooling && !drives {
            lvl.decrease = false;
            lvl.stepped_part = false;
            // A backlog at a node means its limit was full: an unbounded node has none.
            if backlog && restore_part {
                self.part_steps -= 1;
                self.reason = BudgetReason::Increase;
            } else if backlog {
                lvl.limit = lvl.limit.and_then(|l| {
                    let next = l.saturating_add(1);
                    match node_permits {
                        Some(max) => Some(next.min(max)),
                        // Unbounded again once the client's own ceiling binds first.
                        None => (next < ceiling).then_some(next),
                    }
                });
                self.reason = BudgetReason::Increase;
            }
        }
    }

    fn close_client(&mut self, now: u64) {
        let drives = self.drives_over_third(None, now);
        let q = self.quantities(now);
        let ceiling = self.ceiling(now);
        let nodes_clear = self.nodes.values().all(|l| !l.decrease);
        let rule = WindowRule::network(&self.cfg);
        let Some((v, backlog)) = Self::close_level(rule, &mut self.client, now) else {
            return;
        };
        let cooling = now < self.client.cooldown_until;
        if v == Verdict::Above {
            if self.client.above >= 2 {
                self.decrease_client(BudgetReason::ClientNetwork, now);
            }
        } else if !cooling && !drives {
            self.client.decrease = false;
            self.client.stepped_part = false;
            if backlog {
                self.increase_client(&q, ceiling, nodes_clear);
            }
        }
    }

    /// One step up the client's ladder in reverse: `P` first (while no node is in a decrease
    /// state), then the parts, doubling toward the share and then one per window.
    fn increase_client(&mut self, q: &Quantities, ceiling: u32, nodes_clear: bool) {
        if self.part_steps > 0 && nodes_clear {
            self.part_steps -= 1;
            self.reason = BudgetReason::Increase;
        } else if ceiling < q.share {
            self.client.limit = Some((ceiling * 2).min(q.share));
            self.reason = BudgetReason::SlowStart;
        } else if ceiling < q.ceiling_parts {
            self.client.limit = Some(ceiling + 1);
            self.reason = BudgetReason::Increase;
        }
    }

    pub fn snapshot(&self, now: u64) -> Snapshot {
        Snapshot {
            quantities: self.quantities(now),
            ceiling: self.ceiling(now),
            parts_in_flight: self.client.in_flight,
            ceiling_step: self.client.step(),
            part_steps: self.part_steps,
            reason: self.reason,
            nodes: self
                .nodes
                .iter()
                .map(|(n, l)| {
                    let view = NodeView {
                        limit: l.limit,
                        in_flight: l.in_flight,
                        step: l.step(),
                    };
                    (*n, view)
                })
                .collect(),
            devices: self
                .devices
                .iter()
                .map(|(d, dev)| {
                    let view = DeviceView {
                        permits: dev.permits,
                        max: dev.max,
                        in_flight: dev.in_flight,
                        demand_waiting: dev.demand_waiting,
                        pace_ms: dev.pace_ms,
                        decrease: dev.decrease,
                        step: dev.step(),
                    };
                    (*d, view)
                })
                .collect(),
        }
    }
}

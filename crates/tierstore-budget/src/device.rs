//! Per-drive state (4.3, "Per drive d"): the drive's permits, its pacing, its decrease state and
//! cooldown, its window and the baselines of the signals drive d produced (first byte per size
//! class, the reported queue wait). Nothing here reads another drive's state.

use std::collections::BTreeMap;

use crate::signal::{Series, Verdict, Window, worst};

/// The step of a drive's ladder (`drive_decrease_state`, section 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DriveStep {
    /// At `per_drive_max`, not paced.
    Full,
    /// Permits below `per_drive_max`.
    Permits,
    /// One permit and a minimum gap between part starts.
    Pacing,
}

impl DriveStep {
    /// The gauge value: 0 none, 1 permits lowered, 2 pacing.
    #[must_use]
    pub const fn gauge(self) -> u8 {
        match self {
            Self::Full => 0,
            Self::Permits => 1,
            Self::Pacing => 2,
        }
    }
}

/// Which of a drive's signals a closed window found above twice its baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriveSignal {
    FirstByte,
    QueueWait,
}

#[derive(Debug, Clone)]
pub(crate) struct Device {
    /// `per_drive_max[d]`.
    pub max: u32,
    /// The permits now.
    pub permits: u32,
    /// The minimum gap between part starts, 0 when not paced.
    pub pace_ms: u64,
    pub last_start: Option<u64>,
    pub in_flight: u32,
    /// Demand charges waiting with this device among their devices.
    pub demand_waiting: u32,
    pub decrease: bool,
    pub cooldown_until: u64,
    /// Consecutive windows with a signal above twice its baseline.
    pub above: u32,
    pub window: Window,
    pub first_byte: BTreeMap<u8, Series>,
    pub queue_wait: Series,
    /// When the device last gave a load answer; `None` until its first.
    pub last_traffic: Option<u64>,
}

impl Device {
    pub fn new(max: u32, now: u64) -> Self {
        Self {
            max,
            permits: max,
            pace_ms: 0,
            last_start: None,
            in_flight: 0,
            demand_waiting: 0,
            decrease: false,
            cooldown_until: 0,
            above: 0,
            window: Window::at(now),
            first_byte: BTreeMap::new(),
            queue_wait: Series::default(),
            last_traffic: None,
        }
    }

    pub fn step(&self) -> DriveStep {
        match (self.pace_ms, self.permits < self.max) {
            (0, false) => DriveStep::Full,
            (0, true) => DriveStep::Permits,
            _ => DriveStep::Pacing,
        }
    }

    /// The earliest time a part may start, when paced.
    pub fn paced_until(&self) -> Option<u64> {
        let last = self.last_start?;
        (self.pace_ms > 0).then(|| last.saturating_add(self.pace_ms))
    }

    /// One step down the ladder: permits `x 0.75` rounded down and at least one step, from at
    /// most `cap` (the permits a client can use at all, its ceiling); at one permit, pacing,
    /// doubling up to `pace_max`.
    pub fn step_down(&mut self, cap: u32, pace_start: u64, pace_max: u64) {
        let eff = self.permits.min(cap.max(1));
        if eff > 1 {
            self.permits = (eff * 3 / 4).min(eff - 1).max(1);
        } else {
            self.permits = 1;
            self.pace_ms = match self.pace_ms {
                0 => pace_start,
                p => p.saturating_mul(2).min(pace_max),
            };
        }
    }

    /// One step back up, the ladder in reverse: the pacing gap halves (and ends below its first
    /// step), then the permits grow by one up to `max`.
    pub fn step_up(&mut self, pace_start: u64) {
        if self.pace_ms > 0 {
            self.pace_ms /= 2;
            if self.pace_ms < pace_start {
                self.pace_ms = 0;
            }
        } else {
            self.permits = self.permits.saturating_add(1).min(self.max);
        }
    }

    /// Closes the window: the worst verdict of its signals, and which signal was above.
    pub fn close(&mut self, now: u64, span: u64, floor: u32) -> (Verdict, Option<DriveSignal>) {
        let mut verdict = Verdict::Silent;
        let mut which = None;
        for series in self.first_byte.values_mut() {
            let v = series.close(now, span, floor);
            if v == Verdict::Above {
                which = Some(DriveSignal::FirstByte);
            }
            verdict = worst(verdict, v);
        }
        let v = self.queue_wait.close(now, span, floor);
        if v == Verdict::Above {
            which = which.or(Some(DriveSignal::QueueWait));
        }
        (worst(verdict, v), which)
    }
}

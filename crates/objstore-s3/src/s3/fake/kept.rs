//! Bytes as the fake service keeps them: a run of zeros is counted, not stored.

use bytes::Bytes;

use super::RUN;

static ZEROS: [u8; RUN] = [0; RUN];

/// Bytes kept compactly.
#[derive(Clone, Debug, Default)]
pub(super) struct Kept {
    /// Runs in order: `Err(n)` is `n` zero bytes.
    pub(super) runs: Vec<Result<Bytes, u64>>,
    pub(super) len: u64,
}

impl Kept {
    pub(super) fn of(bytes: &[u8]) -> Self {
        let mut kept = Self::default();
        for run in bytes.chunks(RUN) {
            if run == &ZEROS[..run.len()] {
                kept.push(Err(run.len() as u64));
            } else {
                kept.push(Ok(Bytes::copy_from_slice(run)));
            }
        }
        kept
    }

    pub(super) fn push(&mut self, run: Result<Bytes, u64>) {
        let n = match &run {
            Ok(b) => b.len() as u64,
            Err(n) => *n,
        };
        self.len += n;
        match (self.runs.last_mut(), run) {
            (Some(Err(zeros)), Err(more)) => *zeros += more,
            (_, run) => self.runs.push(run),
        }
    }

    pub(super) fn extend(&mut self, other: &Self) {
        for run in &other.runs {
            self.push(run.clone());
        }
    }

    /// Bytes `[from, to)`.
    pub(super) fn read(&self, from: u64, to: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(usize::try_from(to - from).unwrap_or(0));
        let mut at = 0u64;
        for run in &self.runs {
            let n = match run {
                Ok(b) => b.len() as u64,
                Err(n) => *n,
            };
            let (start, end) = (from.max(at), to.min(at + n));
            if start < end {
                let (a, b) = ((start - at) as usize, (end - at) as usize);
                match run {
                    Ok(bytes) => out.extend_from_slice(&bytes[a..b]),
                    Err(_) => out.resize(out.len() + (b - a), 0),
                }
            }
            at += n;
            if at >= to {
                break;
            }
        }
        out
    }
}

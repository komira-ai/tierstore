//! The numbers of file handles. A handle is the kernel's name for an open file, which it keeps
//! across the death of the process that made it; so a number says which process made it (its
//! `generation`, the count of processes that have served the connection) and whether it is a
//! passthrough open, and a process that takes the connection over can tell a handle of an earlier
//! process from its own (`docs/design/virtual-build-fs.md` section 7.1).
//!
//! ```text
//! bit 63      the open was a passthrough open (its release is counted on the blob's backing id)
//! bits 40-62  the generation of the process that made it, 1 to MOST_GENERATION
//! bits 0-39   the process's own count of opens
//! ```

/// Set in the number of a passthrough open.
const BACKED: u64 = 1 << 63;

/// Where the generation begins.
const GENERATION_SHIFT: u32 = 40;

/// The most a generation may be: the bits it has.
pub(super) const MOST_GENERATION: u32 = (1 << 23) - 1;

/// The bits of a process's own count.
const COUNT_MASK: u64 = (1 << GENERATION_SHIFT) - 1;

/// The generation after `generation`: the next number, and 1 after the most there is (a
/// connection that outlives eight million takeovers).
pub(super) fn next_generation(generation: u32) -> u32 {
    if generation >= MOST_GENERATION {
        1
    } else {
        generation + 1
    }
}

/// The handle number of the `count`th open of the process of `generation`.
pub(super) fn make(generation: u32, count: u64, backed: bool) -> u64 {
    let backed = if backed { BACKED } else { 0 };
    backed | (u64::from(generation) << GENERATION_SHIFT) | (count & COUNT_MASK)
}

/// The generation of the process that made `handle`.
pub(super) fn generation_of(handle: u64) -> u32 {
    // Bit 63 is the flag; the generation is the 23 bits below it.
    ((handle & !BACKED) >> GENERATION_SHIFT) as u32
}

/// Whether `handle` is the number of a passthrough open.
pub(super) fn is_backed(handle: u64) -> bool {
    handle & BACKED != 0
}

#[cfg(test)]
#[path = "handle_tests.rs"]
mod tests;

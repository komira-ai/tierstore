//! The numbers of file handles. A handle is the kernel's name for an open file, which it keeps
//! across the death of the process that made it; so a number says which process made it (its
//! `generation`, the count of processes that have served the connection), and a process that takes
//! the connection over can tell a handle of an earlier process from its own
//! (`docs/design/virtual-build-fs.md` section 7.1).
//!
//! ```text
//! bit 63      set by an earlier release on a passthrough open; this one leaves it clear
//! bits 40-62  the generation of the process that made it, 1 to MOST_GENERATION
//! bits 0-39   the process's own count of opens
//! ```

/// The bit an earlier release set in the number of a passthrough open: not the generation's.
const PASSTHROUGH: u64 = 1 << 63;

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
pub(super) fn make(generation: u32, count: u64) -> u64 {
    (u64::from(generation) << GENERATION_SHIFT) | (count & COUNT_MASK)
}

/// The generation of the process that made `handle`.
pub(super) fn generation_of(handle: u64) -> u32 {
    // Bit 63 is an earlier release's flag; the generation is the 23 bits below it.
    ((handle & !PASSTHROUGH) >> GENERATION_SHIFT) as u32
}

#[cfg(test)]
#[path = "handle_tests.rs"]
mod tests;

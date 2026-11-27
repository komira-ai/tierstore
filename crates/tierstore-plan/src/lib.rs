//! The Prepare planner of the read path (`docs/design/fetch-cache-serve.md`, section 6; section
//! 18 row T7): a pure function of a tree list and the byte ranges a job needs, with no learned or
//! recorded input (6.1). It says which cells of which packs Prepare ensures, at what rank, so that
//! the fetch engine (section 4) reads them as few streams as the drives serve best.
//!
//! What it does, in the order of section 6:
//! 1. **Needed cells and ranks.** A needed range covers the cells it touches (1 MiB cells, 2). The
//!    ranges are ranked shared packs first (6.1): the ranges of the packs whose PACKS record has
//!    `pack_flags` bit 1 set (`PACK_SHARED`, which the CAS service sets on a pack many trees name),
//!    in tree order, then every other range, in tree order. A reader ignores the flag bits it does
//!    not know, so no other bit moves a rank. A cell's rank (4.5) is the lowest rank of the needed
//!    ranges that cover it. Lower is sooner.
//! 2. **The gap rule, per drive.** Needed runs of one pack are merged across a gap of at most
//!    `gap_cells` absent cells, `gap_cells = ceil(positioning x media rate / cell bytes)` of the
//!    device the pack is read from ([`config::DriveModel`]): about 3 on a cold hard drive and 0 on
//!    a hot NVMe or SSD copy. Gap cells are real cells: the engine verifies, writes and publishes
//!    them like any other.
//! 3. **The whole-pack rule, budget-aware.** `cov`, the merged cells over the pack's cells, of 50%
//!    or more fetches the whole pack; a pack of 64 MiB or less is fetched whole from 20%. A pack
//!    that would be whole is fetched as its runs while the node's queued backlog exceeds 2 s of
//!    budget throughput.
//!
//! The output is a list of [`plan::Ensure`]s, each one `Engine::ensure` call of class `prepare`
//! (runs of one pack at one rank), sorted by rank. Nothing here reads a clock, a file or a cell
//! state: what is already present is skipped by the engine's claims (4.2), and cancelling a
//! Prepare (section 6 step 5) demotes the engine's unstarted parts, not this plan.

#![forbid(unsafe_code)]

pub mod config;
pub mod plan;

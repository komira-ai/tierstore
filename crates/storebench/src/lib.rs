//! `storebench`: the measurements of the storage-tier design that need no tier code (design
//! `docs/design/fetch-cache-serve.md`, section 16 C, task T0-F-a).
//!
//! - **C-6**, silo's objects: sizes by kind, `L_silo` and `L_live`, and what silo's drives hold
//!   beyond its objects (the raw-to-logical gap).
//! - **C-7**, placement: the key set under the storage nodes' rendezvous rank, and stripes over
//!   the drives with nodes down, for both deployment profiles (design section 21).
//! - **C-10**, garbage collection replayed: the objects' arrivals and deaths against the tier's
//!   sealing, stripe formation and free rules.
//!
//! The farm-side verbs read an S3 store (`list`, `footers`) or a drive (`drive-list`, `du`) and
//! write files; the offline verbs (`c6`, `c7`, `c10`) read those files and write a summary; `report`
//! prints the tables of the measurement document from the summaries committed in `data/`.

pub mod c6;
pub mod cli;
pub mod drivewalk;
pub mod du;
pub mod error;
pub mod footers;
pub mod gc;
pub mod kinds;
pub mod listing;
pub mod placement;
pub mod placement_tables;
pub mod report;
pub mod s3;
pub mod table;
pub mod xlmeta;

pub use error::Error;

//! The marker protocol of `docs/design/put-without-lock.md`, one key, up to three client actors
//! (two proxies and, in the rollout scenarios, one old-release proxy), one leader with its
//! copier/reaper/copy-back, evictor and stray sweep, and the hot and cold stores as linearizable
//! maps with only the capabilities measured in section 2.2 of the document: create-only PUT,
//! `If-Match` PUT, plain PUT and a plain unconditional DELETE.

mod client;
mod config;
mod leader;
mod state;

pub use client::Pc;
pub use config::{Body, Cfg, Flaw, GhostKind, Init, Op, Phase};
pub use state::{Act, Marks, Mk, Resp, St, State, Why};

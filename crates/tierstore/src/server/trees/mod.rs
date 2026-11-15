//! The `trees` interface: the gRPC API that creates, shows and releases node-local trees
//! (`docs/design/node-local-trees.md`, tracking issue komira-ai/tierstore#326).
//!
//! ```toml
//! [[interface]]
//! type = "trees"
//! params = { port = 9600 }
//! ```
//!
//! The contract is `trees.proto` of the `tierstore-trees-api` crate (package
//! `tierstore.trees.v1`, service `TreesService`: `CreateTree`, `GetPlacement`, `DeleteTree`). This
//! module is the front, not the work: it listens, authenticates, refuses what a key may not ask
//! and what the design names a refusal of, and hands the call to a [`TreeCore`]. The core, the
//! node-local tree store, comes in another change; until a node has one, every call that gets past
//! the key check is answered `UNAVAILABLE` with the message `NotServing`.
//!
//! # Keys
//!
//! A call is signed as an S3 request is, with the access keys the server already has (the same
//! `Authorization: AWS4-HMAC-SHA256` header over `host`, `x-amz-content-sha256` and `x-amz-date`;
//! the method is `POST` and the path is the gRPC path, `/tierstore.trees.v1.TreesService/<method>`).
//! `host` is the `:authority` of the call when the client sends no `host` header. The body is not
//! covered: the payload hash is `UNSIGNED-PAYLOAD`, as a presigned URL's is, so serve the
//! interface over TLS when it is not on loopback. No new permission exists (design 3.2, Q7):
//!
//! - the full key may make every call;
//! - a read-only key may `GetPlacement` and nothing that changes a placement;
//! - a scoped key (a GC key has no right here, as it has no read or write prefix) may `CreateTree`
//!   when it may read every source object (`from.key`, or `cas/l/<sha256>`) and may write every
//!   `s3` location (a write prefix that is a prefix of the location's); an `fs` location may be
//!   placed by the full key alone. It may `DeleteTree` and `GetPlacement` as far as the core finds
//!   the placement's locations to be its own.
//!
//! A call with no key, an unknown one, a wrong signature or a stale time is `UNAUTHENTICATED`; a
//! key that is known and lacks the right is `PERMISSION_DENIED`, naming the first key or location
//! that failed.
//!
//! # Refusals
//!
//! [`Refusal`] is every refusal the design names, by its name, with the status that stands for its
//! HTTP status there (see [`Reason`]).

mod listener;
mod refusal;
mod rights;
mod service;
mod setup;
mod tree_core;

pub(super) use self::listener::TreesListener;
pub(super) use self::refusal::{Reason, Refusal};
pub(super) use self::setup::{DEFAULT_PORT, TreesFlags, TreesSetup, trees_setup_of};
pub(super) use self::tree_core::{Caller, TreeCore};

#[cfg(test)]
mod listener_tests;
#[cfg(test)]
mod refusal_tests;
#[cfg(test)]
mod rig;
#[cfg(test)]
mod rights_tests;
#[cfg(test)]
mod service_tests;
#[cfg(test)]
mod setup_tests;
#[cfg(test)]
mod start_tests;

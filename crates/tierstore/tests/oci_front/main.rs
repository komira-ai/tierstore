//! The OCI read front over real TLS on loopback (a client that trusts a test CA reads manifests
//! and blobs by IP address) and over plain HTTP on loopback, the limits of the listener hold in
//! both, the binary's flags start it, and the binary lists `handover-v1` only with the behaviours
//! the name promises. With keys required, a client logs in with HTTP Basic and what each kind of key
//! may do is held over TLS.

mod auth;
mod binary;
mod cache_node;
mod client;
mod drain;
mod front;
mod handover_v1;
mod limits;
mod pki;
mod plain;
mod rig;

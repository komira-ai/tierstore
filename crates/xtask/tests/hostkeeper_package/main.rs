//! The hostkeeper package `cargo xtask release hostkeeper-package` writes, read by the code that
//! reads it on a host: hostkeeper's unpacker and manifest loader (komira-ai/hostkeeper#23 as
//! merged, which reads the manifest's `platform` key and launchd jobs), and its placed-path lint
//! with the manifest loader of an earlier commit of main, taken at pinned commits. The archive's bytes are read here too: its zstd frame, its
//! tar headers, the manifest as TOML, the unit's assignments and the plist as XML. Every platform
//! is built and read on the Linux runner from a stand-in binary; the binary is never run.
//!
//! - `archive`: one zstd frame, the entries and their headers, the binary byte for byte, the same
//!   bytes on every run;
//! - `linux`: the two Linux packages unpacked and loaded, the manifest's keys, the unit, the
//!   placed-path lint;
//! - `macos`: the macOS manifest and plist, loaded by main's loader, and its archive unpacked.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod archive;
mod linux;
mod macos;
mod support;

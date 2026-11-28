//! The runner of `examples/`: each example is another project's S3 client (a compiler cache, a
//! query engine, ...) pointed at tierstore, with the exact results it must give.
//!
//! An example is a directory of plain files, every one of them shown on its `page.md`:
//!
//! - `tierstore.toml`: the configuration tierstore runs with. Its tiers are directories in the
//!   run's work directory, never a cloud bucket, and it listens on `127.0.0.1:0`, a free port.
//! - `example.toml` ([`spec`]): the client and its pinned version, the files to generate, the
//!   environment, the steps (each a list of commands), the commands that always run at the end,
//!   and the checks.
//! - `expected.txt`: the exact lines the checks must give, one `name value` to a line.
//! - the inputs the commands and the generator read (a template, a `Makefile`, ...).
//!
//! A run ([`run::run`]) checks the page ([`page`]), checks the client's version, generates the
//! inputs, starts tierstore ([`server`]) with an empty environment, reads `/metricz` before and
//! after each step ([`metrics`]), runs the steps' commands with an environment of the example's
//! own ([`steps`]), runs the closing commands, evaluates the checks ([`checks`]) and compares
//! their lines with `expected.txt` ([`report`]). The client is any program in any language: the
//! runner only starts processes, reads the files they write and tierstore's counters.
//!
//! In `tierstore.toml` and in the commands and environment of `example.toml`, `${name}` is a
//! value of the run ([`spec::expand`]): `${work}` (the work directory, absolute), `${example}`
//! (the example's directory, absolute), `${endpoint}` (`http://127.0.0.1:<port>` of tierstore's
//! S3 front; not in `tierstore.toml`), `${bucket}` (the `server.bucket` of `tierstore.toml`),
//! `${access_key_id}` and `${secret_access_key}` (the key the runner writes to
//! `${work}/access.key`), and `${port:NAME}`, a free loopback port picked once for each `NAME`.
//! `$$` is one `$`.

pub mod checks;
pub mod cli;
pub mod error;
pub mod generate;
pub mod metrics;
pub mod page;
pub mod report;
pub mod run;
pub mod server;
pub mod spec;
pub mod steps;

pub use error::Error;

/// The access key id of the key the runner writes to `${work}/access.key`.
pub const ACCESS_KEY_ID: &str = "AKIDEXAMPLE";

/// The secret of that key.
pub const SECRET_ACCESS_KEY: &str = "examples-secret-0123456789";

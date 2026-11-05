//! `prepare`: can the node take the update, and which way; the record of the update. Nothing is
//! started.

use std::path::Path;

use super::super::params::{CONFIG_FILE_KEY, Params};
use super::super::ports::Instance;
use super::super::record::{Mode, Record};
use super::super::report::{Done, Reason};
use super::super::{
    CACHE_HANDOFF_CAPABILITY, HANDOFF_CAPABILITY, HANDOFF_SOCKET_KEY, LOCAL_CACHE_TABLE,
};
use super::{Ctx, unit_name};

/// How long hostkeeper waits before it asks again when both instances run.
const BOTH_RUNNING_RETRY_SECS: u64 = 30;

pub(super) fn prepare(
    c: &Ctx<'_, '_>,
    params: &Params,
    record: Option<&Record>,
) -> Result<Done, Done> {
    if let Some(record) = record
        && record.updating
        && !record.markers.is_empty()
    {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "an update is past its cutover: finish or roll it back first",
        ));
    }
    let update = params.previous.as_ref();
    // A node with a local cache hands over only between releases whose cache shapes do.
    let mut needed = vec![HANDOFF_CAPABILITY];
    if update.is_some() && !params.declared_restart && check_handoff_socket(c, params)? {
        needed.push(CACHE_HANDOFF_CAPABILITY);
    }
    // The staged binary is asked on a first install too: it proves `bin/tierstore` is there and
    // runs.
    let staged = c.ports.host.capabilities(&params.staged_release)?;
    let mode = match update {
        None => None,
        Some(_) if params.declared_restart => Some(Mode::Restart),
        Some(previous) => {
            let before = c.ports.host.capabilities(&previous.release)?;
            let can = |list: &[String]| {
                needed
                    .iter()
                    .all(|need| list.iter().any(|word| word == need))
            };
            Some(if can(&staged) && can(&before) {
                Mode::Handover
            } else {
                Mode::Restart
            })
        }
    };
    // Before anything is recorded or stopped, each release that will run the configuration says
    // whether it accepts it: a refusal found later leaves a unit that exits 2 and is not retried.
    if let Some(config) = &params.config_file {
        check_config(c, "staged", &params.staged_release, config)?;
        if let Some(previous) = update {
            check_config(c, "previous", &previous.release, config)?;
        }
    }
    // A run again keeps the instances an earlier run chose: nothing has been started since.
    let kept = record.filter(|record| record.updating);
    let live = match kept {
        Some(kept) => kept.live,
        None => serving(c, update.is_some(), record)?,
    };
    let next = kept
        .and_then(|kept| kept.next)
        .unwrap_or_else(|| live.map_or(Instance::A, Instance::other));
    let mut fresh = Record::idle(live);
    fresh.updating = true;
    fresh.from = update.map(|previous| previous.release.clone());
    fresh.to = Some(params.staged_release.clone());
    fresh.version = Some(params.target_version.clone());
    fresh.next = Some(next);
    fresh.mode = mode;
    c.save(&fresh)?;
    Ok(Done::ok(format!(
        "prepared {}: {}, serving {}, next {}",
        params.target_version,
        mode.map_or("first install", Mode::as_str),
        live.map_or_else(|| "none".to_owned(), unit_name),
        unit_name(next),
    )))
}

/// Has `release` check `config`; a release that refuses it fails `prepare` with the line it said.
fn check_config(c: &Ctx<'_, '_>, which: &str, release: &Path, config: &Path) -> Result<(), Done> {
    match c.ports.host.check_config(release, config)? {
        None => Ok(()),
        Some(line) => Err(Done::fail(
            Reason::ConfigRefused,
            format!(
                "the {which} release {} refuses {}: {line}",
                release.display(),
                config.display()
            ),
        )),
    }
}

/// The instance that serves now. Both running is not a state to start an update from: the old
/// instance of an earlier update still drains.
fn serving(
    c: &Ctx<'_, '_>,
    update: bool,
    record: Option<&Record>,
) -> Result<Option<Instance>, Done> {
    let a = c.ports.units.state(Instance::A)?.running();
    let b = c.ports.units.state(Instance::B)?.running();
    match (a, b) {
        (true, true) => Err(Done::deferred(
            Reason::HostNotReady,
            "both instances run: the old one of an earlier update still drains",
            BOTH_RUNNING_RETRY_SECS,
        )),
        (true, false) => Ok(Some(Instance::A)),
        (false, true) => Ok(Some(Instance::B)),
        // Nothing runs: an update goes on from the instance the record says served last.
        (false, false) => Ok(record.filter(|_| update).and_then(|record| record.live)),
    }
}

/// Holds the node's configuration to naming the hand-over socket: without it the new instance
/// could not take the old one's listening sockets. Says whether the configuration has a
/// `[local_cache]` table (a build node, or a storage node with a cache).
fn check_handoff_socket(c: &Ctx<'_, '_>, params: &Params) -> Result<bool, Done> {
    let path = params.config_file.as_deref().ok_or_else(|| {
        Done::fail(
            Reason::BadParams,
            format!("params key {CONFIG_FILE_KEY:?} is required: it names the configuration"),
        )
    })?;
    let text = c.ports.host.config(path)?.ok_or_else(|| {
        Done::fail(
            Reason::ConfigMissing,
            format!("there is no configuration file at {}", path.display()),
        )
    })?;
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        Done::fail(
            Reason::HandoffNotConfigured,
            format!("{} is not TOML: {}", path.display(), e.message()),
        )
    })?;
    let named = table
        .get("server")
        .and_then(toml::Value::as_table)
        .and_then(|server| server.get(HANDOFF_SOCKET_KEY))
        .and_then(toml::Value::as_str)
        .is_some_and(|socket| !socket.is_empty());
    if named {
        return Ok(table.contains_key(LOCAL_CACHE_TABLE));
    }
    Err(Done::fail(
        Reason::HandoffNotConfigured,
        format!(
            "{} has no [server] {HANDOFF_SOCKET_KEY}: the new instance could not take the old \
             one's sockets; set it, or set mode = \"restart\" in the params",
            path.display()
        ),
    ))
}

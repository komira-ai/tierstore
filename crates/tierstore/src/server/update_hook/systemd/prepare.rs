//! `prepare`: can the node take the update, and which way; the record of the update. Nothing is
//! started.

use std::path::Path;

use super::super::old_layout;
use super::super::params::{CONFIG_FILE_KEY, Params};
use super::super::ports::Instance;
use super::super::record::{Mode, Record};
use super::super::report::{Done, Reason};
use super::super::shape::fs_interface;
use super::super::{
    CACHE_HANDOFF_CAPABILITY, DIR_TIER_KIND, HANDOFF_CAPABILITY, HANDOFF_SERVICE_CAPABILITY,
    HANDOFF_SOCKET_KEY, HANDOFF_STATE_DIR_CAPABILITY, HOT_STORE_TABLE, LOCAL_CACHE_TABLE,
};
use super::cleanup::{drain_note, finish};
use super::{Ctx, unit_name};

/// How long hostkeeper waits before it asks again when both instances run.
const BOTH_RUNNING_RETRY_SECS: u64 = 30;

pub(super) fn prepare(
    c: &Ctx<'_, '_>,
    params: &Params,
    record: Option<&Record>,
) -> Result<Done, Done> {
    // A committed update still in flight is an earlier one whose `cleanup` did not finish (one of
    // an earlier release gave up while the old instance drained): nothing of it can be undone, and
    // it is finished first, as `cleanup` finishes it. The update is then prepared from the record
    // at rest, on what the units say now.
    let finished;
    let (record, done_note) = match record {
        Some(earlier) if earlier.updating && earlier.committed() => {
            let (at_rest, drains) = finish(c, earlier)?;
            finished = at_rest;
            (
                Some(&finished),
                format!(
                    "finished the committed update to {}{}; ",
                    earlier.version.as_deref().unwrap_or("an unknown version"),
                    drain_note(&drains)
                ),
            )
        }
        other => (other, String::new()),
    };
    if let Some(record) = record
        && record.updating
        && !record.markers.is_empty()
    {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "an update is past its cutover: finish or roll it back first",
        ));
    }
    // Before anything is saved or asked of a release: a node that still runs the two-process
    // layout is not updated by this release.
    old_layout::refuse(c.ports, record)?;
    let update = params.previous.as_ref();
    // A node with a local cache hands over only between releases whose cache shapes do, and a
    // node whose file leaves the socket out only between releases that default it.
    let mut needed = vec![HANDOFF_CAPABILITY];
    let mut restart = params.declared_restart;
    // The node holds a mount: its file has an `fs` interface. A restart would drop the mount
    // under running jobs, so such an update is a hand-over or it is refused.
    let mut mount = false;
    if update.is_some() && !params.declared_restart {
        let shape = read_shape(c, params)?;
        if shape.fs {
            needed.push(HANDOFF_SERVICE_CAPABILITY);
            mount = true;
        }
        if shape.cache {
            needed.push(CACHE_HANDOFF_CAPABILITY);
        }
        if !shape.socket_named {
            needed.push(HANDOFF_STATE_DIR_CAPABILITY);
        }
        // Two instances must not share a dir hot tier: its lease is atomic in one process only.
        restart = shape.dir_hot;
    }
    // The staged binary is asked on a first install too: it proves `bin/tierstore` is there and
    // runs.
    let staged = c.ports.host.capabilities(&params.staged_release)?;
    let mode = match update {
        None => None,
        Some(_) if restart => {
            if mount {
                return Err(refuse_restart(
                    "its hot tier is a directory, which two instances must not share".to_owned(),
                ));
            }
            Some(Mode::Restart)
        }
        Some(previous) => {
            let list = c.ports.host.capabilities(&previous.release)?;
            let can = |list: &[String]| {
                needed
                    .iter()
                    .all(|need| list.iter().any(|word| word == need))
            };
            let mode = if can(&staged) && can(&list) {
                Mode::Handover
            } else if mount {
                let lacking = [
                    ("staged", &params.staged_release, &staged),
                    ("previous", &previous.release, &list),
                ]
                .into_iter()
                .filter_map(|(which, release, words)| {
                    let missing: Vec<&str> = needed
                        .iter()
                        .copied()
                        .filter(|need| !words.iter().any(|word| word == need))
                        .collect();
                    (!missing.is_empty()).then(|| {
                        format!(
                            "the {which} release {} lacks {}",
                            release.display(),
                            missing.join(", ")
                        )
                    })
                })
                .collect::<Vec<_>>();
                return Err(refuse_restart(lacking.join("; ")));
            } else {
                Mode::Restart
            };
            Some(mode)
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
    // The instances are what systemd says now, also over a record of an update in flight: a
    // `cutover` that failed before `swapped` may have started its instance, which then took the
    // sockets, and the record of that update still names the other one as live. A run again
    // with nothing started since finds the same instances.
    let live = serving(c, update.is_some(), record)?;
    let next = live.map_or(Instance::A, Instance::other);
    let mut fresh = Record::idle(live);
    fresh.updating = true;
    fresh.from = update.map(|previous| previous.release.clone());
    fresh.to = Some(params.staged_release.clone());
    fresh.version = Some(params.target_version.clone());
    fresh.next = Some(next);
    fresh.mode = mode;
    c.save(&fresh)?;
    Ok(Done::ok(format!(
        "{done_note}prepared {}: {}, serving {}, next {}",
        params.target_version,
        mode.map_or("first install", Mode::as_str),
        live.map_or_else(|| "none".to_owned(), unit_name),
        unit_name(next),
    )))
}

/// The refusal of an update of a node that holds a mount and would be a restart, for the
/// reason `why`.
fn refuse_restart(why: String) -> Done {
    Done::fail(
        Reason::ModeUnsupported,
        format!(
            "the node holds a mount (its file has an fs interface) and this update would be a \
             restart: {why}. A restart would drop the mount under running jobs; mode = \"restart\" \
             in the params overrides this for a node whose jobs are drained"
        ),
    )
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

/// What `prepare` reads of the node's configuration to choose the way of an update.
struct Shape {
    /// The configuration has a `[local_cache]` table (a build node, or a storage node with a
    /// cache).
    cache: bool,
    /// The configuration names `[server] handoff_socket`; if not, the release's default in the
    /// service's state directory is the socket.
    socket_named: bool,
    /// The hot tier is a directory (`[hot_store] kind = "dir"`), which two instances must not
    /// share: the update is a restart.
    dir_hot: bool,
    /// The configuration has an `fs` interface: the node holds a mount.
    fs: bool,
}

/// Reads the node's configuration for its shape. A `handoff_socket` that is an empty string is
/// refused, as it names no socket; one that is left out is the release's default.
fn read_shape(c: &Ctx<'_, '_>, params: &Params) -> Result<Shape, Done> {
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
    let dir_hot = table
        .get(HOT_STORE_TABLE)
        .and_then(toml::Value::as_table)
        .and_then(|hot| hot.get("kind"))
        .and_then(toml::Value::as_str)
        .is_some_and(|kind| kind == DIR_TIER_KIND);
    let socket = table
        .get("server")
        .and_then(toml::Value::as_table)
        .and_then(|server| server.get(HANDOFF_SOCKET_KEY));
    if !dir_hot && socket.is_some_and(|socket| socket.as_str().is_none_or(str::is_empty)) {
        return Err(Done::fail(
            Reason::HandoffNotConfigured,
            format!(
                "{} has an empty or non-string [server] {HANDOFF_SOCKET_KEY}: give the path of \
                 the socket, or leave the key out for the default in the service's state \
                 directory",
                path.display()
            ),
        ));
    }
    Ok(Shape {
        cache: table.contains_key(LOCAL_CACHE_TABLE),
        socket_named: socket.is_some(),
        dir_hot,
        fs: fs_interface(&text).is_some(),
    })
}

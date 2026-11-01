//! What the fake `nft` and `systemctl` (fake/main.rs) keep in their `state` directory, read back:
//! the active units and the loaded tables.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tierstore::server::update_hook::{FORWARD_TABLE, redirect_target};

/// An active slot unit: the order of its start and the release it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub seq: u64,
    pub release: PathBuf,
}

/// The active slot units, by port.
pub fn units(world: &Path) -> BTreeMap<u16, Unit> {
    let Ok(entries) = fs::read_dir(world.join("units")) else {
        return BTreeMap::new();
    };
    let mut units = BTreeMap::new();
    for entry in entries {
        let entry = entry.expect("a unit");
        let name = entry.file_name().to_string_lossy().into_owned();
        let port = name
            .strip_prefix("tierstore@")
            .and_then(|rest| rest.strip_suffix(".service"))
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("{name} is not a slot unit"));
        let text = fs::read_to_string(entry.path()).expect("a unit file");
        let mut lines = text.lines();
        let seq = lines.next().and_then(|seq| seq.parse().ok()).expect("seq");
        let release = PathBuf::from(lines.next().expect("release"));
        units.insert(port, Unit { seq, release });
    }
    units
}

/// The loaded tables, by name: their rules, one per line.
pub fn tables(world: &Path) -> BTreeMap<String, Vec<String>> {
    let Ok(entries) = fs::read_dir(world.join("tables")) else {
        return BTreeMap::new();
    };
    entries
        .map(|entry| {
            let entry = entry.expect("a table");
            let text = fs::read_to_string(entry.path()).expect("a table file");
            let rules = text.lines().map(str::to_owned).collect();
            (entry.file_name().to_string_lossy().into_owned(), rules)
        })
        .collect()
}

/// The port the redirect table names, as the hook reads a listing of it.
pub fn redirect(world: &Path) -> Option<u16> {
    tables(world)
        .get(FORWARD_TABLE)
        .and_then(|rules| redirect_target(&rules.join("\n")))
}

/// The lines of a log of the world (`calls`, `effects`); none when there is none yet.
pub fn lines(world: &Path, log: &str) -> Vec<String> {
    fs::read_to_string(world.join(log))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

//! The group id of a group name (`fs.client_group`), looked up through the system: the lookup
//! `getgrnam` makes, so that a group of a Mac's directory service, where the macOS package makes its
//! account's group and which `/etc/group` does not list, is found as one of `/etc/group` is.
//!
//! The process asks the system's own tool for the group, since a call of `getgrnam` itself would
//! need unsafe code: `dscacheutil -q group -a name <name>` on macOS (Open Directory, the search
//! `getgrnam` makes there) and `getent group <name>` elsewhere (the name service switch). The tool is
//! run through a runner ([`lookup`]) and what it said is read by [`answer`], so the tests run both
//! with no system.

use std::process::Output;

/// The status with which `getent` says that the database has no such key.
const GETENT_ABSENT: i32 = 2;

/// The answer of a lookup of a group name: its id, none for a name the system does not have, or
/// why the system could not be asked.
pub(in crate::server) type Found = Result<Option<u32>, String>;

/// The id of the group `name` as this system knows it.
pub(in crate::server) fn system(name: &str) -> Found {
    lookup(std::env::consts::OS, name, &spawn)
}

/// What runs a program with its arguments and gives its output.
type Run<'a> = &'a dyn Fn(&str, &[&str]) -> std::io::Result<Output>;

/// The id of the group `name` on the system `os` (as `std::env::consts::OS` names it), asked of its
/// tool through `run`.
fn lookup(os: &str, name: &str, run: Run<'_>) -> Found {
    let tool = Tool::of(os);
    let mut arguments = tool.arguments().to_vec();
    arguments.push(name);
    answer(tool, tool.program(), run(tool.program(), &arguments))
}

/// Runs `program` with `arguments`, with no standard input, and gives its output.
fn spawn(program: &str, arguments: &[&str]) -> std::io::Result<Output> {
    std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .output()
}

/// The tool a lookup runs: its output is read by the rules of that tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    /// `dscacheutil`, of macOS: `name: <name>`, `password: *`, `gid: <id>` and `users: ...` lines
    /// for a group, nothing for a name it does not have, status 0 either way.
    Dscacheutil,
    /// `getent`: one line `name:password:gid:members`, or status 2 for a name it does not have.
    Getent,
}

impl Tool {
    /// The tool of the system `os`.
    fn of(os: &str) -> Self {
        if os == "macos" {
            Self::Dscacheutil
        } else {
            Self::Getent
        }
    }

    /// The program.
    fn program(self) -> &'static str {
        match self {
            Self::Dscacheutil => "/usr/bin/dscacheutil",
            Self::Getent => "/usr/bin/getent",
        }
    }

    /// Its arguments before the name.
    fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Dscacheutil => &["-q", "group", "-a", "name"],
            Self::Getent => &["group"],
        }
    }
}

/// What `program`, the `tool` of a lookup, said: `ran` is its output, or why it did not run.
fn answer(tool: Tool, program: &str, ran: std::io::Result<Output>) -> Found {
    let output = ran.map_err(|why| format!("{program} could not be run: {why}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let absent = tool == Tool::Getent && output.status.code() == Some(GETENT_ABSENT);
    if absent {
        return Ok(None);
    }
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{program} failed ({}): {}",
            output.status,
            said.trim()
        ));
    }
    let gid = match tool {
        Tool::Dscacheutil => stdout
            .lines()
            .find_map(|line| line.strip_prefix("gid:"))
            .map(str::trim),
        Tool::Getent => stdout
            .lines()
            .next()
            .and_then(|line| line.split(':').nth(2)),
    };
    match gid {
        None if stdout.trim().is_empty() => Ok(None),
        None => Err(format!("{program} said no group id: {}", stdout.trim())),
        Some(gid) => gid
            .parse()
            .map(Some)
            .map_err(|_| format!("{program} said the group id {gid:?}, which is not a number")),
    }
}

#[cfg(test)]
#[path = "groups_tests.rs"]
mod tests;

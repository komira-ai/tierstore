//! The id of a group or of a user by name (`fs.client_group`, `fs.owner`), looked up through the
//! system: the lookup `getgrnam` or `getpwnam` makes, so that an account of a Mac's directory
//! service, where the macOS package makes its account and its group and which `/etc/passwd` and
//! `/etc/group` do not list, is found as one of those files is.
//!
//! The process asks the system's own tool, since a call of `getgrnam` itself would need unsafe
//! code: `dscacheutil -q group|user -a name <name>` on macOS (Open Directory, the search `getgrnam`
//! and `getpwnam` make there) and `getent group|passwd <name>` elsewhere (the name service switch).
//! The tool is run through a runner ([`lookup`]) and what it said is read by [`answer`], so the
//! tests run both with no system.

use std::process::Output;

/// The status with which `getent` says that the database has no such key.
const GETENT_ABSENT: i32 = 2;

/// What a name is looked up in: the groups of the system or its users.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Database {
    /// The group database: a name gives a group id.
    Group,
    /// The user database: a name gives a user id.
    User,
}

impl Database {
    /// What the database holds, as a word in a message.
    pub(in crate::server) fn noun(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::User => "user",
        }
    }

    /// The line of `dscacheutil`'s answer that holds the id.
    fn dscacheutil_key(self) -> &'static str {
        match self {
            Self::Group => "gid:",
            Self::User => "uid:",
        }
    }
}

/// The answer of a lookup of a name: its id, none for a name the system does not have, or why the
/// system could not be asked.
pub(in crate::server) type Found = Result<Option<u32>, String>;

/// The id of the name `name` in `database` as this system knows it.
pub(in crate::server) fn system(database: Database, name: &str) -> Found {
    lookup(std::env::consts::OS, database, name, &spawn)
}

/// What runs a program with its arguments and gives its output.
type Run<'a> = &'a dyn Fn(&str, &[&str]) -> std::io::Result<Output>;

/// The id of the name `name` in `database` on the system `os` (as `std::env::consts::OS` names it),
/// asked of its tool through `run`.
fn lookup(os: &str, database: Database, name: &str, run: Run<'_>) -> Found {
    let tool = Tool::of(os);
    let mut arguments = tool.arguments(database).to_vec();
    arguments.push(name);
    answer(
        tool,
        database,
        tool.program(),
        run(tool.program(), &arguments),
    )
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
    /// for a group, `name:`, `password:`, `uid: <id>`, `gid:` and more for a user, nothing for a
    /// name it does not have, status 0 either way.
    Dscacheutil,
    /// `getent`: one line `name:password:gid:members` for a group or `name:password:uid:gid:...`
    /// for a user, or status 2 for a name it does not have.
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

    /// Its arguments before the name, for `database`.
    fn arguments(self, database: Database) -> &'static [&'static str] {
        match (self, database) {
            (Self::Dscacheutil, Database::Group) => &["-q", "group", "-a", "name"],
            (Self::Dscacheutil, Database::User) => &["-q", "user", "-a", "name"],
            (Self::Getent, Database::Group) => &["group"],
            (Self::Getent, Database::User) => &["passwd"],
        }
    }
}

/// What `program`, the `tool` of a lookup in `database`, said: `ran` is its output, or why it did
/// not run.
fn answer(tool: Tool, database: Database, program: &str, ran: std::io::Result<Output>) -> Found {
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
    let noun = database.noun();
    let id = match tool {
        Tool::Dscacheutil => stdout
            .lines()
            .find_map(|line| line.strip_prefix(database.dscacheutil_key()))
            .map(str::trim),
        Tool::Getent => stdout
            .lines()
            .next()
            .and_then(|line| line.split(':').nth(2)),
    };
    match id {
        None if stdout.trim().is_empty() => Ok(None),
        None => Err(format!("{program} said no {noun} id: {}", stdout.trim())),
        Some(id) => id
            .parse()
            .map(Some)
            .map_err(|_| format!("{program} said the {noun} id {id:?}, which is not a number")),
    }
}

#[cfg(test)]
#[path = "groups_tests.rs"]
mod tests;

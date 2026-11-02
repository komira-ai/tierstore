//! The stand-in programs of the hook's conformance test (`tests/update_hook`): `nft`,
//! `systemctl` and the `tierstore` binary of a staged release. One program, the
//! `tierstore-hook-fake` binary, is each of them by the name it is run as; the test puts copies
//! of it in place under those names. Built only with the `hook-conformance` feature, which only
//! this crate's own tests turn on: no release has it.
//!
//! `nft` and `systemctl` keep their world in the directory `state` beside them ([`World`]), and
//! nothing else: the hook gives its programs an empty environment, so the test speaks to them
//! through files only.
//!
//! - `calls`: one line per call, the name and the arguments (`nft list table inet <t>`).
//! - `effects`: one line per change of the world, and none for a call that changed nothing:
//!   `set <table> <rules>`, `delete <table>`, `start <unit>`, `stop <unit>`.
//! - `tables/<name>`: a loaded table of the `inet` family, one rule or chain per line.
//! - `units/<unit>`: an active unit: the order of its start (from `seq`) and the release it runs.
//! - `exec`: the binary of a slot unit, `%i` for the port; `condition`, when it is there, the
//!   unit's `ConditionPathExists=` (else the binary's path, as the slot template has it).
//! - `fail`: calls, one per line, each answered once with exit 1 and no change.
//! - `kill-after`: a number `n`. After the n-th call has been logged and done, the program asks
//!   to kill its parent, the hook: a crash at that boundary.
//! - `hold-after`: a number `n`. After the n-th call has been logged and done, the program opens
//!   the pipe `held` and then waits on the pipe `release`: the hook is held in the middle of its
//!   step, with its lock, until the test lets it go. No side sleeps.
//!
//! What it answers is what the real program answers in the cases the hook reads: `nft` says `No
//! such file or directory` for an absent table and applies a ruleset whole or not at all;
//! `systemctl start` exits 0 and starts nothing when the condition fails, as systemd does. The
//! release answers `--capabilities` with the lines of `capabilities` beside it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, FileType, Mode, mknodat};

use super::ruleset::{FORWARD_TABLE, redirect_target};

/// The name of the fake `nft`.
pub const NFT: &str = "nft";
/// The name of the fake `systemctl`.
pub const SYSTEMCTL: &str = "systemctl";
/// The name of the binary of a fake staged release.
pub const RELEASE: &str = "tierstore";
/// The directory of the world, beside the fake programs.
pub const WORLD: &str = "state";

/// What a call answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The exit status.
    pub code: u8,
    /// What it writes on standard output.
    pub stdout: String,
    /// What it writes on standard error.
    pub stderr: String,
}

impl Reply {
    fn ok(stdout: impl Into<String>) -> Reply {
        Reply {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn fail(code: u8, stderr: impl Into<String>) -> Reply {
        Reply {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// How a run of the fake ends: its answer, and whether it then kills its parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    /// The answer.
    pub reply: Reply,
    /// Whether the program kills its parent before it exits.
    pub kill_parent: bool,
}

/// An active slot unit: the order of its start and the release it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// The order of its start, from 1.
    pub seq: u64,
    /// The release its binary was in when it started.
    pub release: PathBuf,
}

/// What `nft` says of a table that is not there: the text of ENOENT.
fn absent(verb: &str, table: &str) -> Reply {
    Reply::fail(
        1,
        format!("Error: No such file or directory\n{verb} table inet {table}\n"),
    )
}

/// The port of a slot unit's name, `tierstore@<port>.service`.
fn slot_port(unit: &str) -> Option<&str> {
    unit.strip_prefix("tierstore@")?.strip_suffix(".service")
}

/// The world of `nft` and `systemctl`: a directory.
pub struct World {
    dir: PathBuf,
}

impl World {
    /// The world in `dir`, which must exist.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> World {
        World { dir: dir.into() }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.file(name)).ok()
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.file(name), text).expect("write to the world");
    }

    fn append(&self, name: &str, line: &str) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file(name))
            .expect("append to the world");
        writeln!(file, "{line}").expect("append to the world");
    }

    /// The number a file of the world holds, if it holds one.
    fn number(&self, name: &str) -> Option<usize> {
        self.read(name)?.trim().parse().ok()
    }

    /// The lines of a log (`calls`, `effects`); none when there is none yet.
    #[must_use]
    pub fn lines(&self, log: &str) -> Vec<String> {
        self.read(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Empties the two logs.
    pub fn clear_logs(&self) {
        for log in ["calls", "effects"] {
            let _ = fs::remove_file(self.file(log));
        }
    }

    /// Sets the binary of a slot unit (`%i` the port).
    pub fn set_exec(&self, template: &str) {
        self.write("exec", template);
    }

    /// Sets the slot unit's `ConditionPathExists=` (`%i` the port).
    pub fn set_condition(&self, template: &str) {
        self.write("condition", template);
    }

    /// Kills the hook after the `n`-th call from now on; `None` stops that.
    pub fn kill_after(&self, n: Option<usize>) {
        match n {
            Some(n) => self.write("kill-after", &(self.lines("calls").len() + n).to_string()),
            None => {
                let _ = fs::remove_file(self.file("kill-after"));
            }
        }
    }

    /// Answers the next `call` (the words the log would show) with exit 1 and no change, once.
    pub fn fail_once(&self, call: &str) {
        self.append("fail", call);
    }

    /// Holds the hook after the `n`-th call from now on, until [`World::release`]: makes the two
    /// pipes the handshake goes over.
    ///
    /// # Panics
    /// The pipes cannot be made.
    pub fn hold_after(&self, n: usize) {
        for pipe in ["held", "release"] {
            let path = self.file(pipe);
            let _ = fs::remove_file(&path);
            mknodat(CWD, &path, FileType::Fifo, Mode::from_raw_mode(0o600), 0)
                .expect("make a pipe");
        }
        self.write("hold-after", &(self.lines("calls").len() + n).to_string());
    }

    /// Returns once the program holding the hook has said so.
    ///
    /// # Panics
    /// The pipe cannot be read.
    pub fn wait_held(&self) {
        let mut said = Vec::new();
        fs::File::open(self.file("held"))
            .and_then(|mut pipe| pipe.read_to_end(&mut said))
            .expect("read the pipe held");
    }

    /// Lets the held hook go on.
    ///
    /// # Panics
    /// The pipe cannot be opened.
    pub fn release(&self) {
        fs::OpenOptions::new()
            .write(true)
            .open(self.file("release"))
            .expect("open the pipe release");
    }

    /// The side of the handshake the program holding the hook takes.
    fn hold(&self) {
        fs::OpenOptions::new()
            .write(true)
            .open(self.file("held"))
            .and_then(|mut pipe| pipe.write_all(b"held\n"))
            .expect("write the pipe held");
        let mut rest = Vec::new();
        fs::File::open(self.file("release"))
            .and_then(|mut pipe| pipe.read_to_end(&mut rest))
            .expect("read the pipe release");
    }

    /// A reboot: no unit runs and no table is loaded.
    pub fn reboot(&self) {
        for dir in ["units", "tables"] {
            let _ = fs::remove_dir_all(self.file(dir));
        }
    }

    /// The active slot units, by port. A file that is not a slot unit's is left out.
    #[must_use]
    pub fn units(&self) -> BTreeMap<u16, Unit> {
        let mut units = BTreeMap::new();
        for entry in fs::read_dir(self.file("units"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            let mut lines = text.lines();
            let found = (
                slot_port(&name).and_then(|port| port.parse().ok()),
                lines.next().and_then(|seq| seq.parse().ok()),
                lines.next(),
            );
            if let (Some(port), Some(seq), Some(release)) = found {
                let release = PathBuf::from(release);
                units.insert(port, Unit { seq, release });
            }
        }
        units
    }

    /// The loaded tables, by name: their rules, one per line.
    #[must_use]
    pub fn tables(&self) -> BTreeMap<String, Vec<String>> {
        fs::read_dir(self.file("tables"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let rules = self.table(&name).unwrap_or_default();
                (name, rules)
            })
            .collect()
    }

    /// The port the redirect table names, as the hook reads a listing of it.
    #[must_use]
    pub fn redirect(&self) -> Option<u16> {
        self.table(FORWARD_TABLE)
            .and_then(|rules| redirect_target(&rules.join("\n")))
    }

    fn table(&self, name: &str) -> Option<Vec<String>> {
        self.read(&format!("tables/{name}"))
            .map(|text| text.lines().map(str::to_owned).collect())
    }

    fn set_table(&self, name: &str, rules: &[String]) {
        fs::create_dir_all(self.file("tables")).expect("tables");
        let text: String = rules.iter().map(|rule| format!("{rule}\n")).collect();
        self.write(&format!("tables/{name}"), &text);
        self.append("effects", &format!("set {name} {}", rules.join(" ; ")));
    }

    fn unit(&self, unit: &str) -> PathBuf {
        self.file(&format!("units/{unit}"))
    }

    /// Whether `call` is scripted to fail; the script is used up.
    fn scripted_failure(&self, call: &str) -> bool {
        let lines = self.lines("fail");
        let Some(at) = lines.iter().position(|line| line == call) else {
            return false;
        };
        let rest: String = lines
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != at)
            .map(|(_, line)| format!("{line}\n"))
            .collect();
        self.write("fail", &rest);
        true
    }

    /// One call of `program` (`nft` or `systemctl`) with `args`; `input` is its standard input.
    pub fn call(&self, program: &str, args: &[&str], input: &mut dyn Read) -> Done {
        let line = format!("{program} {}", args.join(" "));
        self.append("calls", &line);
        let calls = self.lines("calls").len();
        let reply = if self.scripted_failure(&line) {
            Reply::fail(1, format!("Error: {line}: a failure the test asked for\n"))
        } else if program == NFT {
            self.nft(args, input)
        } else {
            self.systemctl(args)
        };
        if self.number("hold-after") == Some(calls) {
            self.hold();
        }
        Done {
            reply,
            kill_parent: self.number("kill-after") == Some(calls),
        }
    }

    fn nft(&self, args: &[&str], input: &mut dyn Read) -> Reply {
        match args {
            ["-f", "-"] => {
                let mut text = String::new();
                if input.read_to_string(&mut text).is_err() {
                    return Reply::fail(1, "Error: cannot read the ruleset\n");
                }
                self.apply(&text)
            }
            ["list", "table", "inet", table] => match self.table(table) {
                Some(rules) => {
                    let body: String = rules.iter().map(|rule| format!("\t{rule}\n")).collect();
                    Reply::ok(format!("table inet {table} {{\n{body}}}\n"))
                }
                None => absent("list", table),
            },
            ["delete", "table", "inet", table] => {
                if self.table(table).is_none() {
                    return absent("delete", table);
                }
                fs::remove_file(self.file(&format!("tables/{table}"))).expect("delete a table");
                self.append("effects", &format!("delete {table}"));
                Reply::ok("")
            }
            _ => Reply::fail(1, "Error: syntax error\n"),
        }
    }

    /// Applies a ruleset in one transaction: every line is read before anything changes, and a
    /// line it does not know changes nothing.
    fn apply(&self, text: &str) -> Reply {
        let mut staged: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let words: Vec<&str> = line.split_whitespace().collect();
            let (verb, table) = match words.as_slice() {
                ["add" | "flush", "table", "inet", table] => (words[0], *table),
                ["add", "chain" | "rule", "inet", table, _, ..] => (words[1], *table),
                _ => return Reply::fail(1, format!("Error: syntax error: {line}\n")),
            };
            let rules = staged
                .entry(table.to_owned())
                .or_insert_with(|| self.table(table));
            if verb == "add" {
                if rules.is_none() {
                    *rules = Some(Vec::new());
                }
                continue;
            }
            let Some(rules) = rules.as_mut() else {
                return absent(verb, table);
            };
            if verb == "flush" {
                rules.clear();
            } else {
                rules.push(format!("{verb} {}", words[4..].join(" ")));
            }
        }
        for (table, rules) in staged {
            let rules = rules.unwrap_or_default();
            if self.table(&table).as_ref() != Some(&rules) {
                self.set_table(&table, &rules);
            }
        }
        Reply::ok("")
    }

    fn systemctl(&self, args: &[&str]) -> Reply {
        match args {
            ["start", "--", unit] => self.start(unit),
            ["stop", "--", unit] => {
                if self.unit(unit).exists() {
                    fs::remove_file(self.unit(unit)).expect("stop a unit");
                    self.append("effects", &format!("stop {unit}"));
                }
                Reply::ok("")
            }
            _ => Reply::fail(1, "Unknown command verb.\n"),
        }
    }

    /// Starts a slot unit: skipped (exit 0, nothing active) when its condition fails, a failure
    /// when its binary cannot be found, as systemd has it.
    fn start(&self, unit: &str) -> Reply {
        if self.unit(unit).exists() {
            return Reply::ok("");
        }
        let Some(port) = slot_port(unit) else {
            return Reply::fail(
                5,
                format!("Failed to start {unit}: Unit {unit} not found.\n"),
            );
        };
        let exec = self
            .read("exec")
            .unwrap_or_default()
            .trim()
            .replace("%i", port);
        let condition = self
            .read("condition")
            .map_or_else(|| exec.clone(), |text| text.trim().replace("%i", port));
        if !Path::new(&condition).exists() {
            return Reply::ok("");
        }
        let Some(release) = fs::canonicalize(&exec)
            .ok()
            .and_then(|binary| binary.parent().map(Path::to_path_buf))
        else {
            return Reply::fail(
                1,
                format!(
                    "Job for {unit} failed because the control process exited with error code.\n"
                ),
            );
        };
        let seq = self.number("seq").unwrap_or(0) + 1;
        self.write("seq", &seq.to_string());
        fs::create_dir_all(self.file("units")).expect("units");
        let text = format!("{seq}\n{}\n", release.display());
        fs::write(self.unit(unit), text).expect("start a unit");
        self.append("effects", &format!("start {unit}"));
        Reply::ok("")
    }
}

/// The binary of a staged release in `dir`: `--capabilities` prints the file beside it.
#[must_use]
pub fn release(dir: &Path, args: &[&str]) -> Reply {
    match args {
        ["--capabilities"] => {
            Reply::ok(fs::read_to_string(dir.join("capabilities")).unwrap_or_default())
        }
        _ => Reply::fail(2, "error: unexpected argument\n"),
    }
}

/// A run of the program at `exe` with `args`: the program it is by its name, its world the
/// directory [`WORLD`] beside it, `input` its standard input.
pub fn run(exe: &Path, args: &[String], input: &mut dyn Read) -> Done {
    let dir = exe.parent().unwrap_or(Path::new("/"));
    let name = exe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match name.as_str() {
        NFT | SYSTEMCTL => World::new(dir.join(WORLD)).call(&name, &args, input),
        RELEASE => Done {
            reply: release(dir, &args),
            kill_parent: false,
        },
        _ => Done {
            reply: Reply::fail(2, format!("{name}: not a program this stands in for\n")),
            kill_parent: false,
        },
    }
}

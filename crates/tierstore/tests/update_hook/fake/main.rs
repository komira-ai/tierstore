//! The stand-in programs of the update hook's conformance test (tests/update_hook): `nft`,
//! `systemctl` and the `tierstore` binary of a staged release. One program is each of them by the
//! name it is run as; the test puts copies of it in place under those names.
//!
//! `nft` and `systemctl` keep their world in the directory `state` beside them, and nothing
//! else: the hook gives its programs an empty environment, so the test speaks to them through
//! files only.
//!
//! - `calls`: one line per call, the name and the arguments (`nft list table inet <t>`).
//! - `effects`: one line per change of the world, and none for a call that changed nothing:
//!   `set <table> <rules>`, `delete <table>`, `start <unit>`, `stop <unit>`.
//! - `tables/<name>`: a loaded table of the `inet` family, one rule or chain per line.
//! - `units/<unit>`: an active unit: the order of its start (from `seq`) and the release it runs.
//! - `exec`: the binary of a slot unit, `%i` for the port; `condition`, when it is there, the
//!   unit's `ConditionPathExists=` (else the binary's path, as the slot template has it).
//! - `kill-after`: a number `n`. After the n-th call has been logged and done, the program
//!   kills its parent, the hook, with SIGKILL: a crash at that boundary.
//!
//! What it answers is what the real program answers in the cases the hook reads: `nft` says `No
//! such file or directory` for an absent table and applies a ruleset whole or not at all;
//! `systemctl start` exits 0 and starts nothing when the condition fails, as systemd does. The
//! release answers `--capabilities` with the lines of `capabilities` beside it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rustix::process::{Signal, getppid, kill_process};

/// What a call ends with.
struct Reply {
    code: u8,
    stdout: String,
    stderr: String,
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

/// The world of `nft` and `systemctl`: the directory `state`.
struct World {
    dir: PathBuf,
}

impl World {
    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.file(name)).ok()
    }

    fn append(&self, name: &str, line: &str) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file(name))
            .expect("append to the world");
        writeln!(file, "{line}").expect("append to the world");
    }

    /// Logs a call and returns how many calls there have been, this one included.
    fn log(&self, line: &str) -> usize {
        self.append("calls", line);
        self.read("calls").unwrap_or_default().lines().count()
    }

    fn effect(&self, line: &str) {
        self.append("effects", line);
    }

    /// Kills the parent when this is the call `kill-after` names.
    fn kill_parent_after(&self, calls: usize) {
        let Some(text) = self.read("kill-after") else {
            return;
        };
        if text.trim().parse() == Ok(calls)
            && let Some(parent) = getppid()
        {
            let _ = kill_process(parent, Signal::KILL);
        }
    }

    fn table(&self, name: &str) -> Option<Vec<String>> {
        self.read(&format!("tables/{name}"))
            .map(|text| text.lines().map(str::to_owned).collect())
    }

    fn set_table(&self, name: &str, rules: &[String]) {
        fs::create_dir_all(self.file("tables")).expect("tables");
        let text: String = rules.iter().map(|rule| format!("{rule}\n")).collect();
        fs::write(self.file(&format!("tables/{name}")), text).expect("write a table");
        self.effect(&format!("set {name} {}", rules.join(" ; ")));
    }

    fn unit(&self, unit: &str) -> PathBuf {
        self.file(&format!("units/{unit}"))
    }

    fn nft(&self, args: &[&str]) -> Reply {
        match args {
            ["-f", "-"] => {
                let mut text = String::new();
                if io::stdin().read_to_string(&mut text).is_err() {
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
                self.effect(&format!("delete {table}"));
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
                    self.effect(&format!("stop {unit}"));
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
        let Some(port) = unit
            .strip_prefix("tierstore@")
            .and_then(|rest| rest.strip_suffix(".service"))
        else {
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
        let seq = self
            .read("seq")
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        fs::write(self.file("seq"), seq.to_string()).expect("seq");
        fs::create_dir_all(self.file("units")).expect("units");
        fs::write(self.unit(unit), format!("{seq}\n{}\n", release.display())).expect("start");
        self.effect(&format!("start {unit}"));
        Reply::ok("")
    }
}

/// What `nft` says of a table that is not there: the text of ENOENT.
fn absent(verb: &str, table: &str) -> Reply {
    Reply::fail(
        1,
        format!("Error: No such file or directory\n{verb} table inet {table}\n"),
    )
}

/// The binary of a staged release: `--capabilities` prints the file beside it.
fn release(dir: &Path, args: &[&str]) -> Reply {
    match args {
        ["--capabilities"] => {
            Reply::ok(fs::read_to_string(dir.join("capabilities")).unwrap_or_default())
        }
        _ => Reply::fail(2, "error: unexpected argument\n"),
    }
}

fn main() -> ExitCode {
    let exe = std::env::current_exe().expect("the path of this program");
    let dir = exe.parent().expect("a directory").to_path_buf();
    let name = exe
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let owned: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let world = World {
        dir: dir.join("state"),
    };
    let reply = match name.as_str() {
        "nft" | "systemctl" => {
            let calls = world.log(&format!("{name} {}", args.join(" ")));
            let reply = if name == "nft" {
                world.nft(&args)
            } else {
                world.systemctl(&args)
            };
            world.kill_parent_after(calls);
            reply
        }
        "tierstore" => release(&dir, &args),
        _ => Reply::fail(2, format!("{name}: not a program this stands in for\n")),
    };
    let _ = io::stdout().write_all(reply.stdout.as_bytes());
    let _ = io::stderr().write_all(reply.stderr.as_bytes());
    ExitCode::from(reply.code)
}

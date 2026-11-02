//! The `tierstore-handover-double` binary: the stand-ins of the hand-over test under load
//! (tests/handover), one program that is each of them by the name it is run as. The test puts
//! copies of it in place under those names; each finds what it needs in files beside it, since
//! the hook gives its programs an environment of its own.
//!
//! - `systemctl`: sends its arguments to the test's service manager over the Unix socket
//!   `mgr` beside it and answers what the manager answers. The test owns the server
//!   processes; this only asks it to start or stop one (services.rs).
//! - `nft`: runs the real `nft` (the path in `real-nft` beside it) with its arguments and standard
//!   input and answers what it answers. When the file `kill-after` beside it holds a text that
//!   the ruleset on its standard input contains, and the call succeeded, it removes the file and
//!   kills its parent, the hook, with SIGKILL: a crash right after that ruleset was applied.
//! - `tierstore`, the binary of a staged release: `--capabilities` prints the file
//!   `capabilities` beside it; anything else replaces this process with the real binary (the path
//!   in `real` beside it) and the same arguments, so that the service manager's signals reach
//!   the server itself.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use rustix::process::{Signal, getppid, kill_process};

/// The text of the file `name` beside this program, trimmed.
fn beside(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name))
        .ok()
        .map(|text| text.trim().to_owned())
}

fn systemctl(dir: &Path, args: &[String]) -> ExitCode {
    let mut socket = match UnixStream::connect(dir.join("mgr")) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("systemctl: the test's service manager: {error}");
            return ExitCode::from(1);
        }
    };
    let asked = socket
        .write_all(args.join("\n").as_bytes())
        .and_then(|()| socket.shutdown(std::net::Shutdown::Write));
    let mut answer = String::new();
    if let Err(error) = asked.and_then(|()| socket.read_to_string(&mut answer).map(drop)) {
        eprintln!("systemctl: the test's service manager: {error}");
        return ExitCode::from(1);
    }
    let (code, said) = answer.split_once('\n').unwrap_or((&answer, ""));
    eprint!("{said}");
    ExitCode::from(code.parse::<u8>().unwrap_or(1))
}

fn nft(dir: &Path, args: &[String]) -> ExitCode {
    let Some(real) = beside(dir, "real-nft") else {
        eprintln!("nft: no real-nft beside the double");
        return ExitCode::from(1);
    };
    let mut ruleset = Vec::new();
    if let Err(error) = io::stdin().read_to_end(&mut ruleset) {
        eprintln!("nft: standard input: {error}");
        return ExitCode::from(1);
    }
    let child = Command::new(&real)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let output = child.and_then(|mut child| {
        let mut input = child.stdin.take().expect("a pipe");
        let written = input.write_all(&ruleset);
        drop(input);
        written.and_then(|()| child.wait_with_output())
    });
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            eprintln!("nft: {real}: {error}");
            return ExitCode::from(1);
        }
    };
    let _ = io::stdout().write_all(&output.stdout);
    let _ = io::stderr().write_all(&output.stderr);
    let marker = beside(dir, "kill-after").filter(|marker| !marker.is_empty());
    let applied = String::from_utf8_lossy(&ruleset);
    if let Some(marker) = marker
        && output.status.success()
        && applied.contains(&marker)
    {
        let _ = fs::remove_file(dir.join("kill-after"));
        if let Some(parent) = getppid() {
            let _ = kill_process(parent, Signal::KILL);
        }
    }
    let code = output.status.code().unwrap_or(1);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn release(dir: &Path, args: &[String]) -> ExitCode {
    if args == ["--capabilities"] {
        print!(
            "{}",
            fs::read_to_string(dir.join("capabilities")).unwrap_or_default()
        );
        return ExitCode::SUCCESS;
    }
    let Some(real) = beside(dir, "real") else {
        eprintln!("tierstore: no real binary beside the launcher");
        return ExitCode::from(1);
    };
    let error = Command::new(&real).args(args).exec();
    eprintln!("tierstore: {real}: {error}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let exe: PathBuf = std::env::current_exe().expect("the path of this program");
    let dir = exe.parent().unwrap_or(Path::new("/")).to_owned();
    let name = exe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match name.as_str() {
        "systemctl" => systemctl(&dir, &args),
        "nft" => nft(&dir, &args),
        "tierstore" => release(&dir, &args),
        _ => {
            eprintln!("{name}: not a program this stands in for");
            ExitCode::from(2)
        }
    }
}

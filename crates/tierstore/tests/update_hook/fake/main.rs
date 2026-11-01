//! The `tierstore-hook-fake` binary: the stand-in `nft`, `systemctl` and staged release of the
//! update hook's conformance test. Everything it does is `tierstore::server::update_hook::fake`
//! (measured, and tested there); this forwards the program's name, arguments and standard input
//! to it, writes what it answers, and kills the parent when it says so.

use std::io::{self, Write};
use std::process::ExitCode;

use rustix::process::{Signal, getppid, kill_process};
use tierstore::server::update_hook::fake;

fn main() -> ExitCode {
    let exe = std::env::current_exe().expect("the path of this program");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let done = fake::run(&exe, &args, &mut io::stdin());
    if done.kill_parent
        && let Some(parent) = getppid()
    {
        let _ = kill_process(parent, Signal::KILL);
    }
    let _ = io::stdout().write_all(done.reply.stdout.as_bytes());
    let _ = io::stderr().write_all(done.reply.stderr.as_bytes());
    ExitCode::from(done.reply.code)
}

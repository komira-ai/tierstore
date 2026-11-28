//! The tierstore of a run: the binary the runner is given, started with `--config` and an empty
//! environment, its standard output and standard error written to one log file.
//!
//! The configuration listens on a free port (`127.0.0.1:0`); tierstore says the address it bound
//! in its start line on standard output, `tierstore <version> listen=<address>[,<address>...] ...`,
//! which the runner reads from the log. A stop is a SIGTERM, after which tierstore drains and
//! exits; its exit status is a line of the report.

use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};

use crate::error::{Error, io_at};

/// How often the log is read while tierstore starts.
const START_POLL: Duration = Duration::from_millis(20);

/// A running tierstore.
pub struct Tierstore {
    child: Reaped,
    addr: SocketAddr,
    log: PathBuf,
}

impl Tierstore {
    /// Starts `binary --config config` with its output in `log`, and waits for its start line.
    ///
    /// # Errors
    /// The binary cannot be started or the log written or read, or tierstore ends before its
    /// start line.
    pub fn start(binary: &Path, config: &Path, log: &Path) -> Result<Self, Error> {
        let file = File::create(log).map_err(io_at(log))?;
        let errors = file.try_clone().map_err(io_at(log))?;
        let spawned = Command::new(binary)
            .arg("--config")
            .arg(config)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(errors)
            .spawn()
            .map_err(io_at(binary))?;
        // From here on every way out of this function stops the process unless it returns it.
        let mut child = Reaped(spawned);
        loop {
            let text = read_log(log)?;
            // Whole lines only: a line still being written could hold half a port.
            let whole = &text[..text.rfind('\n').map_or(0, |at| at + 1)];
            if let Some(addr) = whole.lines().find_map(listen_address) {
                let log = log.to_path_buf();
                return Ok(Self { child, addr, log });
            }
            if let Some(status) = child.0.try_wait().map_err(io_at(binary))? {
                let log_text = read_log(log)?;
                let (status, log) = (status.to_string(), log.to_path_buf());
                return Err(Error::TierstoreStart {
                    status,
                    log,
                    log_text,
                });
            }
            std::thread::sleep(START_POLL);
        }
    }

    /// The address of the S3 front, `/metricz` included.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The file of tierstore's output.
    #[must_use]
    pub fn log(&self) -> &Path {
        &self.log
    }

    /// Sends SIGTERM, waits for tierstore to drain and exit, and gives its exit status.
    ///
    /// # Errors
    /// The signal cannot be sent or the process waited for.
    pub fn stop(mut self) -> Result<String, Error> {
        let pid = Pid::from_child(&self.child.0);
        let sent = kill_process(pid, Signal::TERM).map_err(std::io::Error::from);
        sent.map_err(io_at(&self.log))?;
        let status = self.child.0.wait().map_err(io_at(&self.log))?;
        Ok(status.to_string())
    }
}

/// A child process that is killed and reaped when it is dropped: a run that ends by an error, in
/// [`Tierstore::start`] or before [`Tierstore::stop`], leaves no tierstore behind. After a stop the
/// process is already reaped and this does nothing.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The text of the log, with any bytes that are not UTF-8 replaced.
fn read_log(log: &Path) -> Result<String, Error> {
    let bytes = std::fs::read(log).map_err(io_at(log))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The first address of the start line `line`, if it is one.
#[must_use]
pub fn listen_address(line: &str) -> Option<SocketAddr> {
    let rest = line.strip_prefix("tierstore ")?;
    let word = rest
        .split_whitespace()
        .find_map(|w| w.strip_prefix("listen="))?;
    word.split(',').next()?.parse().ok()
}

//! `fake-s3`: the loopback S3 service of `objstore_s3::s3::fake` as a program of its own, for a
//! node under test to be pointed at by its file (`[cold_store] endpoint`), with a control socket
//! that switches how it serves.
//!
//! ```text
//! fake-s3 --control <socket> [--listen <address>]
//! ```
//!
//! It binds the control socket (which must not exist), starts the service on `--listen` (a free
//! loopback port when left out, as the black-box rule of buildrules gives each fake its own), and
//! prints one line on standard output: `fake-s3 listen=127.0.0.1:<port>
//! control=<socket>`. It takes any request, signed or not, in path style. On the control socket
//! each line is a command and each command gets one line back:
//!
//! | command | answer | what it does |
//! |---|---|---|
//! | `serve` | `ok` | every request is served |
//! | `stall [<prefix>]` | `ok` | a request whose key starts with the prefix is held unanswered until another command; no prefix holds every request |
//! | `fail <status> [<prefix>]` | `ok` | such a request is answered with the HTTP status (400 to 599) and S3's error body |
//! | `stats` | `stalled=<n> begun=<n>` | the requests a stall has held so far, and every request the service has read |
//! | `stop` | `ok` | the program then exits 0 |
//!
//! Anything else is answered `refused: <why>`. A wrong flag exits 2, a control socket or an address
//! that cannot be bound exits 1.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use objstore_s3::s3::fake::{FakeS3, Serving};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// The flags of `fake-s3`.
#[derive(Debug, Parser)]
#[command(
    name = "fake-s3",
    about = "The loopback S3 service of objstore-s3's tests, with a control socket that stalls or fails its keys"
)]
struct Args {
    /// The Unix socket of the control commands, made by the program; it must not exist.
    #[arg(long)]
    control: PathBuf,
    /// The loopback address to serve S3 on; a free port of 127.0.0.1 when left out.
    #[arg(long)]
    listen: Option<SocketAddr>,
}

/// Why the program stopped.
#[derive(Debug, thiserror::Error)]
enum FakeError {
    #[error("the control socket {}: {source}", path.display())]
    Control {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the address {addr}: {source}")]
    Listen {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("the start line: {0}")]
    Line(std::io::Error),
    #[error("the runtime: {0}")]
    Runtime(std::io::Error),
}

/// What a line of the control socket asks.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve(Serving),
    Stats,
    Stop,
}

/// The program: the flags, then the service until `stop`.
pub fn main() -> ExitCode {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(refused) => {
            let _ = refused.print();
            return ExitCode::from(u8::try_from(refused.exit_code()).unwrap_or(2));
        }
    };
    let ran = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(FakeError::Runtime)
        .and_then(|runtime| {
            runtime.block_on(run(&args.control, args.listen, &mut std::io::stdout()))
        });
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "fake-s3: {why}");
            ExitCode::from(1)
        }
    }
}

/// Binds `control`, starts the service on `listen` (a free loopback port when none), writes the
/// start line to `out`, and answers the control socket's connections one after another until one
/// says `stop`.
async fn run(
    control: &Path,
    listen: Option<SocketAddr>,
    out: &mut impl Write,
) -> Result<(), FakeError> {
    let refused = |source| FakeError::Control {
        path: control.to_owned(),
        source,
    };
    let listener = UnixListener::bind(control).map_err(refused)?;
    let s3 = match listen {
        Some(addr) => FakeS3::start_at(addr)
            .await
            .map_err(|source| FakeError::Listen { addr, source })?,
        None => FakeS3::start().await,
    };
    let listen = s3.endpoint().trim_start_matches("http://");
    writeln!(out, "fake-s3 listen={listen} control={}", control.display())
        .and_then(|()| out.flush())
        .map_err(FakeError::Line)?;
    loop {
        let (stream, _) = listener.accept().await.map_err(refused)?;
        if converse(stream, &s3).await {
            return Ok(());
        }
    }
}

/// Answers the commands of one connection in turn until it closes; whether one was `stop`.
async fn converse(stream: UnixStream, s3: &FakeS3) -> bool {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let (answer, stop) = match parse(&line) {
            Ok(Command::Serve(serving)) => {
                s3.serve_as(serving);
                ("ok".to_owned(), false)
            }
            Ok(Command::Stats) => (
                format!(
                    "stalled={} begun={}",
                    s3.stalled_requests(),
                    s3.requests_begun()
                ),
                false,
            ),
            Ok(Command::Stop) => ("ok".to_owned(), true),
            Err(why) => (format!("refused: {why}"), false),
        };
        // A client that went before its answer finds the connection closed; nothing is lost.
        let _ = write.write_all(format!("{answer}\n").as_bytes()).await;
        if stop {
            return true;
        }
    }
    false
}

/// The command of one control line, or why it is none.
fn parse(line: &str) -> Result<Command, String> {
    let mut words = line.split_whitespace();
    let command = match words.next() {
        Some("serve") => Command::Serve(Serving::All),
        Some("stall") => Command::Serve(Serving::Stalled {
            prefix: words.next().unwrap_or_default().to_owned(),
        }),
        Some("fail") => {
            let word = words.next().ok_or("fail needs a status")?;
            let status = word
                .parse::<u16>()
                .ok()
                .filter(|status| (400..600).contains(status))
                .ok_or_else(|| format!("{word:?} is not a status of 400 to 599"))?;
            Command::Serve(Serving::Failing {
                prefix: words.next().unwrap_or_default().to_owned(),
                status,
            })
        }
        Some("stats") => Command::Stats,
        Some("stop") => Command::Stop,
        Some(other) => return Err(format!("unknown command {other:?}")),
        None => return Err("an empty line".to_owned()),
    };
    match words.next() {
        None => Ok(command),
        Some(extra) => Err(format!("{extra:?} after the command")),
    }
}

#[cfg(test)]
#[path = "fake_tests.rs"]
mod tests;

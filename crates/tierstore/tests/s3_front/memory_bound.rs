//! The memory bound of the `tierstore` binary (`server.memory_bound`), as a process: one over its
//! bound drains, counts the exit in its state directory and exits 75, start after start; one under
//! it serves a normal load, shows the bound on `/memoryz` and `/metricz` and exits 0 on SIGTERM.
//! Nothing asserts how long anything takes: each wait is bounded only so that a hung process fails
//! the test.

use std::process::{Command, Stdio};
use std::time::Duration;

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use objstore_s3::{ByteRange, ObjectKey, ObjectStore};
use rustix::process::{Pid, Signal, kill_process};
use tierstore_testkit::dir::TestDir;
use tierstore_testkit::http::client as http_client;
use tierstore_testkit::reaped::spawn;

use crate::binary::start;
use crate::files::{Files, Tables};

const BIN: &str = env!("CARGO_BIN_EXE_tierstore");

/// The longest a test waits for the binary; it only ends a test whose process hangs.
const SAFETY: Duration = Duration::from_secs(60);

/// The default bound, 32 GiB, and its mark.
const BOUND: u64 = 32 << 30;
const MARK: u64 = BOUND / 2;

/// Runs the binary with `flags` and the state directory `state` to its end: the exit status and
/// stderr. A process that has not ended within `SAFETY` is killed, and the test fails.
async fn run_in_state_dir(flags: &[String], state: &TestDir, name: &str) -> (Option<i32>, String) {
    let log = state.path().join(format!("{name}.stderr"));
    let mut command = Command::new(BIN);
    command
        .args(flags)
        .env("STATE_DIRECTORY", state.path())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    let mut child = spawn(&mut command, 3).unwrap();
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    let waiting = tokio::task::spawn_blocking(move || {
        let status = child.wait();
        (child, status)
    });
    let waited = tokio::time::timeout(SAFETY, waiting).await;
    if waited.is_err() {
        kill_process(pid, Signal::KILL).unwrap();
    }
    let (_child, status) = waited.expect("the process did not exit").unwrap();
    (
        status.unwrap().code(),
        std::fs::read_to_string(&log).unwrap(),
    )
}

/// Asserts that a process whose bound (1 MiB) is below what it holds at its start reaches the
/// bound at its first look: it says the sizes on stderr, drains (a short settle and deadline) and
/// exits 75, and the count in its state directory is 1 after the first start and 2 after the
/// second. Catches a bound that does not stop the process, an exit status the units would not
/// start again, and an exit that is not counted across starts.
#[tokio::test]
async fn a_process_over_its_bound_drains_counts_the_exit_and_exits_75() {
    let files = Files::new("bin-memory-over");
    let tables = Tables {
        server: "memory_bound = \"1MiB\"\ndrain_lb_wait = \"50ms\"\ndrain_deadline = \"5s\""
            .to_owned(),
        ..Tables::default()
    };
    let flags = files.flags_of(&tables, &[]);
    let state = TestDir::under(std::env::temp_dir(), "tsmb");
    for start in 1..=2 {
        let (status, stderr) = run_in_state_dir(&flags, &state, &format!("start-{start}")).await;
        assert_eq!(status, Some(75), "{stderr}");
        let line = stderr
            .lines()
            .find(|line| line.contains("memory bound reached"))
            .unwrap_or_else(|| panic!("no line of the bound in:\n{stderr}"));
        assert!(
            line.contains("tierstore: memory bound reached: anonymous memory "),
            "{line}"
        );
        assert!(
            line.contains(" bytes, bound 1048576 bytes (resident file "),
            "{line}"
        );
        assert!(
            line.ends_with("; draining for at most 30 s, then exiting with status 75"),
            "{line}"
        );
        let count = std::fs::read_to_string(state.path().join("memory-bound-exits")).unwrap();
        assert_eq!(count, format!("{start}\n"));
    }
}

/// The value of the unlabelled series `name` in a `/metricz` text.
fn sample(text: &str, name: &str) -> u64 {
    let prefix = format!("{name} ");
    let line = text.lines().find(|line| line.starts_with(&prefix));
    let line = line.unwrap_or_else(|| panic!("no series {name} in:\n{text}"));
    line[prefix.len()..].parse().unwrap()
}

/// The `/metricz` of the process at `addr` once it has taken a look at its memory (its anonymous
/// memory is above 0).
async fn metricz_after_a_look(addr: &str) -> String {
    let url = format!("http://{addr}/metricz");
    let client = http_client();
    let asking = async {
        loop {
            let text = client.get(&url).send().await.unwrap().text().await.unwrap();
            if sample(&text, "tierstore_memory_anon_bytes") > 0 {
                return text;
            }
        }
    };
    tokio::time::timeout(SAFETY, asking)
        .await
        .expect("no look at the memory")
}

/// Asserts that a process with the default bound serves a normal load (32 objects of 1 MiB put by
/// 8 clients at once and read back) without reaching its mark: `/metricz` shows the bound of 32
/// GiB, its anonymous memory above 0 and under the mark, not degraded, no exit at the bound;
/// `/memoryz` is 200 with the mark and the bound; and SIGTERM ends it with 0, not 75. Catches a
/// default that would restart a node under load, and a watch that is not shown.
#[tokio::test]
async fn a_process_under_its_bound_serves_a_load_shows_it_and_exits_0() {
    let files = Files::new("bin-memory-under");
    let tables = Tables {
        hot: "kind = \"memory\"\nbudget = 268435456".to_owned(),
        server: "drain_lb_wait = \"50ms\"".to_owned(),
        ..Tables::default()
    };
    let mut running = start(&files.flags_of(&tables, &[]));
    let store = Arc::new(running.store());
    let body = Bytes::from(vec![7u8; 1 << 20]);
    let puts = (0..32).map(|n| {
        let (store, body) = (Arc::clone(&store), body.clone());
        async move {
            let key = ObjectKey::new(&format!("cas/load-{n}")).unwrap();
            store.put_new(&key, body, None).await.unwrap();
            key
        }
    });
    let keys = futures::stream::iter(puts)
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await;
    let whole = ByteRange::new(0, 1 << 20).unwrap();
    for key in &keys {
        assert_eq!(store.get_range(key, whole).await.unwrap(), body, "{key:?}");
    }

    let text = metricz_after_a_look(running.addr()).await;
    let anon = sample(&text, "tierstore_memory_anon_bytes");
    assert!(anon < MARK, "{anon}");
    assert_eq!(sample(&text, "tierstore_memory_bound_bytes"), BOUND);
    assert_eq!(sample(&text, "tierstore_memory_degraded"), 0);
    assert_eq!(sample(&text, "tierstore_memory_bound_exits_total"), 0);
    let memoryz = format!("http://{}/memoryz", running.addr());
    let probe = http_client().get(&memoryz).send().await.unwrap();
    assert_eq!(probe.status(), 200);
    let said = probe.text().await.unwrap();
    assert!(said.starts_with("anonymous memory "), "{said}");
    assert!(
        said.ends_with(&format!(" bytes, mark {MARK} bytes, bound {BOUND} bytes")),
        "{said}"
    );

    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

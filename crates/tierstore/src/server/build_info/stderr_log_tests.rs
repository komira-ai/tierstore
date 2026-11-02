use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use tracing::Level;
use tracing::subscriber::with_default;

use super::stderr_log::{LineLog, install};

/// A writer whose bytes the test reads back.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Sink {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("the sink is not poisoned").clone())
            .expect("the lines are text")
    }
}

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the sink is not poisoned")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn capture(max: Level, emit: impl FnOnce()) -> String {
    let sink = Sink::default();
    with_default(LineLog::new(sink.clone(), max), emit);
    sink.text()
}

#[test]
fn an_event_is_one_line_with_the_text_of_its_message_and_nothing_else() {
    let text = capture(Level::INFO, || {
        tracing::info!("tierstore: scrub dropped {}: gone", "sha256/ab");
    });
    assert_eq!(text, "tierstore: scrub dropped sha256/ab: gone\n");
}

#[test]
fn the_fields_of_an_event_follow_its_message_as_name_equals_value() {
    let text = capture(Level::INFO, || {
        let error = "timed out";
        tracing::warn!(key = "a b", %error, tries = 3, "a read failed");
    });
    assert_eq!(text, "a read failed key=\"a b\" error=timed out tries=3\n");
}

#[test]
fn events_below_the_level_are_not_written() {
    let text = capture(Level::INFO, || {
        tracing::trace!("trace");
        tracing::debug!("debug");
        tracing::info!("info");
        tracing::warn!("warn");
        tracing::error!("error");
    });
    assert_eq!(text, "info\nwarn\nerror\n");
    let only_errors = capture(Level::ERROR, || {
        tracing::warn!("warn");
        tracing::error!("error");
    });
    assert_eq!(only_errors, "error\n");
}

#[test]
fn spans_are_accepted_and_not_written() {
    let text = capture(Level::INFO, || {
        let span = tracing::info_span!("outer", late = tracing::field::Empty);
        span.record("late", 1);
        let other = tracing::info_span!("other");
        span.follows_from(&other);
        span.in_scope(|| tracing::info!("inside"));
    });
    assert_eq!(text, "inside\n");
}

/// Panics on its first write and fails every later one.
struct Broken {
    panicked: bool,
}

impl Write for Broken {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        if self.panicked {
            return Err(io::Error::other("closed"));
        }
        self.panicked = true;
        panic!("the writer fails once with a panic");
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_stderr_that_cannot_be_written_does_not_stop_the_server() {
    with_default(
        LineLog::new(Broken { panicked: false }, Level::INFO),
        || {
            // The first write panics and poisons the lock; the next two meet the poisoned lock and
            // a write that returns an error, and neither takes the caller down.
            assert!(catch_unwind(AssertUnwindSafe(|| tracing::info!("first"))).is_err());
            tracing::info!("second");
            tracing::info!("third");
        },
    );
}

#[test]
fn installing_twice_keeps_the_first_subscriber() {
    install();
    install();
}

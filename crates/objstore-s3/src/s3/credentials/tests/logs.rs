//! What [`Live`] logs: a line for each fetch, and a warning for a renewal that failed while the
//! kept keys still sign, each naming the source and the expiry and never a secret.
//!
//! The subscriber is the process's global one, installed once, and keeps only the events of a
//! thread that asked for them. A scoped subscriber would not do: the interest of a callsite is
//! cached for the whole process, and with one scoped subscriber registered, a callsite that
//! another test's thread meets first is judged by that thread's subscriber (none), so its events
//! would never reach this test's.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::Once;

use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};

use super::*;

thread_local! {
    /// The events this thread keeps, as `LEVEL field=value ...`, while it keeps them.
    static KEPT: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// The global subscriber: it keeps the events of this crate on a thread that keeps them.
struct Keeping;

/// The fields of one event, as text.
struct Fields(String);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        write!(self.0, " {}={value:?}", field.name()).unwrap();
    }
}

impl Subscriber for Keeping {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target().starts_with("objstore_s3") && KEPT.with(|kept| kept.borrow().is_some())
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields(event.metadata().level().to_string());
        event.record(&mut fields);
        KEPT.with(|kept| {
            if let Some(lines) = kept.borrow_mut().as_mut() {
                lines.push(fields.0);
            }
        });
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Starts keeping the events of this thread.
fn keep() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| tracing::subscriber::set_global_default(Keeping).unwrap());
    KEPT.with(|kept| *kept.borrow_mut() = Some(Vec::new()));
}

/// The events this thread kept, and stops keeping them.
fn kept() -> Vec<String> {
    KEPT.with(|kept| kept.borrow_mut().take().unwrap_or_default())
}

/// Asserts the line of a fetch, word for word, and the warning of a renewal that failed with
/// more than a minute left, word for word: the source, the expiry and the error, and neither the
/// secret, the session token nor the access key id.
#[tokio::test]
async fn a_fetch_and_a_failed_renewal_are_logged_without_secrets() {
    let answer = json_credentials("ASIALOGGED", "logged-secret", "logged-token", T0 + 3600);
    let fake = Fake::start(move |_| (200, answer.clone())).await;
    let uri = format!("{}/creds", fake.url());
    let env = env(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", &uri)]);
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Container, env, &clock);
    keep();
    live.keys().await.unwrap();
    fake.reroute(|_| (503, String::new()));
    clock.set(T0 + 3600 - 120);
    live.keys().await.unwrap();
    let lines = kept();
    let expires = iso(T0 + 3600);
    assert_eq!(
        lines,
        [
            format!("INFO message=fetched AWS credentials source=container expires={expires}"),
            format!(
                "WARN message=renewing AWS credentials failed; signing with the current ones until \
                 they near their end source=container expires={expires} error=credential source \
                 container: the container credentials endpoint answered 503"
            ),
        ]
    );
    for line in &lines {
        for secret in ["ASIALOGGED", "logged-secret", "logged-token"] {
            assert!(!line.contains(secret), "{line}");
        }
    }
}

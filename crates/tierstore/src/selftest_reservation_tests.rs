//! The reservation check of the self-test: the reference backends pass it, and a backend that
//! answers one of its questions wrongly is refused with the check and the answer.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tierstore_testkit::dir::TestDir;

use crate::{
    Backend, ByteRange, Check, DirBackend, Error, Listed, MemoryBackend, Meta, SelfTestError,
    SelfTestFailure, SelfTestOptions, self_test,
};

/// The prefix the probe keys are under.
const PROBE: &str = "_tiering/selftest/";

/// How an [`Odd`] backend differs from a memory backend.
#[derive(Clone, Copy, Debug)]
enum Way {
    /// Behaves as the memory backend.
    Right,
    /// Does not do reservations.
    Unsupported,
    /// A reservation is written with no mark.
    DropsTheMark,
    /// A HEAD of a reservation gives a size that is one too many.
    WrongSize,
    /// A second reservation of a key replaces the first.
    ReplacesOnReserve,
    /// A marked replace on a wrong tag takes effect.
    IgnoresTagOnMarkedReplace,
    /// A marked replace writes no mark.
    MarkedReplaceDropsTheMark,
    /// A plain replace of a reservation on a wrong tag takes effect.
    IgnoresTagOnPlainReplace,
    /// Callers that race to reserve one key all win.
    EveryRacerWins,
    /// A delete on the tag of a reservation leaves it.
    KeepsOnDeleteIfMatch,
    /// A HEAD of an empty object says it is a reservation.
    EmptyIsMarked,
}

/// A memory backend that does one thing wrongly about reservations.
struct Odd {
    inner: MemoryBackend,
    way: Way,
}

impl Odd {
    fn new(way: Way) -> Self {
        Self {
            inner: MemoryBackend::new(),
            way,
        }
    }

    fn is(&self, way: Way) -> bool {
        std::mem::discriminant(&self.way) == std::mem::discriminant(&way)
    }
}

#[async_trait]
impl Backend for Odd {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_reservation(&self, key: &str, body: Bytes) -> Result<String, Error> {
        if self.is(Way::Unsupported) {
            let why = "put_reservation: this backend does not do reservations";
            return Err(Error::Unsupported(why.to_owned()));
        }
        if self.is(Way::DropsTheMark) {
            self.inner.put(key, body).await?;
            return Ok(self.inner.head(key).await?.tag);
        }
        let again = key.ends_with("/reservation");
        let racing = key.ends_with("/reservation-race");
        if (self.is(Way::ReplacesOnReserve) && again) || (self.is(Way::EveryRacerWins) && racing) {
            let _ = self.inner.delete(key).await;
        }
        self.inner.put_reservation(key, body).await
    }

    async fn put_reservation_if_match(
        &self,
        key: &str,
        body: Bytes,
        etag: &str,
    ) -> Result<String, Error> {
        if self.is(Way::IgnoresTagOnMarkedReplace) {
            let tag = self.inner.head(key).await?.tag;
            return self.inner.put_reservation_if_match(key, body, &tag).await;
        }
        if self.is(Way::MarkedReplaceDropsTheMark) {
            return self.inner.put_if_match(key, body, etag).await;
        }
        self.inner.put_reservation_if_match(key, body, etag).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        if self.is(Way::IgnoresTagOnPlainReplace) {
            let tag = self.inner.head(key).await?.tag;
            return self.inner.put_if_match(key, body, &tag).await;
        }
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let mut meta = self.inner.head(key).await?;
        if self.is(Way::WrongSize) && meta.reservation {
            meta.size += 1;
        }
        if self.is(Way::EmptyIsMarked) && meta.size == 0 {
            meta.reservation = true;
        }
        Ok(meta)
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        if self.is(Way::KeepsOnDeleteIfMatch) {
            return Ok(());
        }
        self.inner.delete_if_match(key, etag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}

/// The checks that are not the reservation check.
fn the_others() -> Vec<Check> {
    Check::ALL
        .into_iter()
        .filter(|check| *check != Check::Reservation)
        .collect()
}

/// Runs the reservation check alone on a cold tier.
async fn reservation_check(backend: &dyn Backend) -> Result<(), SelfTestError> {
    let skip = the_others();
    let options = SelfTestOptions {
        name: "the backend",
        run_id: "run-1",
        multipart: true,
        skip: &skip,
        wait: Duration::ZERO,
        pause: Duration::from_secs(1),
        max_pause: Duration::from_secs(4),
    };
    self_test(backend, &options, &|_| ()).await
}

/// The failure of the reservation check of a backend that does one thing wrongly.
async fn refused(way: Way) -> (SelfTestFailure, Odd) {
    let backend = Odd::new(way);
    let outcome = reservation_check(&backend).await;
    match outcome {
        Err(SelfTestError::Violation(failure)) => (failure, backend),
        other => panic!("{way:?} was not found to answer wrongly: {other:?}"),
    }
}

/// Asserts that the memory backend and a directory backend pass the reservation check and leave
/// no probe key behind.
#[tokio::test]
async fn the_memory_and_the_directory_backend_pass_the_reservation_check() {
    let memory = MemoryBackend::new();
    assert_eq!(reservation_check(&memory).await, Ok(()));
    assert!(memory.list(PROBE).await.unwrap().is_empty());

    let dir = TestDir::under(std::env::temp_dir(), "tierstore-selftest-reservation");
    let folder = DirBackend::open(dir.path().join("bucket")).unwrap();
    assert_eq!(reservation_check(&folder).await, Ok(()));
    assert!(folder.list(PROBE).await.unwrap().is_empty());
}

/// Asserts that a backend that does one thing wrongly about reservations is refused as the
/// reservation check, with the answer it gave, whichever of the questions it answers wrongly, and
/// that no probe key is left behind.
#[tokio::test]
async fn a_backend_that_answers_a_reservation_question_wrongly_is_refused_with_the_answer() {
    let cases = [
        (Way::Unsupported, "does not do reservations"),
        (
            Way::DropsTheMark,
            "the mark of a reservation gave false, not true",
        ),
        (Way::WrongSize, "the size of a reservation gave 57, not 56"),
        (
            Way::ReplacesOnReserve,
            "a second reservation of the same key was accepted",
        ),
        (
            Way::IgnoresTagOnMarkedReplace,
            "a reservation replace on a wrong ETag was accepted",
        ),
        (
            Way::MarkedReplaceDropsTheMark,
            "the mark of the reservation after a marked replace gave false",
        ),
        (
            Way::IgnoresTagOnPlainReplace,
            "a replace of a reservation on a wrong ETag was accepted",
        ),
        (
            Way::EveryRacerWins,
            "the callers that won a race to reserve one key gave 4, not 1",
        ),
        (
            Way::KeepsOnDeleteIfMatch,
            "a HEAD found a reservation after its delete on its ETag",
        ),
        (Way::EmptyIsMarked, "the mark of an empty object gave true"),
    ];
    for (way, said) in cases {
        let (failure, backend) = refused(way).await;
        assert_eq!(failure.check, Check::Reservation, "{way:?}");
        assert_eq!(failure.backend, "the backend");
        assert!(failure.observed.contains(said), "{way:?}: {failure}");
        let left = backend.inner.list(PROBE).await.unwrap();
        assert!(left.is_empty(), "{way:?} left {left:?}");
    }
}

/// Asserts that the double with nothing wrong passes the check, so that each refusal above is its
/// one flaw and not the double.
#[tokio::test]
async fn the_double_with_no_flaw_passes_the_reservation_check() {
    assert_eq!(reservation_check(&Odd::new(Way::Right)).await, Ok(()));
}

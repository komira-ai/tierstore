//! The reconciler's loop, the skipped checks and the checks run before a start.

use super::*;

/// Asserts that the reconciler's loop works at once and then at every interval, tries again at
/// the next interval a copy that failed, and gives its lease up when it is halted: the copy that
/// failed in the first pass is made in the second, and the lease is gone after the halt.
#[tokio::test(start_paused = true)]
async fn the_reconciler_works_every_interval_until_halted() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
    cold.fail_next(Op::Put, 1);
    let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    let reconciler = Reconciler::new(hot.clone(), cold.clone(), settings);
    let halt = Arc::new(Notify::new());
    let working = reconcile(reconciler, Duration::from_secs(5), Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(cold_mem.get("k", None).await.is_err());
        tokio::time::sleep(Duration::from_secs(5)).await;
        let copied = cold_mem.get("k", None).await.unwrap();
        assert_eq!(copied, Bytes::from_static(b"v"));
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(hot.get("_tiering/lease", None).await.is_err());
}

/// Asserts that a halt cuts a pass that is still running and gives the lease up at once: a copy
/// to a cold tier that takes an hour is not waited for, the object is not copied, and the lease
/// is gone.
#[tokio::test(start_paused = true)]
async fn a_halt_cuts_the_pass_that_is_running() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
    cold.set_delay(Op::Put, Some(Duration::from_secs(3600)));
    let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    let reconciler = Reconciler::new(hot.clone(), cold.clone(), settings);
    let halt = Arc::new(Notify::new());
    let working = reconcile(reconciler, Duration::from_secs(5), Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        halt.notify_one();
    };
    let both = async { tokio::join!(working, watching) };
    let done = tokio::time::timeout(Duration::from_secs(60), both).await;
    assert!(done.is_ok());
    assert!(hot.get("_tiering/lease", None).await.is_err());
    assert!(cold_mem.get("k", None).await.is_err());
}

/// Asserts that `--skip-backend-check` takes `multipart-conditional` and a reason, may be given
/// more than once, trims the reason and keeps a `=` inside it; that a value with no `=` or an
/// unknown check is refused with a message that lists the checks; that a blank reason is refused
/// with a message that says a reason is needed; and that every check but
/// `multipart-conditional` is refused as a usage error (exit status 2), whatever its reason.
#[test]
fn only_the_multipart_check_can_be_skipped_and_with_a_reason() {
    let mut args = flags("key", &[]);
    args.extend(
        [
            "--skip-backend-check",
            "multipart-conditional= store ignores it on completion ",
            "--skip-backend-check",
            "multipart-conditional=a=b",
        ]
        .map(str::to_owned),
    );
    let cli = Cli::try_parse_from(args).unwrap();
    let want = [
        SkippedCheck {
            check: Check::MultipartConditional,
            reason: "store ignores it on completion".to_owned(),
        },
        SkippedCheck {
            check: Check::MultipartConditional,
            reason: "a=b".to_owned(),
        },
    ];
    assert_eq!(cli.skip_backend_check, want);

    for text in ["range", "ranges=because", "=because"] {
        let why = parse_skip(text).unwrap_err();
        assert!(why.contains("CHECK=REASON"), "{text}: {why}");
        assert!(why.contains("multipart-conditional"), "{text}: {why}");
    }
    for text in ["multipart-conditional=", "multipart-conditional=   "] {
        let why = parse_skip(text).unwrap_err();
        assert!(why.contains("needs a reason"), "{text}: {why}");
    }
    for check in Check::ALL
        .into_iter()
        .filter(|c| *c != Check::MultipartConditional)
    {
        let value = format!("{check}=the store cannot do it");
        let why = parse_skip(&value).unwrap_err();
        assert!(why.contains("cannot be skipped"), "{value}: {why}");
        assert!(why.contains(check.name()), "{value}: {why}");
        let refused = Cli::try_parse_from(flags("key", &[("--skip-backend-check", &value)]));
        assert_eq!(refused.unwrap_err().exit_code(), 2, "{value}");
    }
}

/// Asserts that the id of a start's probe keys has the proxy id, the time and the process, with
/// every character a key cannot carry in the proxy id replaced by an underscore.
#[test]
fn the_probe_run_id_is_made_of_the_proxy_the_time_and_the_process() {
    assert_eq!(probe_run_id("p1", 1234, 77), "p1-1234-77");
    assert_eq!(probe_run_id("a/b c.d-e", 5, 6), "a_b_c_d-e-5-6");
    assert_ne!(probe_run_id("p1", 1, 7), probe_run_id("p1", 2, 7));
}

/// Asserts that a tier that answered the self-test wrongly is status 3, a tier that could not be
/// reached status 1 (a restart may mend it), flags that cannot go together status 2 and any
/// other failure status 1.
#[test]
fn a_wrong_answer_has_its_own_exit_status_and_an_unreachable_tier_has_not() {
    let failure = SelfTestFailure {
        backend: "hot tier memory".to_owned(),
        check: Check::Range,
        observed: "x".to_owned(),
    };
    let wrong: super::Error = Box::new(SelfTestError::Violation(failure));
    let down = SelfTestError::Unreachable {
        backend: "hot tier memory".to_owned(),
        error: "connection refused".to_owned(),
    };
    let down: super::Error = Box::new(down);
    let invalid: super::Error = Box::new(InvalidFlags("x".to_owned()));
    let other: super::Error = "another reason".into();
    assert_eq!(exit_status(&wrong), 3);
    assert_eq!(exit_status(&down), 1);
    assert_eq!(exit_status(&invalid), 2);
    assert_eq!(exit_status(&other), 1);
}

/// Asserts that the two tiers are tested before a start: tiers that are correct pass; a hot tier
/// that fails is named as the hot tier with its flag value and the check; a cold tier that fails
/// is named as the cold tier; the failure is the error a start gives, so that its exit status is
/// 3; and a check the flags skip is not run, so a tier that fails only that one passes.
#[tokio::test]
async fn the_tiers_are_tested_before_a_start_and_a_failure_names_the_tier() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-probe-tiers");
    let mut cli = cli(&dir);
    cli.hot = "s3:hot-bucket".to_owned();
    cli.cold = "dir:/cold".to_owned();
    let good = Flawed::new(Flaw::None);
    let now = Duration::ZERO;
    probe_tiers(&cli, &good, &good, now).await.unwrap();

    let broken = Flawed::new(Flaw::OverwritesOnCreate);
    let why = probe_tiers(&cli, &broken, &good, now).await.unwrap_err();
    assert_eq!(exit_status(&why), 3);
    let text = why.to_string();
    assert!(text.contains("hot tier s3:hot-bucket"), "{text}");
    assert!(text.contains("create-only"), "{text}");

    let why = probe_tiers(&cli, &good, &broken, now).await.unwrap_err();
    let text = why.to_string();
    assert!(text.contains("cold tier dir:/cold"), "{text}");
    assert!(text.contains("create-only"), "{text}");

    let no_multipart = Flawed::new(Flaw::NoMultipart);
    probe_tiers(&cli, &no_multipart, &good, now).await.unwrap();
    let why = probe_tiers(&cli, &good, &no_multipart, now)
        .await
        .unwrap_err();
    assert!(why.to_string().contains("multipart-conditional"), "{why}");

    cli.skip_backend_check = vec![SkippedCheck {
        check: Check::MultipartConditional,
        reason: "test".to_owned(),
    }];
    probe_tiers(&cli, &good, &no_multipart, now).await.unwrap();
    assert!(good.inner.list("_tiering/").await.unwrap().is_empty());
}

/// Asserts that a tier that cannot be reached, hot or cold, is not a wrong answer: the start
/// fails with `tier unreachable: <tier>: <error>` and exit status 1, which the unit restarts; and
/// that a full hot tier, a state it is designed to be in, and a full cold tier, start.
#[tokio::test]
async fn an_unreachable_tier_is_status_1_and_a_full_tier_starts() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-probe-down");
    let mut cli = cli(&dir);
    cli.hot = "s3:hot-bucket".to_owned();
    cli.cold = "s3:cold-bucket".to_owned();
    let good = Flawed::new(Flaw::None);
    let now = Duration::ZERO;
    let down = Flawed::new(Flaw::Down);
    for (hot, cold, tier) in [
        (&down, &good, "hot tier s3:hot-bucket"),
        (&good, &down, "cold tier s3:cold-bucket"),
    ] {
        let why = probe_tiers(&cli, hot, cold, now).await.unwrap_err();
        assert_eq!(exit_status(&why), 1, "{why}");
        let text = why.to_string();
        assert!(
            text.starts_with(&format!("tier unreachable: {tier}: ")),
            "{text}"
        );
        assert!(text.contains("connection refused"), "{text}");
    }

    let full = Flawed::new(Flaw::Full);
    probe_tiers(&cli, &full, &good, now).await.unwrap();
    probe_tiers(&cli, &good, &full, now).await.unwrap();
    let wrong = Flawed::new(Flaw::IgnoresIfMatch);
    let why = probe_tiers(&cli, &full, &wrong, now).await.unwrap_err();
    assert_eq!(exit_status(&why), 3, "{why}");
    // A 501 from the store (the S3 backend's other error) is a contract gap: status 3.
    let not_implemented = Flawed::new(Flaw::RejectsConditionalHeader);
    let why = probe_tiers(&cli, &not_implemented, &good, now)
        .await
        .unwrap_err();
    assert_eq!(exit_status(&why), 3, "{why}");
}

/// Asserts that the report of a drain names the requests that the deadline cut, and that a drain
/// that cut none reports nothing.
#[test]
fn a_drain_that_cut_requests_reports_them_and_one_that_cut_none_says_nothing() {
    assert_eq!(cut_note(Drained { abandoned: 0 }), "");
    assert_eq!(
        cut_note(Drained { abandoned: 3 }),
        "tierstore: 3 requests were cut at the drain deadline\n"
    );
}

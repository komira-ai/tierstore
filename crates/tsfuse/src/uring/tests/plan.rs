//! Memory accounting, the choice of transport, and INIT.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a unit test: a failed unwrap or expect is the failure it reports"
)]

use super::*;

#[test]
fn the_registered_memory_is_queues_times_depth_times_header_and_payload() {
    // K13's table: 88 queues of 1 MiB and of 128 KiB payloads.
    let mib = UringMemory::of(88, 1, 1 << 20);
    assert_eq!(mib.bytes, 88 * (288 + (1 << 20)));
    assert!(mib.bytes > UringConfig::MEMORY_LIMIT, "88 MiB at depth 1");
    let tree = UringMemory::of(88, 5, 128 << 10);
    assert_eq!(tree.bytes, 57_798_400);
    assert!(tree.bytes <= UringConfig::MEMORY_LIMIT, "55 MiB at depth 5");
    let six = UringMemory::of(88, 6, 128 << 10);
    assert!(
        six.bytes > UringConfig::MEMORY_LIMIT,
        "depth 6 is past 64 MiB"
    );
    assert_eq!(UringMemory::of(0, 5, 8192).bytes, 0);
    assert_eq!(
        UringMemory::of(u32::MAX, u16::MAX, u32::MAX).bytes,
        u64::MAX,
        "past u64 it saturates, over any limit"
    );
}

#[test]
fn the_payload_is_the_largest_of_8_kib_max_write_and_max_pages() {
    assert_eq!(UringMemory::payload(4096, 32, 4096), 128 << 10);
    assert_eq!(UringMemory::payload(4096, 256, 4096), 1 << 20);
    assert_eq!(
        UringMemory::payload(1 << 20, 32, 4096),
        1 << 20,
        "max_write"
    );
    assert_eq!(
        UringMemory::payload(4096, 1, 4096),
        MIN_PAYLOAD,
        "8 KiB floor"
    );
    assert_eq!(
        UringMemory::payload(4096, 0, 4096),
        128 << 10,
        "no max_pages: the kernel's 32"
    );
    assert_eq!(UringMemory::payload(0, u16::MAX, u32::MAX), u32::MAX);
}

fn negotiated(offered: u64, config: &InitConfig) -> Negotiated {
    let init = InitIn {
        major: 7,
        minor: 45,
        max_readahead: 65536,
        flags: offered as u32,
        flags2: (offered >> 32) as u32,
        ..InitIn::default()
    };
    match config.negotiate(&init).unwrap() {
        crate::abi::InitOutcome::Accepted(n) => n,
        crate::abi::InitOutcome::MajorAhead(_) => unreachable!(),
    }
}

#[test]
fn the_plan_checks_the_parameter_the_offer_the_cpus_and_the_memory_in_that_order() {
    let config = UringConfig::default();
    let tree32 = InitConfig {
        max_pages: 32,
        ..InitConfig::tree()
    };
    let n = negotiated(offered_uring(), &tree32);
    assert_eq!(
        plan(&config, &host(88), &n),
        Ok(UringMemory::of(88, 5, 128 << 10)),
        "both profiles' build nodes: 88 possible CPUs"
    );
    let off = Host {
        enabled: false,
        cpus: None,
        ..host(88)
    };
    let not_offered = negotiated(InitFlags::INIT_EXT.0, &tree32);
    assert_eq!(plan(&config, &off, &not_offered), Err(Fallback::Disabled));
    assert_eq!(
        plan(&config, &host(88), &not_offered),
        Err(Fallback::NotOffered)
    );
    let unknown = Host {
        cpus: None,
        ..host(88)
    };
    assert_eq!(plan(&config, &unknown, &n), Err(Fallback::CpusUnknown));
    let tree256 = negotiated(offered_uring(), &InitConfig::tree());
    assert_eq!(
        plan(&config, &host(88), &tree256),
        Err(Fallback::OverMemoryLimit {
            memory: UringMemory::of(88, 5, 1 << 20),
            limit: 64 << 20,
        }),
        "the tree's max_pages 256 is over 64 MiB"
    );
    // At exactly the limit io_uring is used; one byte under it, not.
    let exact = UringConfig {
        memory_limit: 57_798_400,
        ..config
    };
    assert!(plan(&exact, &host(88), &n).is_ok());
    let under = UringConfig {
        memory_limit: 57_798_399,
        ..config
    };
    assert!(matches!(
        plan(&under, &host(88), &n),
        Err(Fallback::OverMemoryLimit { .. })
    ));
}

#[test]
fn the_default_config_is_depth_5_and_64_mib() {
    let c = UringConfig::default();
    assert_eq!(c.depth.get(), 5);
    assert_eq!(c.memory_limit, 64 * 1024 * 1024);
}

#[test]
fn a_cpu_list_is_counted_by_its_ranges() {
    assert_eq!(parse_cpu_list("0-87\n"), Some(88));
    assert_eq!(parse_cpu_list("0"), Some(1));
    assert_eq!(parse_cpu_list("0,2-5,7"), Some(6));
    assert_eq!(parse_cpu_list(""), None);
    assert_eq!(parse_cpu_list("  \n"), None);
    assert_eq!(parse_cpu_list("a-3"), None);
    assert_eq!(parse_cpu_list("0-b"), None);
    assert_eq!(parse_cpu_list("x"), None);
    assert_eq!(parse_cpu_list("5-3"), None, "a range that runs backwards");
    assert_eq!(parse_cpu_list("0-4294967295"), None, "a count past u32");
    assert_eq!(parse_cpu_list("0-4294967294,1"), None, "a total past u32");
}

#[test]
fn the_parameter_is_on_only_when_it_says_so() {
    for on in ["Y", "y\n", "1", " Y "] {
        assert!(parse_enabled(on), "{on:?}");
    }
    for off in ["N", "n", "0", "", "yes"] {
        assert!(!parse_enabled(off), "{off:?}");
    }
}

#[test]
fn the_host_is_read_from_the_two_files_and_a_missing_one_means_off_or_unknown() {
    let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "uring-host");
    let (enable, possible) = (dir.path().join("enable"), dir.path().join("possible"));
    let h = Host::read_from(&enable, &possible);
    assert!(!h.enabled);
    assert_eq!(h.cpus, None);
    std::fs::write(&enable, "Y\n").unwrap();
    std::fs::write(&possible, "0-87\n").unwrap();
    let h = Host::read_from(&enable, &possible);
    assert!(h.enabled);
    assert_eq!(h.cpus, Some(88));
    assert!(h.page >= 4096);
}

#[test]
fn a_ring_that_cannot_be_made_is_a_fallback_with_its_errno() {
    let memory = UringMemory::of(3, 1, 8192);
    let mut made = 0;
    let err = make_rings(memory, &mut |qid| {
        made += 1;
        match qid {
            2 => Err(io::Error::from_raw_os_error(1)),
            _ => Ok(Box::new(FakeRing::new(qid, &Arc::default())) as Box<dyn Ring>),
        }
    })
    .unwrap_err();
    assert_eq!(err, Fallback::RingSetup { errno: 1 });
    assert_eq!(made, 3);
    let err = make_rings(memory, &mut |_| Err(io::Error::other("no errno"))).unwrap_err();
    assert_eq!(err, Fallback::RingSetup { errno: 5 });
    let too_many = UringMemory::of(65_537, 1, 8192);
    let err = make_rings(too_many, &mut |_| panic!("no ring is made")).unwrap_err();
    assert_eq!(err, Fallback::RingSetup { errno: 22 }, "a qid past u16");
}

#[test]
fn the_reader_buffer_holds_a_whole_ring_request() {
    let memory = UringMemory::of(1, 1, 128 << 10);
    assert_eq!(reader_buffer(8192, &memory), 40 + 128 + (128 << 10));
    assert_eq!(reader_buffer(1 << 21, &memory), 1 << 21);
}

#[test]
fn init_asks_for_io_uring_only_when_the_plan_and_the_rings_allow_it() {
    let r = rig(host(2), depth(1), offered_uring());
    assert_ne!(reply_flags2(&r.init_reply) & OVER_IO_URING_HIGH, 0);
    assert!(
        r.session
            .negotiated()
            .granted
            .contains(InitFlags::OVER_IO_URING)
    );
    assert_eq!(
        r.session.transport(),
        TransportMode::Uring(UringMemory::of(2, 1, 8192))
    );
    let off = Host {
        enabled: false,
        ..host(2)
    };
    let r = rig(off, depth(1), offered_uring());
    assert_eq!(reply_flags2(&r.init_reply) & OVER_IO_URING_HIGH, 0);
    assert!(
        !r.session
            .negotiated()
            .granted
            .contains(InitFlags::OVER_IO_URING)
    );
    assert_eq!(
        r.session.transport(),
        TransportMode::DevFuse(Fallback::Disabled)
    );
    let r = rig(host(2), depth(1), InitFlags::INIT_EXT.0);
    assert_eq!(
        r.session.transport(),
        TransportMode::DevFuse(Fallback::NotOffered)
    );
}

#[test]
fn a_session_initialised_without_io_uring_says_it_was_not_asked() {
    let (device, dev) = fake_device();
    (&dev).write_all(&init_request(offered_uring())).unwrap();
    let session = Session::init(device, &small()).unwrap();
    assert_eq!(
        session.transport(),
        TransportMode::DevFuse(Fallback::NotAsked)
    );
    assert!(
        !session
            .negotiated()
            .granted
            .contains(InitFlags::OVER_IO_URING)
    );
}

#[test]
fn a_ring_setup_failure_answers_init_without_io_uring() {
    let (device, dev) = fake_device();
    (&dev).write_all(&init_request(offered_uring())).unwrap();
    let mut make = |_: &Arc<crate::DevFd>, _: UringMemory| Err(Fallback::RingSetup { errno: 1 });
    let config = UringConfig::default();
    let ask = AskUring {
        config: &config,
        host: host(2),
        make: &mut make,
    };
    let session = Session::init_with(device, &small(), Some(ask)).unwrap();
    let mut buf = [0u8; 4096];
    let n = (&dev).read(&mut buf).unwrap();
    assert_eq!(reply_flags2(&buf[..n]) & OVER_IO_URING_HIGH, 0);
    assert_eq!(
        session.transport(),
        TransportMode::DevFuse(Fallback::RingSetup { errno: 1 })
    );
}

#[test]
fn a_connection_granted_io_uring_is_not_resumed() {
    let r = rig(host(1), depth(1), offered_uring());
    let handover = r.session.handover().unwrap();
    let err = Session::resume(handover).unwrap_err();
    assert!(matches!(err, SessionError::UringNotResumable));
    assert_eq!(
        err.to_string(),
        "a FUSE connection served on io_uring cannot be taken over"
    );
}

#[test]
fn the_kernel_rings_are_one_per_queue_on_the_session_s_device() {
    // A pipe stands in for the device: making a ring names it but sends nothing.
    let (dev, _) = rustix::pipe::pipe().unwrap();
    let device = Arc::new(crate::DevFd::from_fd(dev));
    let memory = UringMemory::of(2, 1, 8192);
    let rings = kernel_rings(&device, memory);
    // Where io_uring is refused (EPERM or ENOSYS), the fallback says so.
    let refused = |f: &Fallback| matches!(f, Fallback::RingSetup { errno: 1 | 38 });
    let ok = rings.as_ref().map_or_else(refused, |r| r.queues.len() == 2);
    assert!(ok, "{rings:?}");
}

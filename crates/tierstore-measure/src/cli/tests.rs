use super::*;
use crate::clock::StepClock;
use crate::loopfs::tests::Script;
use crate::testutil::Fake;
use objstore_s3::ObjectKey;
use objstore_s3::ObjectStore as _;

/// Runs a command line with a scripted runner and a stepping clock; returns the exit status,
/// standard output and standard error.
fn cli(args: &[String], runner: &Script) -> (u8, String, String) {
    let clock = StepClock::new(Duration::from_millis(1), 1_000_000);
    let env = Env {
        runner,
        clock: &clock,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut argv = vec![OsString::from("ts-measure")];
    argv.extend(args.iter().map(OsString::from));
    let code = main_with(argv, &env, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

/// The flags every verb takes, pointed at the fake machine, with the idle check off.
fn common(f: &Fake) -> String {
    format!(
        "--out {} --proc-root {} --sys-root {} --idle-secs 0",
        f.path().join("out").display(),
        f.path().display(),
        f.sys().display()
    )
}

fn ok(f: &Fake, line: &str, runner: &Script) -> String {
    let (code, out, err) = cli(&args(&format!("{line} {}", common(f))), runner);
    assert_eq!(code, 0, "{line}: {err}");
    out
}

fn md(f: &Fake, arm: &str) -> String {
    std::fs::read_to_string(f.path().join("out").join(format!("{arm}.md"))).unwrap()
}

#[test]
fn help_and_bad_command_lines() {
    let s = Script::default();
    assert_eq!(cli(&args("--help"), &s).0, 0);
    assert_eq!(cli(&args("frobnicate"), &s).0, 2);
    let f = Fake::new("cli", &["sdb"]);
    let d = f.drive("d");
    f.link_dev(&d, "sdb");
    let (code, _, err) = cli(
        &args(&format!(
            "hdd --dir {} --patterns sideways {}",
            d.join("a").display(),
            common(&f)
        )),
        &s,
    );
    assert_eq!(code, 1);
    assert_eq!(
        err,
        "ts-measure: pattern sideways: want adjacent or scattered\n"
    );
    // The program's own entry point parses this test binary's arguments, which are not a verb.
    assert_ne!(super::main(), std::process::ExitCode::SUCCESS);
}

#[test]
fn facts_and_cpu() {
    let f = Fake::new("cli", &["sdb"]);
    std::fs::write(f.path().join("cpuinfo"), "flags : sse sha_ni\n").unwrap();
    let hdd = f.drive("hdd1");
    f.link_dev(&hdd, "sdb");
    let s = Script::default();
    // One part file of 16 MiB (sparse) under the named directory: it is mapped only when the
    // directory is named.
    let part = hdd.join("bucket/obj/uuid/part.1");
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::File::create(&part)
        .unwrap()
        .set_len(16 << 20)
        .unwrap();
    ok(&f, &format!("facts --node n3 --hdd {}", hdd.display()), &s);
    assert!(!md(&f, "facts-n3").contains("**fiemap "));
    ok(
        &f,
        &format!(
            "facts --node n3 --ssd sda --hdd {} --parts-dir bucket",
            hdd.display()
        ),
        &s,
    );
    let m = md(&f, "facts-n3");
    assert!(m.contains("**sha_ni**\n\n```\ntrue"), "{m}");
    assert!(m.contains(&format!("**xfs_info {}**", hdd.display())));
    assert!(m.contains(&format!("**fiemap {}**", part.display())), "{m}");
    ok(&f, "cpu --node n3 --ms 2", &s);
    assert!(md(&f, "cpu-n3").contains("sha_ni: yes"));
    let (code, _, _) = cli(
        &args(&format!(
            "facts --node n3 --hdd {} {}",
            f.path().join("none").display(),
            common(&f)
        )),
        &s,
    );
    assert_eq!(code, 1);
}

#[test]
fn hdd_prep_and_grid() {
    let f = Fake::new("cli", &["sdb"]);
    let d = f.drive("hdd1");
    f.link_dev(&d, "sdb");
    let s = Script::default();
    let a = d.join("a");
    let flags = format!(
        "--dir {} --sets one --sizes 1 --streams 1 --modes direct,ra512 --patterns adjacent,scattered --files 1 --file-mib 2 --warmup-ms 1 --measure-ms 20",
        a.display()
    );
    ok(&f, &format!("hdd-prep {flags}"), &s);
    ok(
        &f,
        &format!(
            "hdd {flags} --guard-comm silo --guard-write-dev sdb --guard-quiet-ms 1 --guard-max-wait-ms 1"
        ),
        &s,
    );
    assert!(md(&f, "a").contains("### A, raw HDD reads, drives: one"));
    let (code, _, err) = cli(
        &args(&format!("hdd {flags} --modes fast {}", common(&f))),
        &s,
    );
    assert_eq!(code, 1, "{err}");
    // The idle check runs when asked, over the fake counters (an idle drive).
    let (code, _, err) = cli(
        &args(&format!(
            "hdd {flags} {} --idle-secs 1",
            common(&f).replace(" --idle-secs 0", "")
        )),
        &s,
    );
    assert_eq!(code, 0, "{err}");
}

#[test]
fn cache_on_a_target() {
    let f = Fake::new("cli", &["sda"]);
    let d = f.drive("root");
    f.link_dev(&d, "sda");
    let s = Script {
        ..Script::default()
    };
    ok(
        &f,
        &format!(
            "cache --target rootfs={} --fstrim rootfs={} --blocks 128 --jobs 1 --warmup-ms 1 --measure-ms 10 --sync-commits 1 --read-set-mib 4 --punch-mib 32 --dontneed-mib 4 --dontneed-cap-mib 1",
            d.display(),
            d.display()
        ),
        &s,
    );
    assert!(md(&f, "b").contains("### B, cache device: punch"));
    for bad in ["--target nolabel", "--target x=/a --fstrim nolabel"] {
        let (code, _, _) = cli(&args(&format!("cache {bad} {}", common(&f))), &s);
        assert_eq!(code, 1, "{bad}");
    }
}

#[test]
fn cache_on_a_loop_target_samples_the_disk_under_the_image() {
    let f = Fake::new("cli", &["loop1"]);
    let d = f.drive("cachefs");
    f.link_dev(&d, "loop1");
    let img = f.path().join("cache.img");
    std::fs::write(&img, vec![0u8; 4096]).unwrap();
    let back = f.sys().join("block/loop1/loop");
    std::fs::create_dir_all(&back).unwrap();
    std::fs::write(back.join("backing_file"), format!("{}\n", img.display())).unwrap();
    let s = Script {
        ..Script::default()
    };
    ok(
        &f,
        &format!(
            "cache --target loop={} --blocks 128 --jobs 1 --warmup-ms 1 --measure-ms 5 --sync-commits 1 --read-set-mib 4 --punch-mib 32 --dontneed-mib 4",
            d.display()
        ),
        &s,
    );
    assert!(md(&f, "b").contains("image_freed_mib="));
}

#[test]
fn write_verbs_and_churn() {
    let f = Fake::new("cli", &["sdb"]);
    let d = f.drive("hdd1");
    f.link_dev(&d, "sdb");
    let s = Script {
        answers: vec![("xfs_io -r -c fiemap", " 0: [0..7]: 1..8\n".into())],
        ..Script::default()
    };
    let w = d.join("w");
    let flags = format!(
        "--dir {} --sizes 1 --writers 1 --measure-ms 5 --sustained-mib 2 --pack-mib 1 --window-mib 1 --rewrite-cap-mib 1 --keep-free 0",
        w.display()
    );
    ok(&f, &format!("write-cells {flags}"), &s);
    assert!(md(&f, "c5-cells").contains("size=1MiB writers=1"));
    ok(&f, &format!("write-sustained {flags}"), &s);
    assert!(md(&f, "c5-sustained").contains("rewrite after deletes over fresh"));
    // A churn that is over its band at once and has no turns to write: one fill window.
    ok(
        &f,
        &format!(
            "churn --dir {} --lo 0 --hi 0.0001 --sizes 1 --turns 0 --window-mib 1 --read-every-ms 1 --label t",
            d.display()
        ),
        &s,
    );
    assert!(md(&f, "c5-churn-t").contains("### C-5, churn summary: t"));
    let made = flags.replace(
        &w.display().to_string(),
        &f.path().join("f/x").display().to_string(),
    );
    ok(&f, &format!("write-cells {made}"), &s);
    assert!(
        f.path().join("f/x").is_dir(),
        "a missing scratch directory is made"
    );
    std::fs::write(f.path().join("file"), b"").unwrap();
    let (code, _, _) = cli(
        &args(&format!(
            "write-cells --dir {} {}",
            f.path().join("file/x").display(),
            common(&f)
        )),
        &s,
    );
    assert_eq!(code, 1);
}

#[test]
fn loopfs_verbs() {
    let f = Fake::new("cli", &[]);
    let s = Script::default();
    let img = f.path().join("img");
    let mnt = f.path().join("mnt");
    let (code, out, err) = cli(
        &args(&format!(
            "loopfs-setup --image {} --gib 1 --direct-io --mnt {}",
            img.display(),
            mnt.display()
        )),
        &s,
    );
    assert_eq!((code, out.as_str()), (0, "/dev/loop9\n"), "{err}");
    let (code, _, err) = cli(
        &args(&format!(
            "loopfs-teardown --image {} --dev /dev/loop9 --mnt {}",
            img.display(),
            mnt.display()
        )),
        &s,
    );
    assert_eq!(code, 0, "{err}");
}

#[test]
fn silo_list_get_and_analyse() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let fake = rt.block_on(objstore_s3::s3::fake::FakeS3::start());
    let store = fake.store(objstore_s3::Capabilities::default());
    for k in ["cas/a", "cas/b"] {
        rt.block_on(store.put_new(
            &ObjectKey::new(k).unwrap(),
            bytes::Bytes::from(vec![1u8; 2 << 20]),
            None,
        ))
        .unwrap();
    }
    let f = Fake::new("cli", &["sdb"]);
    let creds = f.path().join("env");
    std::fs::write(&creds, "MINIO_ROOT_USER=k\nMINIO_ROOT_PASSWORD=s\n").unwrap();
    let s = Script::default();
    let base = format!(
        "--endpoint {} --bucket tierstore-fake --credentials {}",
        fake.endpoint(),
        creds.display()
    );
    let out = ok(&f, &format!("silo-list {base}"), &s);
    assert_eq!(out, "2 objects\n");
    let listing = f.path().join("out/silo-list-tierstore-fake.tsv");
    // Resuming after the first key appends the second: the file then names it twice.
    let first = std::fs::read_to_string(&listing).unwrap();
    let k1 = first
        .lines()
        .next()
        .unwrap()
        .split('\t')
        .next()
        .unwrap()
        .to_string();
    let out = ok(&f, &format!("silo-list {base} --after {k1}"), &s);
    assert_eq!(out, "1 objects\n");
    let both = std::fs::read_to_string(&listing).unwrap();
    assert_eq!(both, format!("{first}{}\n", first.lines().nth(1).unwrap()));
    std::fs::write(&listing, &first).unwrap();
    ok(
        &f,
        &format!(
            "silo-get {base} --listing {} --min-pack-mib 1 --node n3 --start-at 1000001 --parts 1 --conc 1 --devs sdb --warmup-ms 1 --measure-ms 20",
            listing.display()
        ),
        &s,
    );
    assert!(md(&f, "c1-silo-n3-nodes1").contains("### C-1, silo ranged GETs: n3"));
    ok(&f, &format!("analyse --listing {}", listing.display()), &s);
    let m = md(&f, "c6-c7");
    assert!(
        m.contains("### C-6, silo: totals") && m.contains("### C-7, placement: copies per drive")
    );
    for bad in [
        format!(
            "silo-list --endpoint {} --bucket tierstore-fake --credentials {}",
            fake.endpoint(),
            f.path().join("none").display()
        ),
        format!(
            "silo-get {base} --listing {} --node n",
            f.path().join("none").display()
        ),
        format!("analyse --listing {}", f.path().join("none").display()),
        format!(
            "silo-list --endpoint nope --bucket tierstore-fake --credentials {}",
            creds.display()
        ),
    ] {
        let (code, _, _) = cli(&args(&format!("{bad} {}", common(&f))), &s);
        assert_eq!(code, 1, "{bad}");
    }
}

#[test]
fn cold_prep_local_serve_and_load() {
    let f = Fake::new("cli", &["sdb"]);
    let (d1, d2) = (f.drive("c1"), f.drive("c2"));
    f.link_dev(&d1, "sdb");
    let s = Script::default();
    let drives = format!(
        "--drive hdd1={} --drive hdd2={} --packs 1 --pack-mib 8",
        d1.display(),
        d2.display()
    );
    ok(&f, &format!("cold-prep {drives}"), &s);
    ok(
        &f,
        &format!(
            "cold-local {drives} --arms c4 --warmup-ms 1 --measure-ms 20 --guard-comm silo --guard-quiet-ms 1 --guard-max-wait-ms 1"
        ),
        &s,
    );
    let m = md(&f, "cold-local");
    assert!(m.contains("### C-4, cold-tier prototype"));
    assert!(m.contains("| silo MB/s | fw/s | attempts |"));
    // Every row records what the guard saw: here one clean attempt per cell.
    let rows = std::fs::read_to_string(f.path().join("out").join("cold-local.jsonl")).unwrap();
    assert_eq!(rows.lines().count(), 4);
    for l in rows.lines() {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        assert_eq!(v["seen"]["attempts"], 1, "{l}");
    }
    let (code, _, err) = cli(
        &args(&format!("cold-local {drives} --arms c9 {}", common(&f))),
        &s,
    );
    assert_eq!(code, 1);
    assert!(err.contains("arm c9"));
    // A server for the load verb, on loopback, with the prototype's defaults.
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let b = crate::cold::arms::base("hdd1");
    let server = crate::cold::server::Server::new(
        crate::cold::server::Config {
            drives: [("hdd1".to_string(), d1.clone())].into(),
            params: b.params,
            chunk: b.chunk,
            path: b.path,
            release_after: None,
        },
        &clock,
    );
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let h = scope.spawn(|| server.serve(&l, &stop));
        let _stop = crate::testutil::StopOnDrop(&stop);
        ok(
            &f,
            &format!(
                "cold-load --addr {addr} --label peer --packs 1 --pack-mib 8 --background 1 --part-mib 2 --every-ms 5 --start-at 1000000 --warmup-ms 1 --measure-ms 20"
            ),
            &s,
        );
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        h.join().unwrap().unwrap();
    });
    assert!(md(&f, "cold-load-peer").contains("| peer |"));
    ok(
        &f,
        &format!("cold-serve {drives} --listen 127.0.0.1:0 --secs 1"),
        &s,
    );
    assert_eq!(
        std::fs::read_to_string(f.path().join("out/cold-serve.jsonl")).unwrap(),
        "[]\n"
    );
    let (code, _, _) = cli(
        &args(&format!(
            "cold-serve {drives} --listen 192.0.2.1:9 --secs 0 {}",
            common(&f)
        )),
        &s,
    );
    assert_eq!(code, 1, "an address not on this machine cannot be bound");
    let (code, _, _) = cli(&args(&format!("cold-prep --drive bad {}", common(&f))), &s);
    assert_eq!(code, 1);
}

use super::*;
use crate::loopfs::tests::Script;
use crate::testutil::Fake;

fn fake_proc(f: &Fake) -> PathBuf {
    let p = f.path().join("proc");
    for (pid, comm) in [("10", "js-agent"), ("11", "other"), ("12", "js-agent")] {
        let d = p.join(pid);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
        if pid == "10" {
            std::fs::write(
                d.join("limits"),
                "Limit Soft Hard Units\nMax locked memory         8388608  8388608  bytes\n",
            )
            .unwrap();
            std::fs::write(d.join("status"), "Name: js-agent\nSeccomp:\t0\n").unwrap();
        }
    }
    std::fs::create_dir_all(p.join("sys/fs/fuse")).unwrap();
    std::fs::write(p.join("sys/fs/fuse/max_request_timeout"), "0\n").unwrap();
    std::fs::create_dir_all(p.join("sys/fs/fuse/sub")).unwrap();
    std::fs::create_dir_all(p.join("sys/vm")).unwrap();
    std::fs::write(p.join("sys/vm/dirty_ratio"), "20\n").unwrap();
    std::fs::write(p.join("cmdline"), "ro quiet\n").unwrap();
    p
}

#[test]
fn node_facts_read_files_dirs_processes_and_commands() {
    let f = Fake::new("facts", &["sdb"]);
    let proc_ = fake_proc(&f);
    let probes = node_probes(&proc_, &f.sys(), Some("sda"));
    assert!(
        probes
            .iter()
            .any(|p| p.name == "ssd partitions and free space")
    );
    assert_eq!(node_probes(&proc_, &f.sys(), None).len(), probes.len() - 1);
    let s = Script {
        answers: vec![("uname -r", "7.0.0-test\n".into())],
        ..Script::default()
    };
    let facts = collect(&probes, &proc_, &s);
    assert_eq!(facts["uname -r"], "7.0.0-test");
    assert_eq!(facts["kernel cmdline"], "ro quiet");
    assert_eq!(facts["vm.dirty_ratio"], "20");
    assert!(facts["vm.dirty_bytes"].starts_with("error: "));
    assert_eq!(
        facts["fuse sysctl (request timeouts)"],
        "max_request_timeout=0\nsub=<Is a directory (os error 21)>"
    );
    assert!(facts["fuse module parameters"].starts_with("error: "));
    assert_eq!(
        facts["js-agent limits and seccomp"],
        "pid 10: Max locked memory 8388608 8388608 bytes; Seccomp: 0\npid 12: Max locked memory ?; Seccomp: ?"
    );
    assert_eq!(
        facts["crun limits and seccomp"],
        "error: no process named crun"
    );
    assert!(
        s.seen
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "parted -s -m /dev/sda unit GiB print free")
    );
    let md = markdown("E, node facts", &facts);
    assert!(md.starts_with(
        "### E, node facts\n\n**crun limits and seccomp**\n\n```\nerror: no process named crun\n```"
    ));
}

#[test]
fn drive_facts_name_the_drive_and_each_sampled_part() {
    let f = Fake::new("facts", &["sdb"]);
    let parts = vec![PathBuf::from("/m/kbf-cas/x/part.1")];
    let probes = drive_probes(&f.sys(), Path::new("/m"), "sdb", &parts);
    let names: Vec<&str> = probes.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "xfs_info /m",
            "lsblk -t -D sdb",
            "free space extents sdb",
            "queue sdb",
            "df /m",
            "fiemap /m/kbf-cas/x/part.1"
        ]
    );
    let facts = collect(&probes, f.path(), &Script::default());
    assert_eq!(facts["queue sdb"], "read_ahead_kb=8192");
    let fail = Script {
        fail: vec!["xfs_info"],
        ..Script::default()
    };
    let facts = collect(&probes, f.path(), &fail);
    assert!(facts["xfs_info /m"].starts_with("error: command `xfs_info /m`"));
    assert!(read_process(&f.path().join("none"), "x").is_err());
}

#[test]
fn sample_parts_finds_big_part_files_within_its_budget() {
    let f = Fake::new("facts", &[]);
    let root = f.path().join("kbf-cas");
    for (i, len) in [(0, 10u64), (1, 4096), (2, 4096)] {
        let d = root.join(format!("obj{i}/uuid"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("part.1"), vec![0u8; usize::try_from(len).unwrap()]).unwrap();
        std::fs::write(d.join("xl.meta"), b"m").unwrap();
    }
    let all = sample_parts(&root, 10, 1000, 1000);
    assert_eq!(all.len(), 2);
    assert_eq!(sample_parts(&root, 1, 1000, 1000).len(), 1);
    assert!(sample_parts(&root, 10, 1000, 2).is_empty());
    assert!(sample_parts(&f.path().join("none"), 10, 0, 100).is_empty());
}

//! The store: layout, `flock`, open, remove, the holder registry and `attach` (section 2, 7; 5.1).
//!
//! Section 14 rows: "No eviction before the grace window; an entry locked by a second process is
//! never evicted" (the lock half: the grace window is the evictor's, T8); "Eviction removes an
//! unheld entry's path while a mount holds the data fd"; "Mount killed (lifeline hangup) ..."
//! (`drop_holder`).
//!
//! Catches: two openers of one data file (a second store, which is what a second process is to
//! `flock`), or an opener that changes the file before it wins the lock; a second entry for a pack,
//! another size accepted, or a file made for a pack too large to index; a removal of a held entry,
//! or one that cuts off a mount still reading the old inode; a file name that two pack ids share;
//! an attach that skips a check, takes a second hold for the same region, or survives a
//! `drop_holder`.

use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::Arc;

use tierstore_fetch_api::entry::{Entry, EntryError, EntryInfo, EntryStore, HolderCounters};
use tierstore_fetch_api::fake::MemCounters;
use tierstore_fetch_api::ids::{HolderId, PackId, PackKey, PackRoot};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_format::ids::Generation;

use super::{MIB, bytes, open, pack, publish, scratch, spec};
use crate::layout::file_hash;
use crate::{FileEntryStore, PACKS_DIR, StoreError};

#[test]
fn an_entry_lives_at_packs_v2_aa_bb_hash_with_its_journal_beside_it() {
    let dir = scratch("store-layout");
    let (store, e) = open(&dir, "p", MIB);
    let hash = "7eedbd0f1327a25265a713754fe186a3fd09690c3faf1fbcc6b3b4196d6a7096";
    assert_eq!(file_hash(&pack("p")), hash);
    let base = dir.join(PACKS_DIR).join("7e").join("ed");
    assert_eq!(store.dir(), dir.join(PACKS_DIR));
    assert_eq!(e.data_path(), base.join(format!("{hash}.pack")));
    assert_eq!(e.journal_path(), base.join(format!("{hash}.jnl")));
    assert!(e.data_path().is_file());
    assert!(e.journal_path().is_file());
}

#[test]
fn a_digest_root_and_an_etag_root_of_the_same_bytes_name_different_files() {
    let etag = PackId {
        key: PackKey("p".to_owned()),
        root: PackRoot::Etag(Generation([2; 16])),
    };
    assert_eq!(
        file_hash(&etag),
        "7ec4b9f4dbbd8c53641055669ce057a5c073395ae837b34fe241a6322bf4c953"
    );
    assert_ne!(file_hash(&etag), file_hash(&pack("p")));
}

#[test]
fn opening_a_pack_twice_gives_the_same_entry_and_another_size_is_refused() {
    let dir = scratch("store-open-twice");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let a = store.open(&spec("p", 2 * MIB)).unwrap();
    let b = store.open(&spec("p", 2 * MIB)).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    let err = store.open(&spec("p", 3 * MIB)).err().unwrap();
    assert!(
        matches!(err, EntryError::SizeMismatch { size, existing, .. } if size == 3 * MIB && existing == 2 * MIB),
        "{err}"
    );
    assert_eq!(store.packs(), vec![pack("p")]);
    assert!(store.get(&pack("p")).is_some());
    assert!(store.get(&pack("q")).is_none());
}

#[test]
fn a_pack_of_more_than_u32_max_cells_is_refused_before_any_file_is_made() {
    let dir = scratch("store-too-large");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let size = (u64::from(u32::MAX) + 1) * MIB;
    let err = store.open(&spec("huge", size)).err().unwrap();
    assert!(matches!(err, EntryError::TooLarge { .. }), "{err}");
    assert!(store.packs().is_empty());
    assert_eq!(std::fs::read_dir(store.dir()).unwrap().count(), 1);
}

#[test]
fn a_second_store_cannot_open_a_locked_entry_and_changes_nothing_of_it() {
    let dir = scratch("store-flock");
    let (first, e) = open(&dir, "p", 2 * MIB);
    publish(&e, 0, 0x33);
    e.commit_batch().unwrap();
    let journal_len = std::fs::metadata(e.journal_path()).unwrap().len();
    let second = FileEntryStore::new(dir.path()).unwrap();
    let err = second.open(&spec("p", 2 * MIB)).err().unwrap();
    assert!(matches!(err, EntryError::Io { op: "flock", .. }), "{err}");
    assert!(second.packs().is_empty());
    // The first store's entry is intact: bytes, size and journal.
    assert_eq!(e.read_cell(0).unwrap(), bytes(0x33, MIB));
    assert_eq!(std::fs::metadata(e.data_path()).unwrap().len(), 2 * MIB);
    assert_eq!(
        std::fs::metadata(e.journal_path()).unwrap().len(),
        journal_len
    );
    // Once the first store and its entry are gone, the lock is free.
    drop(e);
    drop(first);
    second.open(&spec("p", 2 * MIB)).unwrap();
}

#[test]
fn reopening_an_entry_left_on_disk_starts_it_empty_with_every_cell_absent() {
    let dir = scratch("store-reopen");
    {
        let (_store, e) = open(&dir, "p", 2 * MIB);
        publish(&e, 1, 0x44);
        e.commit_batch().unwrap();
    }
    let (_store, e) = open(&dir, "p", 2 * MIB);
    assert_eq!(e.counts().absent, 2);
    assert_eq!(std::fs::metadata(e.data_path()).unwrap().len(), 2 * MIB);
    assert_eq!(std::fs::metadata(e.data_path()).unwrap().blocks(), 0);
    assert_eq!(std::fs::metadata(e.journal_path()).unwrap().len(), 0);
}

#[test]
fn removing_an_unheld_entry_unlinks_its_files_while_a_mount_keeps_reading_the_old_inode() {
    let dir = scratch("store-remove");
    let (store, e) = open(&dir, "p", 2 * MIB);
    publish(&e, 0, 0x55);
    let mount_fd = std::fs::File::from(e.fds().unwrap().data.try_clone_to_owned().unwrap());
    let old_inode = std::fs::metadata(e.data_path()).unwrap().ino();
    let (data, journal) = (e.data_path().to_owned(), e.journal_path().to_owned());
    assert!(store.remove(&pack("p")).unwrap());
    assert!(!data.exists());
    assert!(!journal.exists());
    assert!(store.packs().is_empty());
    assert!(store.get(&pack("p")).is_none());
    let mut buf = vec![0u8; MIB as usize];
    mount_fd.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(buf, bytes(0x55, MIB));
    // The next open makes a new file (a new inode, with its own lock), even while the removed
    // entry is still referenced.
    let fresh = store.open(&spec("p", 2 * MIB)).unwrap();
    assert_ne!(std::fs::metadata(&data).unwrap().ino(), old_inode);
    assert_eq!(fresh.counts().absent, 2);
    assert_eq!(e.read_cell(0).unwrap(), bytes(0x55, MIB));
}

#[test]
fn a_held_or_unknown_entry_is_not_removed() {
    let dir = scratch("store-remove-held");
    let (store, e) = open(&dir, "p", MIB);
    e.hold(HolderId(1));
    assert!(!store.remove(&pack("p")).unwrap());
    assert!(e.data_path().exists());
    assert!(!store.remove(&pack("q")).unwrap());
    e.release(HolderId(1));
    assert!(store.remove(&pack("p")).unwrap());
}

#[test]
fn a_removal_whose_data_file_is_already_gone_still_unlinks_the_journal() {
    let dir = scratch("store-remove-gone");
    let (store, e) = open(&dir, "p", MIB);
    std::fs::remove_file(e.data_path()).unwrap();
    assert!(store.remove(&pack("p")).unwrap());
    assert!(!e.journal_path().exists());
    assert!(store.packs().is_empty());
}

#[test]
fn a_failed_unlink_keeps_the_entry_listed_and_a_retry_finishes_the_removal() {
    let dir = scratch("store-remove-fail");
    let (store, e) = open(&dir, "p", MIB);
    // A directory where the journal was: unlink(2) of it fails (EISDIR), even as root.
    std::fs::remove_file(e.journal_path()).unwrap();
    std::fs::create_dir(e.journal_path()).unwrap();
    let err = store.remove(&pack("p")).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::Io {
                op: "unlink entry file",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(store.packs(), vec![pack("p")]);
    assert!(!e.data_path().exists());
    std::fs::remove_dir(e.journal_path()).unwrap();
    assert!(store.remove(&pack("p")).unwrap());
    assert!(store.packs().is_empty());
}

#[test]
fn file_system_failures_of_an_open_name_their_step() {
    let dir = scratch("store-open-fail");
    let store = FileEntryStore::new(dir.path()).unwrap();
    // A file where the entry's directory should be.
    let hash = file_hash(&pack("a"));
    std::fs::write(store.dir().join(&hash[0..2]), b"").unwrap();
    let err = store.open(&spec("a", MIB)).err().unwrap();
    assert!(
        matches!(
            err,
            EntryError::Io {
                op: "make entry directory",
                ..
            }
        ),
        "{err}"
    );
    // A directory where the data file should be.
    let hash = file_hash(&pack("b"));
    let base = store.dir().join(&hash[0..2]).join(&hash[2..4]);
    std::fs::create_dir_all(base.join(format!("{hash}.pack"))).unwrap();
    let err = store.open(&spec("b", MIB)).err().unwrap();
    assert!(
        matches!(
            err,
            EntryError::Io {
                op: "open data file",
                ..
            }
        ),
        "{err}"
    );
    // A directory where the journal should be.
    let hash = file_hash(&pack("c"));
    let base = store.dir().join(&hash[0..2]).join(&hash[2..4]);
    std::fs::create_dir_all(base.join(format!("{hash}.jnl"))).unwrap();
    let err = store.open(&spec("c", MIB)).err().unwrap();
    assert!(
        matches!(
            err,
            EntryError::Io {
                op: "create journal",
                ..
            }
        ),
        "{err}"
    );
    assert!(store.packs().is_empty());
}

#[test]
fn a_store_whose_directory_cannot_be_made_names_it() {
    let dir = scratch("store-new-fail");
    std::fs::write(dir.join("cache"), b"").unwrap();
    let err = FileEntryStore::new(&dir.join("cache")).unwrap_err();
    assert!(matches!(err, StoreError::Dir { .. }), "{err}");
    assert!(err.to_string().starts_with(&format!(
        "making {}: ",
        dir.join("cache").join(PACKS_DIR).display()
    )));
}

#[test]
fn entries_reports_every_entry_in_pack_order() {
    let dir = scratch("store-entries");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let b = store.open(&spec("b", 3 * MIB)).unwrap();
    store.open(&spec("a", MIB)).unwrap();
    publish(&store.file_entry(&pack("b")).unwrap(), 2, 1);
    b.touch(4_000, 0);
    b.hold(HolderId(1));
    let infos = store.entries();
    assert_eq!(infos.len(), 2);
    assert_eq!(infos[0].pack, pack("a"));
    assert_eq!(
        infos[1],
        EntryInfo {
            pack: pack("b"),
            size: 3 * MIB,
            last_use: 4_000,
            holders: 1,
            counts: b.counts(),
        }
    );
    assert_eq!(infos[1].counts.present, 1);
}

#[test]
fn attach_checks_the_pack_then_the_holder_then_the_region() {
    let dir = scratch("store-attach-checks");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let e = store.open(&spec("p", 20 * MIB)).unwrap();
    let region = CounterRegion {
        offset: 0,
        groups: 2,
    };
    let err = store.attach(&pack("q"), HolderId(1), region).unwrap_err();
    assert!(matches!(err, EntryError::UnknownPack { .. }), "{err}");
    let err = store.attach(&pack("p"), HolderId(1), region).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::UnknownHolder {
                holder: HolderId(1)
            }
        ),
        "{err}"
    );
    let counters = Arc::new(MemCounters::new(4096));
    store
        .register_holder(HolderId(1), counters.clone())
        .unwrap();
    for bad in [
        CounterRegion {
            offset: 4,
            groups: 2,
        },
        CounterRegion {
            offset: 0,
            groups: 1,
        },
        CounterRegion {
            offset: 0,
            groups: 3,
        },
        CounterRegion {
            offset: u64::MAX - 7,
            groups: 2,
        },
    ] {
        let err = store.attach(&pack("p"), HolderId(1), bad).unwrap_err();
        assert!(
            matches!(err, EntryError::BadRegion { region, .. } if region == bad),
            "{err}"
        );
    }
    assert_eq!(e.holders(), 0);
    store.attach(&pack("p"), HolderId(1), region).unwrap();
    // The same region again: no second hold. Another region for the same holder: refused.
    store.attach(&pack("p"), HolderId(1), region).unwrap();
    assert_eq!(e.holders(), 1);
    let other = CounterRegion {
        offset: 8,
        groups: 2,
    };
    let err = store.attach(&pack("p"), HolderId(1), other).unwrap_err();
    assert!(
        matches!(err, EntryError::BadRegion { region, .. } if region == other),
        "{err}"
    );
    counters.set(2, 3);
    assert_eq!(e.group_readers(1), 3);
}

#[test]
fn drop_holder_forgets_counters_regions_and_holds_on_every_entry() {
    let dir = scratch("store-drop-holder");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let a = store.open(&spec("a", MIB)).unwrap();
    let b = store.open(&spec("b", MIB)).unwrap();
    store.open(&spec("c", MIB)).unwrap();
    let counters = Arc::new(MemCounters::new(4096));
    store
        .register_holder(HolderId(5), counters.clone())
        .unwrap();
    store
        .attach(
            &pack("a"),
            HolderId(5),
            CounterRegion {
                offset: 0,
                groups: 1,
            },
        )
        .unwrap();
    // A Prepare hold on b by the same holder id, with no region.
    b.hold(HolderId(5));
    a.hold(HolderId(6));
    counters.set(0, 1);
    assert_eq!(a.group_readers(0), 1);
    assert_eq!(store.drop_holder(HolderId(5)), 2);
    assert_eq!(a.group_readers(0), 0);
    assert_eq!(a.holders(), 1);
    assert_eq!(b.holders(), 0);
    assert_eq!(counters.len(), 4096);
    assert_eq!(store.drop_holder(HolderId(5)), 0);
    // The holder may register again, as a new mount process with a new memfd.
    let fresh = Arc::new(MemCounters::new(4096));
    store.register_holder(HolderId(5), fresh.clone()).unwrap();
    store
        .attach(
            &pack("a"),
            HolderId(5),
            CounterRegion {
                offset: 8,
                groups: 1,
            },
        )
        .unwrap();
    fresh.set(8, 2);
    assert_eq!(a.group_readers(0), 2);
}

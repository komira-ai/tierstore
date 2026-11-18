//! The descriptors of `open-pack` (5.1, 5.4): the pack's data fd and its sealed state memfd travel
//! with `PackOpened` over `SCM_RIGHTS`, and the mount reads through them, never by path.
//!
//! Proves: `PackOpened` carries exactly two descriptors, the entry's data file and state memfd (the
//! same inodes), with the pack's size and the region asked; the data fd reads the pack's bytes;
//! the state memfd carries every seal of `MOUNT_SEALS`, refuses `write(2)` and holds the layout of
//! the pack; a re-sent `open-pack` returns the same handle and fresh descriptors of the same files
//! with no second hold; a data fd the mount holds keeps reading its entry's bytes after the entry
//! is removed from the store and a new entry of the same pack is made (section 14: "Eviction
//! removes an unheld entry's path while a mount holds the data fd: reads return that entry's
//! bytes").

use std::os::fd::{AsFd, OwnedFd};

use crate::common::{Fixture, MountProc, PACKS, inode, layout, pack_byte, pack_id, region};
use rustix::fs::fcntl_get_seals;
use rustix::io::{Errno, pread, write};
use tierstore_fetch_api::entry::EntryStore;
use tierstore_fetch_api::page::{StatePageLayout, sealed_for_mounts};
use tierstore_fetch_api::wire::{Message, PackHandle};

fn read_at(fd: &OwnedFd, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = pread(fd, &mut buf, offset).unwrap();
    buf.truncate(n);
    buf
}

fn expect_opened(message: Message) -> (PackHandle, u64) {
    match message {
        Message::PackOpened {
            pack, pack_size, ..
        } => (pack, pack_size),
        other => panic!("open-pack: {other:?}"),
    }
}

#[tokio::test]
async fn open_pack_passes_the_entry_data_fd_and_sealed_state_memfd() {
    let fx = Fixture::new();
    let mount = MountProc::new(5, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let (reply, fds) = conn.call(mount.open(1, 64), &[]).await;
    assert_eq!(
        reply.message,
        Message::PackOpened {
            pack: PackHandle(1),
            pack_size: PACKS[1].1,
            region: region(1, 64),
        }
    );
    let [data, state]: [OwnedFd; 2] = fds.try_into().unwrap();
    let entry = fx.store.entry(1);
    assert_eq!(inode(data.as_fd()), inode(entry.data_fd()));
    assert_eq!(inode(state.as_fd()), inode(entry.state_fd()));

    // The data fd reads the pack: its first bytes, bytes across a cell boundary, its last byte.
    let size = PACKS[1].1;
    assert_eq!(
        read_at(&data, 0, 4),
        (0..4).map(pack_byte).collect::<Vec<_>>()
    );
    let boundary = (1 << 20) - 2;
    assert_eq!(
        read_at(&data, boundary, 4),
        (boundary..boundary + 4).map(pack_byte).collect::<Vec<_>>()
    );
    assert_eq!(read_at(&data, size - 1, 8), vec![pack_byte(size - 1)]);

    // The state memfd is read-only for every mount and is the pack's page.
    let seals = fcntl_get_seals(&state).unwrap().bits();
    assert!(sealed_for_mounts(seals), "seals {seals:#x}");
    assert_eq!(write(&state, b"x"), Err(Errno::PERM));
    let header = read_at(&state, 0, 64);
    assert_eq!(StatePageLayout::from_header(&header), Some(layout(size)));
}

#[tokio::test]
async fn a_resent_open_pack_returns_the_same_handle_and_files_and_takes_no_second_hold() {
    let fx = Fixture::new();
    let mount = MountProc::new(5, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let (first, first_fds) = conn.call(mount.open(0, 0), &[]).await;
    let (second, second_fds) = conn.call(mount.open(0, 0), &[]).await;
    assert_eq!(first.message, second.message);
    assert_eq!(fx.store.holders_of(0), 1);
    assert_eq!(fx.trees.calls(), 1);
    for (a, b) in first_fds.iter().zip(&second_fds) {
        assert_eq!(inode(a.as_fd()), inode(b.as_fd()));
    }
}

#[tokio::test]
async fn a_held_data_fd_reads_its_entry_after_the_entry_is_removed() {
    let fx = Fixture::new();
    let mount = MountProc::new(5, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let (_, fds) = conn.call(mount.open(0, 0), &[]).await;
    let old_inode = inode(fds[0].as_fd());

    // The mount releases its holds; the evictor removes the unheld entry.
    let (reply, _) = conn
        .call(
            Message::Release {
                holder: mount.holder,
            },
            &[],
        )
        .await;
    assert_eq!(reply.message, Message::Released);
    assert!(fx.store.remove(&pack_id(0)).unwrap());

    // Another mount opens the same pack: a new entry, a new file.
    let other = MountProc::new(6, 4096);
    let mut conn2 = fx.connect();
    assert_eq!(conn2.handshake(&other).await, Message::HandshakeOk);
    let (reply, new_fds) = conn2.call(other.open(0, 0), &[]).await;
    expect_opened(reply.message);
    assert_ne!(inode(new_fds[0].as_fd()), old_inode);

    // The first mount's descriptor still reads the bytes of the entry it was given.
    assert_eq!(
        read_at(&fds[0], 10, 3),
        (10..13).map(pack_byte).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_pack_opened_on_one_connection_is_sent_again_on_another() {
    let fx = Fixture::new();
    let mount = MountProc::new(5, 4096);
    let mut first = fx.connect();
    assert_eq!(first.handshake(&mount).await, Message::HandshakeOk);
    let (a, a_fds) = first.call(mount.open(2, 8), &[]).await;
    let mut second = fx.connect();
    assert_eq!(second.handshake(&mount).await, Message::HandshakeOk);
    let (b, b_fds) = second.call(mount.open(2, 8), &[]).await;
    assert_eq!(expect_opened(a.message), expect_opened(b.message));
    assert_eq!(inode(a_fds[1].as_fd()), inode(b_fds[1].as_fd()));
}

#[tokio::test]
async fn a_pack_whose_descriptors_are_gone_is_not_sent_again() {
    let fx = Fixture::new();
    let mount = MountProc::new(5, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let (first, _) = conn.call(mount.open(0, 0), &[]).await;
    expect_opened(first.message);
    fx.store
        .entry(0)
        .hide_fds
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (again, fds) = conn.call(mount.open(0, 0), &[]).await;
    assert_eq!(
        again.message,
        Message::Refused {
            code: tierstore_fetch_api::wire::RefusalCode::UnknownPack
        }
    );
    assert!(fds.is_empty());
}

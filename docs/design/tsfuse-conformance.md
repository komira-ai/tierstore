# The tsfuse kernel conformance suite

Row T-F7 of `docs/design/fetch-cache-serve.md`: the kernel conformance suite of 5.2 ("Kernel
conformance tests, on a farm node", items 1 to 11). This page says which test proves each item,
and which parts wait for which task. The suite is the integration targets of `crates/tsfuse`
named `conformance_*_integration`, plus the earlier targets the table names. They drive the
kernel through the library: `Mounter`, `Session::init`, cloned readers, `Session::serve` and
`Session::resume`, and `ReplyPath`. The file system is the suite's `Model`
(`crates/tsfuse/tests/conformance/mod.rs`), which serves an expected tree the way the tree mount
does. The raw checks of `docs/design/tsfuse-kernel-findings.md` (K1 to K26) answer the kernel by
hand. The suite checks the same facts through the code that serves them.

## Where it runs

Every target mounts through `mount(2)` as root (a farm node) or `fusermount3` as a user (CI's test
job, which installs `fuse3`). A test that cannot run says why on stderr and does not run. That
happens where `/dev/fuse` cannot be opened, or neither root nor `fusermount3` is available. It
also happens for a check that needs root (the abort file, `allow_other` with another user), a
flag the kernel did not offer, or a kernel older than 6.14 for the check that names that version.
Every wait is on an event (a request handled, a caller's system call returning, a child process
ending, a line from a child), never on a timer. The privileged farm environment (row T-ENV)
runs the same targets as root. Until it exists, they are run on a farm node by hand, as root
and as the unprivileged user.

## Items

| 5.2 item | Tests (target: test) |
|---|---|
| 1. INIT: what the kernel offers; negotiated down; the flag list | `conformance_init_integration`: `a_tree_mount_and_a_put_mount_are_granted_exactly_what_was_asked_and_offered_with_the_configured_limits`, `a_kernel_of_6_14_or_later_offers_every_flag_the_tree_and_the_put_mount_require`, `an_enosys_open_is_the_only_open_of_two_opened_and_read_files_and_no_release_is_sent`, `an_enosys_opendir_is_the_only_opendir_of_two_listings_and_no_releasedir_is_sent`, `a_symlink_read_twice_sends_one_readlink_when_the_kernel_caches_symlinks`. Negotiation against an older kernel (a smaller minor, a missing flag) cannot be made by a real kernel; `tests/abi_init.rs` covers it on synthetic INIT requests |
| 2. LOOKUP, GETATTR, READDIR, READDIRPLUS equal an expected tree | `conformance_init_integration`: `a_directory_larger_than_one_reply_is_listed_whole_over_requests_that_resume_after_the_entries_sent`, `stat_of_the_root_a_directory_a_file_and_a_symlink_equals_the_tree_and_a_second_stat_asks_nothing`, `after_a_readdirplus_listing_a_stat_of_each_child_sends_no_lookup`, `a_missing_name_is_enoent_after_one_lookup_and_a_name_of_256_bytes_reaches_the_server_which_decides` |
| 3. READ: every range shape, holes, short files, by hash; writev and splice; `out.len` | `conformance_read_integration`: `direct_preads_of_every_range_shape_are_answered_byte_exact_by_splice`, `direct_preads_of_every_range_shape_are_answered_byte_exact_by_writev`, `ranges_over_holes_read_zeros_there_and_the_file_elsewhere`, `a_short_file_and_an_empty_file_read_whole_return_their_bytes`, `a_reply_is_as_long_as_the_bytes_the_data_file_had`, `buffered_reads_of_a_mount_without_open_are_whole_and_no_larger_than_max_readahead`, `spliced_replies_of_four_readers_answering_at_once_each_reach_their_caller`; also `kernel_reply_integration` (T-F4) |
| 4. INTERRUPT | Direct READs: `kernel_dispatch_integration`: `an_interrupt_read_by_another_clone_answers_each_parked_read_eintr_once` (a killed caller ends once its READ is answered `EINTR` on its own clone, the INTERRUPT read by another). Buffered READs: `conformance_load_integration`: `a_killed_reader_of_a_buffered_read_ends_with_no_interrupt_and_the_read_stays_parked` (K8). The `EAGAIN` answer to an INTERRUPT of a request read but not yet registered is proved on the kernel by `kernel_interrupt_integration` (K7), and the library's answer on every clone by `tests/interrupt.rs` and `tests/dispatch.rs` on fake devices. A `Session` reader registers a request in the same step that reads it, so no test can hold that window open through the library |
| 5. Parked replies completed from other tasks, N > 1 clones, out of order | `kernel_dispatch_integration`: `reads_parked_on_four_clones_and_completed_by_tasks_out_of_order_reach_their_callers`; under load, `conformance_load_integration`: `eight_callers_making_sixteen_direct_reads_each_get_exactly_their_bytes_from_four_readers` |
| 6. N readers under load, no lost or duplicated reply; a stop ends every reader | `conformance_load_integration`: `eight_callers_making_sixteen_direct_reads_each_get_exactly_their_bytes_from_four_readers`, `stopping_ends_every_reader_and_leaves_parked_replies_answerable_on_their_own_device`, `a_stop_made_while_a_reader_is_blocked_inline_ends_it_once_its_handler_returns` |
| 7. Takeover with N > 1 readers, a server killed mid-load (`/dev/fuse`) | `conformance_takeover_integration`: `a_killed_server_with_a_read_parked_on_every_clone_is_resumed_with_no_error_and_each_read_answered_once`, `a_clone_the_supervisor_did_not_keep_loses_its_parked_read_to_econnaborted_when_the_server_dies`; the cooperative hand-over in one process, `takeover_integration` (T-F6) |
| 8. Unmount while busy, abort, mount table; the ported fuser mount tests | `conformance_mount_integration`: `a_tree_mount_is_in_the_mount_table_with_its_options_and_its_unmount_removes_the_entry`, `a_put_mount_is_writable_in_the_mount_table_and_its_unmount_removes_the_entry`, `the_connection_of_a_mount_is_a_directory_of_the_control_file_system_until_it_is_unmounted`, `an_unmount_of_a_busy_mount_fails_and_leaves_it_and_a_lazy_unmount_detaches_it`, `an_abort_with_a_read_parked_fails_the_caller_and_the_readers_and_leaves_the_mount_to_unmount`, `a_killed_server_leaves_a_mount_that_answers_enotconn_and_unmounts`, `with_allow_other_another_user_stats_the_root_and_opens_a_file_of_the_mount`, `without_allow_other_another_user_is_refused_with_eacces_on_the_root_and_on_a_file`, `a_mount_forced_through_fusermount3_is_served_and_unmounted_through_it`, `a_busy_mount_made_and_unmounted_through_fusermount3_fails_to_unmount_and_detaches_lazily`, `the_abort_of_a_mount_that_is_gone_is_an_error_naming_the_abort_file_and_writes_nothing`; also `mount_integration` (T-F2) |
| 9. io_uring: registration, fallback, the storm | `kernel_uring_integration` (T-F5): `a_session_asked_for_io_uring_serves_on_the_transport_the_kernel_allows` (with `fuse.enable_uring` off, the fallback to `/dev/fuse`; with it on, every possible CPU's queue registered and every READ read by a queue's thread), `a_tree_mount_of_1_mib_requests_never_asks_for_io_uring` (the 64 MiB rule); the registration order, a refused REGISTER and the fallback on fake rings in `src/uring/tests/`. Every farm node has `N`, so on the farm the on case runs only where the parameter is set for the run (T-ENV). The storm is T-B3's: `fsbench storm` (`crates/fsbench/tests/storm_integration.rs` runs it on both transports), measured in `docs/measurements/2026-11-fuse-v5-storm.md` (io_uring does not pass it; `/dev/fuse` does) |
| 10. ABI layout probe equals our structs | `abi_layout`: `every_wire_struct_has_the_layout_of_the_farm_probe`; `abi_probe`: `the_probe_prints_exactly_the_checked_in_layout` (needs a C compiler); the running kernel's minor within the pinned ABI's range, in item 1's first test |
| 11. A `PROT_WRITE`, `MAP_SHARED` mapping of the passed state memfd fails `EPERM` | In the crate that makes the state memfd, `tierstore-entry`: `a_writable_shared_mapping_of_the_passed_state_memfd_fails_with_eperm`, `the_passed_state_memfd_carries_every_mount_seal`, `the_passed_state_memfd_is_not_executable_and_sealed_against_becoming_so` (5.1: `MFD_NOEXEC_SEAL`, K25); the mount's counter memfd, `tierstore-mount-client`: `the_memfd_is_sealed_against_shrinking_and_exec_only` |

The io_uring halves of items 3 and 4 are in `kernel_uring_integration` (T-F5), where
`fuse.enable_uring` is on: a READ answered by a commit, a parked READ completed from another
thread, and a killed caller's INTERRUPT arriving on the `/dev/fuse` reader and answered by a
commit of the READ's entry. The io_uring half of item 7 (a ring request is not resent;
`ECANCELED` for a request caught mid-dispatch, K14) is still to run: `Session::resume` refuses a
connection granted io_uring.

## What the kernel does that the suite asserts

These facts are what the farm kernel (Linux 7.0.0-42) does when served through the library. The
tests assert them as found.

- **A READDIR resumes after the last entry the caller kept**, not after the last one the server
  sent. The kernel keeps only the entries that fit the caller's `getdents` buffer, and its next
  READDIR names the offset of the last one it kept. Every entry is still listed once. A server
  therefore pages a directory by the offset the kernel sends and never by what it sent before.
- **An abort through `/sys/fs/fuse/connections/<n>/abort` ends the readers' reads with `ENODEV`**,
  the same end as an unmount (`ReaderEnd::Unmounted`). The kernel reads `ECONNABORTED` only on a
  connection whose INIT asked for `FUSE_ABORT_ERROR`, which `tsfuse` does not ask for [run;
  source: `fs/fuse/control.c` sets `fc->aborted` only with `fc->abort_err`, and
  `fuse_dev_do_read` returns `ECONNABORTED` only when it is set]. Every request waiting on the
  connection still fails `ECONNABORTED` for its caller. A reader whose read races the end of the
  connection can get `ECONNABORTED` instead (`ReaderEnd::Aborted`). On the farm that happened on
  some runs when a lazily detached mount ended at its last close. The abort and lazy-unmount
  tests therefore accept either end for each reader. They assert that the kernel ended the
  connection, and that no reader ended as stopped.
- **A name longer than `NAME_MAX` (255 bytes) reaches the server.** The kernel does not hold FUSE
  names to it, so a LOOKUP of 256 bytes is sent and answered `ENOENT` by the tree.

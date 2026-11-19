/* SPDX-License-Identifier: Apache-2.0 */
/*
 * The layout probe of tsfuse's ABI (docs/design/fetch-cache-serve.md, 5.2, "ABI module"): it
 * prints the protocol version, then `sizeof` and `_Alignof` of every struct of the pinned uapi
 * header `fuse.h` (next to this file) that `tsfuse::abi` mirrors, and `offsetof` and `sizeof`
 * of each of its fields, as the C compiler of a farm node lays them out. Its output, verbatim,
 * is `layout.txt`, which `tests/abi_layout.rs` compares with the Rust structs.
 *
 * `fuse.h` is the uapi header of the farm's kernel (Linux 7.0), used under its BSD-2-Clause
 * licence. The probe is run on a farm node, from the repository root, inside the
 * `docker.io/library/rust:1.95.0-trixie` container (the nodes have no C compiler of their own;
 * the container's is gcc 14.2):
 *
 *   cc -std=c11 -Wall -Wextra -Werror -o target/tsfuse-abi-probe crates/tsfuse/abi/probe.c
 *   target/tsfuse-abi-probe crates/tsfuse/abi/fuse.h > crates/tsfuse/abi/layout.txt
 *
 * Output lines:
 *   header-fnv1a64 <16 hex digits>   (FNV-1a of the bytes of the header file named by argv[1],
 *                                     which must be the `fuse.h` this probe was compiled with)
 *   version <major> <minor>
 *   struct <name> <sizeof> <alignof>
 *   field <struct> <field> <offsetof> <sizeof>   (sizeof 0: a flexible array member)
 */

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>

#include "fuse.h"

#define S(T) printf("struct %s %zu %zu\n", #T, sizeof(struct T), _Alignof(struct T));
#define F(T, f) \
	printf("field %s %s %zu %zu\n", #T, #f, offsetof(struct T, f), sizeof(((struct T *)0)->f));
#define FLEX(T, f) printf("field %s %s %zu 0\n", #T, #f, offsetof(struct T, f));

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: %s <path of fuse.h>\n", argv[0]);
		return 2;
	}
	FILE *header = fopen(argv[1], "rb");
	if (!header) {
		perror(argv[1]);
		return 1;
	}
	uint64_t hash = 0xcbf29ce484222325ULL;
	int c;
	while ((c = fgetc(header)) != EOF)
		hash = (hash ^ (uint64_t)(unsigned char)c) * 0x100000001b3ULL;
	fclose(header);
	printf("header-fnv1a64 %016llx\n", (unsigned long long)hash);
	printf("version %d %d\n", FUSE_KERNEL_VERSION, FUSE_KERNEL_MINOR_VERSION);

	S(fuse_in_header)
	F(fuse_in_header, len) F(fuse_in_header, opcode) F(fuse_in_header, unique)
	F(fuse_in_header, nodeid) F(fuse_in_header, uid) F(fuse_in_header, gid)
	F(fuse_in_header, pid) F(fuse_in_header, total_extlen) F(fuse_in_header, padding)

	S(fuse_out_header)
	F(fuse_out_header, len) F(fuse_out_header, error) F(fuse_out_header, unique)

	S(fuse_attr)
	F(fuse_attr, ino) F(fuse_attr, size) F(fuse_attr, blocks) F(fuse_attr, atime)
	F(fuse_attr, mtime) F(fuse_attr, ctime) F(fuse_attr, atimensec) F(fuse_attr, mtimensec)
	F(fuse_attr, ctimensec) F(fuse_attr, mode) F(fuse_attr, nlink) F(fuse_attr, uid)
	F(fuse_attr, gid) F(fuse_attr, rdev) F(fuse_attr, blksize) F(fuse_attr, flags)

	S(fuse_kstatfs)
	F(fuse_kstatfs, blocks) F(fuse_kstatfs, bfree) F(fuse_kstatfs, bavail)
	F(fuse_kstatfs, files) F(fuse_kstatfs, ffree) F(fuse_kstatfs, bsize)
	F(fuse_kstatfs, namelen) F(fuse_kstatfs, frsize) F(fuse_kstatfs, padding)
	F(fuse_kstatfs, spare)

	S(fuse_entry_out)
	F(fuse_entry_out, nodeid) F(fuse_entry_out, generation) F(fuse_entry_out, entry_valid)
	F(fuse_entry_out, attr_valid) F(fuse_entry_out, entry_valid_nsec)
	F(fuse_entry_out, attr_valid_nsec) F(fuse_entry_out, attr)

	S(fuse_forget_in)
	F(fuse_forget_in, nlookup)

	S(fuse_forget_one)
	F(fuse_forget_one, nodeid) F(fuse_forget_one, nlookup)

	S(fuse_batch_forget_in)
	F(fuse_batch_forget_in, count) F(fuse_batch_forget_in, dummy)

	S(fuse_getattr_in)
	F(fuse_getattr_in, getattr_flags) F(fuse_getattr_in, dummy) F(fuse_getattr_in, fh)

	S(fuse_attr_out)
	F(fuse_attr_out, attr_valid) F(fuse_attr_out, attr_valid_nsec) F(fuse_attr_out, dummy)
	F(fuse_attr_out, attr)

	S(fuse_mkdir_in)
	F(fuse_mkdir_in, mode) F(fuse_mkdir_in, umask)

	S(fuse_rename_in)
	F(fuse_rename_in, newdir)

	S(fuse_rename2_in)
	F(fuse_rename2_in, newdir) F(fuse_rename2_in, flags) F(fuse_rename2_in, padding)

	S(fuse_mknod_in)
	F(fuse_mknod_in, mode) F(fuse_mknod_in, rdev) F(fuse_mknod_in, umask)
	F(fuse_mknod_in, padding)

	S(fuse_link_in)
	F(fuse_link_in, oldnodeid)

	S(fuse_setattr_in)
	F(fuse_setattr_in, valid) F(fuse_setattr_in, padding) F(fuse_setattr_in, fh)
	F(fuse_setattr_in, size) F(fuse_setattr_in, lock_owner) F(fuse_setattr_in, atime)
	F(fuse_setattr_in, mtime) F(fuse_setattr_in, ctime) F(fuse_setattr_in, atimensec)
	F(fuse_setattr_in, mtimensec) F(fuse_setattr_in, ctimensec) F(fuse_setattr_in, mode)
	F(fuse_setattr_in, unused4) F(fuse_setattr_in, uid) F(fuse_setattr_in, gid)
	F(fuse_setattr_in, unused5)

	S(fuse_open_in)
	F(fuse_open_in, flags) F(fuse_open_in, open_flags)

	S(fuse_create_in)
	F(fuse_create_in, flags) F(fuse_create_in, mode) F(fuse_create_in, umask)
	F(fuse_create_in, open_flags)

	S(fuse_open_out)
	F(fuse_open_out, fh) F(fuse_open_out, open_flags) F(fuse_open_out, backing_id)

	S(fuse_release_in)
	F(fuse_release_in, fh) F(fuse_release_in, flags) F(fuse_release_in, release_flags)
	F(fuse_release_in, lock_owner)

	S(fuse_flush_in)
	F(fuse_flush_in, fh) F(fuse_flush_in, unused) F(fuse_flush_in, padding)
	F(fuse_flush_in, lock_owner)

	S(fuse_read_in)
	F(fuse_read_in, fh) F(fuse_read_in, offset) F(fuse_read_in, size)
	F(fuse_read_in, read_flags) F(fuse_read_in, lock_owner) F(fuse_read_in, flags)
	F(fuse_read_in, padding)

	S(fuse_write_in)
	F(fuse_write_in, fh) F(fuse_write_in, offset) F(fuse_write_in, size)
	F(fuse_write_in, write_flags) F(fuse_write_in, lock_owner) F(fuse_write_in, flags)
	F(fuse_write_in, padding)

	S(fuse_write_out)
	F(fuse_write_out, size) F(fuse_write_out, padding)

	S(fuse_statfs_out)
	F(fuse_statfs_out, st)

	S(fuse_fsync_in)
	F(fuse_fsync_in, fh) F(fuse_fsync_in, fsync_flags) F(fuse_fsync_in, padding)

	S(fuse_init_in)
	F(fuse_init_in, major) F(fuse_init_in, minor) F(fuse_init_in, max_readahead)
	F(fuse_init_in, flags) F(fuse_init_in, flags2) F(fuse_init_in, unused)

	S(fuse_init_out)
	F(fuse_init_out, major) F(fuse_init_out, minor) F(fuse_init_out, max_readahead)
	F(fuse_init_out, flags) F(fuse_init_out, max_background)
	F(fuse_init_out, congestion_threshold) F(fuse_init_out, max_write)
	F(fuse_init_out, time_gran) F(fuse_init_out, max_pages) F(fuse_init_out, map_alignment)
	F(fuse_init_out, flags2) F(fuse_init_out, max_stack_depth)
	F(fuse_init_out, request_timeout) F(fuse_init_out, unused)

	S(fuse_interrupt_in)
	F(fuse_interrupt_in, unique)

	S(fuse_dirent)
	F(fuse_dirent, ino) F(fuse_dirent, off) F(fuse_dirent, namelen) F(fuse_dirent, type)
	FLEX(fuse_dirent, name)

	S(fuse_direntplus)
	F(fuse_direntplus, entry_out) F(fuse_direntplus, dirent)

	return 0;
}

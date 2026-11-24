//! The reader's tests: the listing, the passes and their digest for any thread count, the node it reads from a `proc` directory, the
//! report, and its arguments. `proc` is a directory the tests make, never the system's.
const std = @import("std");
const t = std.testing;
const reader = @import("tree_read.zig");
const Sha256 = std.crypto.hash.sha2.Sha256;

fn write(dir: std.fs.Dir, path: []const u8, bytes: []const u8) !void {
    if (std.fs.path.dirname(path)) |d| try dir.makePath(d);
    const f = try dir.createFile(path, .{});
    defer f.close();
    try f.writeAll(bytes);
}

/// A tree of five files (one empty, one of more than a read chunk), an empty directory and a symbolic link.
fn tree(a: std.mem.Allocator, dir: std.fs.Dir) ![]u8 {
    try write(dir, "zig", "the compiler");
    try write(dir, "lib/std/std.zig", "pub const a = 1;\n");
    try write(dir, "lib/std/empty.zig", "");
    try write(dir, "lib/c/x.h", "#define X 1\n");
    const big = try a.alloc(u8, reader.chunk + 99);
    for (big, 0..) |*b, i| b.* = @truncate(i * 13 + 1);
    try write(dir, "lib/big", big);
    try dir.makePath("doc/none");
    try dir.symLink("zig", "zig-link", .{});
    return big;
}

fn pathOf(a: std.mem.Allocator, dir: std.fs.Dir) ![]u8 {
    return dir.realpathAlloc(a, ".");
}

/// The digest the reader must give the tree of `tree`, computed here from the files' bytes.
fn expectedDigest(big: []const u8) [32]u8 {
    const paths = [_][]const u8{ "lib/big", "lib/c/x.h", "lib/std/empty.zig", "lib/std/std.zig", "zig" };
    const bytes = [_][]const u8{ big, "#define X 1\n", "", "pub const a = 1;\n", "the compiler" };
    var sizes: [5]u64 = undefined;
    var digests: [5][32]u8 = undefined;
    for (bytes, 0..) |b, i| {
        sizes[i] = b.len;
        Sha256.hash(b, &digests[i], .{});
    }
    return reader.treeDigest(&paths, &sizes, &digests);
}

test "the listing has every file sorted, counts directories and links, and follows no link" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const a = arena.allocator();
    var tmp = t.tmpDir(.{});
    defer tmp.cleanup();
    _ = try tree(a, tmp.dir);
    const l = try reader.list(a, try pathOf(a, tmp.dir));
    try t.expectEqual(@as(usize, 5), l.files.len);
    const want = [_][]const u8{ "lib/big", "lib/c/x.h", "lib/std/empty.zig", "lib/std/std.zig", "zig" };
    for (want, l.files) |w, got| try t.expectEqualStrings(w, got);
    try t.expectEqual(@as(u64, 5), l.dirs);
    try t.expectEqual(@as(u64, 1), l.others);
    try t.expect(l.root_ns <= l.walk_ns);
}

test "a pass reads every file whole, and its digest is the same for any number of threads" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const a = arena.allocator();
    var tmp = t.tmpDir(.{});
    defer tmp.cleanup();
    const big = try tree(a, tmp.dir);
    const root = try pathOf(a, tmp.dir);
    const l = try reader.list(a, root);
    const want = std.fmt.bytesToHex(expectedDigest(big), .lower);
    for ([_]u32{ 1, 2, 5, 64 }) |threads| {
        const p = try reader.readPass(a, root, l.files, threads);
        try t.expectEqualStrings(&want, p.tree_sha256);
        try t.expectEqual(@as(u64, 5), p.files);
        try t.expectEqual(@as(u64, 1), p.empty);
        try t.expectEqual(@as(u64, big.len), p.largest);
        try t.expectEqual(@as(u64, 12 + 17 + 0 + 12 + big.len), p.bytes);
        // At most one thread per file.
        try t.expectEqual(@as(usize, @min(threads, 5)), p.threads);
        try t.expect(p.first_file_ns <= p.read_ns);
    }
}

test "one changed byte changes the tree's digest" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const a = arena.allocator();
    var tmp = t.tmpDir(.{});
    defer tmp.cleanup();
    _ = try tree(a, tmp.dir);
    const root = try pathOf(a, tmp.dir);
    const l = try reader.list(a, root);
    const before = try reader.readPass(a, root, l.files, 4);
    try write(tmp.dir, "lib/c/x.h", "#define X 2\n");
    const after = try reader.readPass(a, root, l.files, 4);
    try t.expectEqual(before.bytes, after.bytes);
    try t.expect(!std.mem.eql(u8, before.tree_sha256, after.tree_sha256));
}

test "a tree with no file: a pass of nothing, whose digest is that of no input" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const a = arena.allocator();
    var tmp = t.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.makePath("only/dirs");
    const root = try pathOf(a, tmp.dir);
    const l = try reader.list(a, root);
    try t.expectEqual(@as(usize, 0), l.files.len);
    const p = try reader.readPass(a, root, l.files, 16);
    var none: [32]u8 = undefined;
    Sha256.hash("", &none, .{});
    try t.expectEqualStrings(&std.fmt.bytesToHex(none, .lower), p.tree_sha256);
    try t.expectEqual(@as(u64, 0), p.first_file_ns);
    try t.expectEqual(@as(u64, 0), p.files);
    try t.expectEqual(@as(usize, 1), p.threads);
}

test "a file that cannot be opened fails the pass" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const a = arena.allocator();
    var tmp = t.tmpDir(.{});
    defer tmp.cleanup();
    try write(tmp.dir, "here", "x");
    const root = try pathOf(a, tmp.dir);
    try t.expectError(error.FileNotFound, reader.readPass(a, root, &.{ "here", "gone" }, 2));
    try t.expectError(error.FileNotFound, reader.readPass(a, root, &.{ "gone", "here" }, 1));
}

test "the mount of a path is the line whose mount point is the longest above it" {
    const info =
        \\22 1 0:21 / / rw - overlay overlay rw,lowerdir=/l
        \\30 22 0:40 / /in rw - fuse.tierstore tierstore rw
        \\31 22 0:41 / /input rw - tmpfs tmpfs rw
        \\32 30 0:42 / /in/tree rw - fuse.tierstore tierstore rw,deep
        \\short line
        \\
    ;
    try t.expectEqualStrings("32 30 0:42 / /in/tree rw - fuse.tierstore tierstore rw,deep", reader.mountFor(info, "/in/tree/lib").?);
    try t.expectEqualStrings("32 30 0:42 / /in/tree rw - fuse.tierstore tierstore rw,deep", reader.mountFor(info, "/in/tree").?);
    // `/input` is not below `/in`: the match is by whole path components.
    try t.expectEqualStrings("31 22 0:41 / /input rw - tmpfs tmpfs rw", reader.mountFor(info, "/input/x").?);
    try t.expectEqualStrings("30 22 0:40 / /in rw - fuse.tierstore tierstore rw", reader.mountFor(info, "/in/other").?);
    try t.expectEqualStrings("22 1 0:21 / / rw - overlay overlay rw,lowerdir=/l", reader.mountFor(info, "/elsewhere").?);
    try t.expectEqual(@as(?[]const u8, null), reader.mountFor("30 22 0:40 / /in rw - fuse f rw\n", "/out"));
    try t.expectEqual(@as(?[]const u8, null), reader.mountFor("", "/"));
}

test "the node comes from the proc directory given, and is null where it has nothing" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const a = arena.allocator();
    var proc = t.tmpDir(.{});
    defer proc.cleanup();
    var data = t.tmpDir(.{});
    defer data.cleanup();
    const tree_path = try pathOf(a, data.dir);
    try write(proc.dir, "sys/kernel/random/boot_id", "6f1c2a9e-0000-4000-8000-000000000001\n");
    try write(proc.dir, "sys/kernel/hostname", "node-x\n");
    try write(proc.dir, "loadavg", "12.50 11.00 10.25 9/2000 4242\n");
    const info = try std.fmt.allocPrint(a, "1 0 0:1 / / rw - ext4 /dev/root rw\n9 1 0:9 / {s} ro - fuse.tierstore tierstore ro\n", .{tree_path});
    try write(proc.dir, "self/mountinfo", info);
    const n = reader.node(a, try pathOf(a, proc.dir), tree_path);
    try t.expectEqualStrings("6f1c2a9e-0000-4000-8000-000000000001", n.boot_id.?);
    try t.expectEqualStrings("node-x", n.hostname.?);
    try t.expectEqualStrings("12.50 11.00 10.25 9/2000 4242", n.loadavg.?);
    const want_mount = try std.fmt.allocPrint(a, "9 1 0:9 / {s} ro - fuse.tierstore tierstore ro", .{tree_path});
    try t.expectEqualStrings(want_mount, n.mount.?);

    // A proc directory without the files, and one that does not exist; a tree path that does not resolve is matched as given.
    var empty = t.tmpDir(.{});
    defer empty.cleanup();
    const bare = reader.node(a, try pathOf(a, empty.dir), "no/such/tree");
    try t.expectEqualDeep(reader.Node{ .boot_id = null, .hostname = null, .loadavg = null, .mount = null }, bare);
    const missing = try std.fs.path.join(a, &.{ try pathOf(a, empty.dir), "missing" });
    try t.expectEqualDeep(reader.Node{ .boot_id = null, .hostname = null, .loadavg = null, .mount = null }, reader.node(a, missing, tree_path));
}

test "the arguments: a tree, an output, and optional counts and proc" {
    try t.expectEqualDeep(reader.Args{ .tree = "t", .out = "o" }, try reader.parseArgs(&.{ "--out", "o", "t" }));
    try t.expectEqualDeep(
        reader.Args{ .tree = "t", .out = "o", .threads = 4, .passes = 3, .proc = "p" },
        try reader.parseArgs(&.{ "t", "--threads", "4", "--passes", "3", "--proc", "p", "--out", "o" }),
    );
    try t.expectError(error.MissingArgument, reader.parseArgs(&.{"t"}));
    try t.expectError(error.MissingArgument, reader.parseArgs(&.{ "--out", "o" }));
    try t.expectError(error.TooManyArguments, reader.parseArgs(&.{ "--out", "o", "t", "u" }));
    try t.expectError(error.MissingValue, reader.parseArgs(&.{ "t", "--out" }));
    try t.expectError(error.UnknownFlag, reader.parseArgs(&.{ "t", "--out", "o", "--nonce", "1" }));
    try t.expectError(error.BadNumber, reader.parseArgs(&.{ "t", "--out", "o", "--threads", "0" }));
    try t.expectError(error.BadNumber, reader.parseArgs(&.{ "t", "--out", "o", "--passes", "two" }));
    try t.expectError(error.BadNumber, reader.parseArgs(&.{ "t", "--out", "o", "--threads", "-1" }));
}

test "the program writes one JSON report to its output and to the report stream" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const a = arena.allocator();
    var data = t.tmpDir(.{});
    defer data.cleanup();
    var work = t.tmpDir(.{});
    defer work.cleanup();
    var proc = t.tmpDir(.{});
    defer proc.cleanup();
    const big = try tree(a, data.dir);
    try write(proc.dir, "sys/kernel/random/boot_id", "b00710\n");
    const out = try std.fs.path.join(a, &.{ try pathOf(a, work.dir), "read.json" });
    var report = std.ArrayList(u8).init(t.allocator);
    defer report.deinit();
    try reader.run(t.allocator, &.{ "--out", out, "--threads", "3", "--proc", try pathOf(a, proc.dir), try pathOf(a, data.dir) }, report.writer());

    const written = try work.dir.readFileAlloc(a, "read.json", 1 << 20);
    try t.expectEqualStrings(report.items, written);
    try t.expect(std.mem.endsWith(u8, written, "}\n"));
    const parsed = try std.json.parseFromSlice(std.json.Value, a, written, .{});
    const o = parsed.value.object;
    try t.expectEqual(@as(i64, 1), o.get("version").?.integer);
    try t.expectEqual(@as(i64, 3), o.get("threads").?.integer);
    try t.expectEqual(@as(i64, 5), o.get("dirs").?.integer);
    try t.expectEqual(@as(i64, 1), o.get("others").?.integer);
    try t.expect(o.get("started_ms").?.integer > 0);
    const passes = o.get("passes").?.array.items;
    try t.expectEqual(@as(usize, 2), passes.len);
    const want = std.fmt.bytesToHex(expectedDigest(big), .lower);
    for (passes) |p| {
        try t.expectEqualStrings(&want, p.object.get("tree_sha256").?.string);
        try t.expectEqual(@as(i64, 5), p.object.get("files").?.integer);
    }
    const n = o.get("node").?.object;
    try t.expectEqualStrings("b00710", n.get("boot_id").?.string);
    try t.expect(n.get("hostname").? == .null);
}

test "the program fails on a missing tree and on bad arguments, and writes nothing" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const a = arena.allocator();
    var work = t.tmpDir(.{ .iterate = true });
    defer work.cleanup();
    const root = try pathOf(a, work.dir);
    const out = try std.fs.path.join(a, &.{ root, "read.json" });
    const missing = try std.fs.path.join(a, &.{ root, "missing" });
    var report = std.ArrayList(u8).init(t.allocator);
    defer report.deinit();
    try t.expectError(error.FileNotFound, reader.run(t.allocator, &.{ "--out", out, missing }, report.writer()));
    try t.expectError(error.MissingArgument, reader.run(t.allocator, &.{missing}, report.writer()));
    try t.expectEqual(@as(usize, 0), report.items.len);
    var it = work.dir.iterate();
    try t.expect((try it.next()) == null);
}

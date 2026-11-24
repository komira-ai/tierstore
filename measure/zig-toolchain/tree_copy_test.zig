//! The writer's tests: what a copy holds with and without a salt, the entries it refuses, and its arguments.
const std = @import("std");
const t = std.testing;
const copy = @import("tree_copy.zig");
const Sha256 = std.crypto.hash.sha2.Sha256;

fn write(dir: std.fs.Dir, path: []const u8, bytes: []const u8, mode: std.fs.File.Mode) !void {
    if (std.fs.path.dirname(path)) |d| try dir.makePath(d);
    const f = try dir.createFile(path, .{ .mode = mode });
    defer f.close();
    try f.writeAll(bytes);
}

fn read(dir: std.fs.Dir, path: []const u8) ![]u8 {
    return dir.readFileAlloc(t.allocator, path, 1 << 24);
}

fn modeOf(dir: std.fs.Dir, path: []const u8) !std.fs.File.Mode {
    const f = try dir.openFile(path, .{});
    defer f.close();
    return (try f.stat()).mode & 0o777;
}

/// A source tree with a nested directory, an executable, an empty file, a file shorter than the salt, two files of equal bytes, a file
/// of more than one read chunk, an empty directory and a symbolic link.
fn source(dir: std.fs.Dir) ![]u8 {
    try write(dir, "bin/tool", "#!/bin/sh\necho tool\n", 0o755);
    try write(dir, "lib/std/a.zig", "const a = 1;\n", 0o644);
    try write(dir, "lib/std/same1.txt", "the same bytes, twice over\n", 0o644);
    try write(dir, "lib/std/same2.txt", "the same bytes, twice over\n", 0o644);
    try write(dir, "lib/empty", "", 0o644);
    try write(dir, "short", "abc", 0o644);
    const big = try t.allocator.alloc(u8, copy.chunk + 12345);
    for (big, 0..) |*b, i| b.* = @truncate(i * 7 + 3);
    try write(dir, "lib/big.bin", big, 0o644);
    try dir.makePath("doc/empty-dir");
    try dir.symLink("bin/tool", "zig", .{});
    return big;
}

test "a copy without a salt holds every entry byte for byte, with its mode and link target" {
    var src = t.tmpDir(.{ .iterate = true });
    defer src.cleanup();
    var out = t.tmpDir(.{});
    defer out.cleanup();
    const big = try source(src.dir);
    defer t.allocator.free(big);

    const totals = try copy.copyTree(t.allocator, src.dir, out.dir, "");
    try t.expectEqual(copy.Totals{ .files = 7, .empty = 1, .dirs = 5, .links = 1, .bytes = 20 + 13 + 27 + 27 + 0 + 3 + big.len }, totals);

    for ([_][]const u8{ "bin/tool", "lib/std/a.zig", "lib/std/same1.txt", "lib/empty", "short", "lib/big.bin" }) |p| {
        const want = try read(src.dir, p);
        defer t.allocator.free(want);
        const got = try read(out.dir, p);
        defer t.allocator.free(got);
        try t.expectEqualSlices(u8, want, got);
    }
    try t.expectEqual(@as(std.fs.File.Mode, 0o755), try modeOf(out.dir, "bin/tool"));
    try t.expectEqual(@as(std.fs.File.Mode, 0o644), try modeOf(out.dir, "short"));
    var buf: [std.fs.MAX_PATH_BYTES]u8 = undefined;
    try t.expectEqualStrings("bin/tool", try out.dir.readLink("zig", &buf));
    var empty_dir = try out.dir.openDir("doc/empty-dir", .{ .iterate = true });
    defer empty_dir.close();
    var it = empty_dir.iterate();
    try t.expect((try it.next()) == null);
}

/// What a salted copy of `bytes` must be: the same bytes with the last `salt_len` (or all) replaced by `saltTail`.
fn salted(salt: []const u8, bytes: []const u8) ![]u8 {
    const want = try t.allocator.dupe(u8, bytes);
    if (bytes.len == 0) return want;
    var d: [32]u8 = undefined;
    Sha256.hash(bytes, &d, .{});
    const tail = copy.saltTail(salt, d);
    const k = @min(bytes.len, copy.salt_len);
    @memcpy(want[want.len - k ..], tail[0..k]);
    return want;
}

test "a salted copy keeps each file's size and its bytes before the last ones, and replaces those" {
    var src = t.tmpDir(.{ .iterate = true });
    defer src.cleanup();
    var out = t.tmpDir(.{});
    defer out.cleanup();
    const big = try source(src.dir);
    defer t.allocator.free(big);

    const totals = try copy.copyTree(t.allocator, src.dir, out.dir, "run-1");
    try t.expectEqual(@as(u64, 7), totals.files);
    for ([_][]const u8{ "bin/tool", "lib/std/a.zig", "lib/std/same1.txt", "lib/empty", "short", "lib/big.bin" }) |p| {
        const orig = try read(src.dir, p);
        defer t.allocator.free(orig);
        const want = try salted("run-1", orig);
        defer t.allocator.free(want);
        const got = try read(out.dir, p);
        defer t.allocator.free(got);
        try t.expectEqualSlices(u8, want, got);
        try t.expectEqual(orig.len, got.len);
        // A file shorter than the salt is replaced whole; every longer one changes in its last bytes only.
        if (orig.len > copy.salt_len) try t.expectEqualSlices(u8, orig[0 .. orig.len - copy.salt_len], got[0 .. got.len - copy.salt_len]);
        if (orig.len > 0) try t.expect(!std.mem.eql(u8, orig, got));
    }
    // Files of equal bytes stay equal, so the tree keeps its duplicates.
    const one = try read(out.dir, "lib/std/same1.txt");
    defer t.allocator.free(one);
    const two = try read(out.dir, "lib/std/same2.txt");
    defer t.allocator.free(two);
    try t.expectEqualSlices(u8, one, two);
    try t.expectEqual(@as(std.fs.File.Mode, 0o755), try modeOf(out.dir, "bin/tool"));
}

test "copies made with two salts differ in every non-empty file" {
    var src = t.tmpDir(.{ .iterate = true });
    defer src.cleanup();
    var a = t.tmpDir(.{});
    defer a.cleanup();
    var b = t.tmpDir(.{});
    defer b.cleanup();
    const big = try source(src.dir);
    defer t.allocator.free(big);
    _ = try copy.copyTree(t.allocator, src.dir, a.dir, "run-1");
    _ = try copy.copyTree(t.allocator, src.dir, b.dir, "run-2");
    for ([_][]const u8{ "bin/tool", "short", "lib/big.bin" }) |p| {
        const x = try read(a.dir, p);
        defer t.allocator.free(x);
        const y = try read(b.dir, p);
        defer t.allocator.free(y);
        try t.expect(!std.mem.eql(u8, x, y));
    }
}

test "the salt's bytes depend on both the salt and the content" {
    var d1: [32]u8 = undefined;
    var d2: [32]u8 = undefined;
    Sha256.hash("one", &d1, .{});
    Sha256.hash("two", &d2, .{});
    try t.expectEqualSlices(u8, &copy.saltTail("s", d1), &copy.saltTail("s", d1));
    try t.expect(!std.mem.eql(u8, &copy.saltTail("s", d1), &copy.saltTail("s", d2)));
    try t.expect(!std.mem.eql(u8, &copy.saltTail("s", d1), &copy.saltTail("t", d1)));
    // The tail is the start of SHA-256(salt, digest).
    var h = Sha256.init(.{});
    h.update("s");
    h.update(&d1);
    var full: [32]u8 = undefined;
    h.final(&full);
    try t.expectEqualSlices(u8, full[0..copy.salt_len], &copy.saltTail("s", d1));
}

test "an entry that is neither a file, a directory nor a link is refused" {
    var src = t.tmpDir(.{ .iterate = true });
    defer src.cleanup();
    var out = t.tmpDir(.{});
    defer out.cleanup();
    const linux = std.os.linux;
    try t.expectEqual(@as(usize, 0), linux.mknodat(src.dir.fd, "fifo", linux.S.IFIFO | 0o644, 0));
    try t.expectError(error.UnsupportedKind, copy.copyTree(t.allocator, src.dir, out.dir, ""));
}

test "a file already in the output is not written over" {
    var src = t.tmpDir(.{ .iterate = true });
    defer src.cleanup();
    var out = t.tmpDir(.{});
    defer out.cleanup();
    try write(src.dir, "f", "new", 0o644);
    try write(out.dir, "f", "old", 0o644);
    try t.expectError(error.PathAlreadyExists, copy.copyTree(t.allocator, src.dir, out.dir, ""));
    const kept = try read(out.dir, "f");
    defer t.allocator.free(kept);
    try t.expectEqualStrings("old", kept);
}

test "the arguments: two paths and an optional salt, in any order" {
    try t.expectEqualDeep(copy.Args{ .src = "s", .out = "o" }, try copy.parseArgs(&.{ "s", "o" }));
    try t.expectEqualDeep(copy.Args{ .src = "s", .out = "o", .salt = "x" }, try copy.parseArgs(&.{ "--salt", "x", "s", "o" }));
    try t.expectEqualDeep(copy.Args{ .src = "s", .out = "o", .salt = "x" }, try copy.parseArgs(&.{ "s", "o", "--salt", "x" }));
    try t.expectError(error.MissingArgument, copy.parseArgs(&.{}));
    try t.expectError(error.MissingArgument, copy.parseArgs(&.{"s"}));
    try t.expectError(error.TooManyArguments, copy.parseArgs(&.{ "s", "o", "p" }));
    try t.expectError(error.MissingValue, copy.parseArgs(&.{ "s", "o", "--salt" }));
    try t.expectError(error.UnknownFlag, copy.parseArgs(&.{ "--nonce", "s", "o" }));
}

test "the program copies into a new output directory and reports the totals as one JSON line" {
    var src = t.tmpDir(.{ .iterate = true });
    defer src.cleanup();
    var work = t.tmpDir(.{});
    defer work.cleanup();
    try write(src.dir, "a/b", "bytes", 0o644);
    try write(src.dir, "e", "", 0o644);
    const src_path = try src.dir.realpathAlloc(t.allocator, ".");
    defer t.allocator.free(src_path);
    const work_path = try work.dir.realpathAlloc(t.allocator, ".");
    defer t.allocator.free(work_path);
    const out_path = try std.fs.path.join(t.allocator, &.{ work_path, "out", "tree" });
    defer t.allocator.free(out_path);

    var report = std.ArrayList(u8).init(t.allocator);
    defer report.deinit();
    try copy.run(t.allocator, &.{ src_path, out_path, "--salt", "n" }, report.writer());
    try t.expectEqualStrings("{\"salted\":true,\"totals\":{\"files\":2,\"empty\":1,\"dirs\":1,\"links\":0,\"bytes\":5}}\n", report.items);
    const got = try work.dir.readFileAlloc(t.allocator, "out/tree/a/b", 64);
    defer t.allocator.free(got);
    try t.expectEqual(@as(usize, 5), got.len);
    try t.expect(!std.mem.eql(u8, "bytes", got));

    report.clearRetainingCapacity();
    const plain = try std.fs.path.join(t.allocator, &.{ work_path, "plain" });
    defer t.allocator.free(plain);
    try copy.run(t.allocator, &.{ src_path, plain }, report.writer());
    try t.expectEqualStrings("{\"salted\":false,\"totals\":{\"files\":2,\"empty\":1,\"dirs\":1,\"links\":0,\"bytes\":5}}\n", report.items);
}

test "the program fails on a missing source and on bad arguments" {
    var work = t.tmpDir(.{});
    defer work.cleanup();
    const work_path = try work.dir.realpathAlloc(t.allocator, ".");
    defer t.allocator.free(work_path);
    const missing = try std.fs.path.join(t.allocator, &.{ work_path, "missing" });
    defer t.allocator.free(missing);
    const out = try std.fs.path.join(t.allocator, &.{ work_path, "out" });
    defer t.allocator.free(out);
    var report = std.ArrayList(u8).init(t.allocator);
    defer report.deinit();
    try t.expectError(error.FileNotFound, copy.run(t.allocator, &.{ missing, out }, report.writer()));
    try t.expectError(error.MissingArgument, copy.run(t.allocator, &.{"only-one"}, report.writer()));
    try t.expectEqual(@as(usize, 0), report.items.len);
}

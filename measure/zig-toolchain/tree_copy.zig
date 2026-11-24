//! The writer of the measurement (README.md): copies a directory tree, the Zig 0.12.0 toolchain, into the output directory of a build
//! action, file by file, so that the action's output is that tree.
//!
//! With a salt, the last bytes of every non-empty file are replaced by bytes derived from the salt and the file's content digest
//! (`saltTail`): each file keeps its path, size and executable bit, two files of equal bytes stay equal, and no file has the bytes of
//! any earlier copy made with another salt. Without one the copy is byte for byte.
const std = @import("std");
const Sha256 = std.crypto.hash.sha2.Sha256;

/// How many bytes at the end of a file the salt replaces (all of a shorter file).
pub const salt_len = 16;

/// The size of the read buffer: each file is read once, in pieces of this size, hashed and written as it passes.
pub const chunk = 1 << 20;

/// What a copy made.
pub const Totals = struct {
    files: u64 = 0,
    empty: u64 = 0,
    dirs: u64 = 0,
    links: u64 = 0,
    bytes: u64 = 0,
};

/// The arguments: `tree_copy <src> <out> [--salt <salt>]`.
pub const Args = struct {
    src: []const u8,
    out: []const u8,
    salt: []const u8 = "",
};

pub const ArgsError = error{ MissingArgument, UnknownFlag, TooManyArguments, MissingValue };

pub fn parseArgs(args: []const []const u8) ArgsError!Args {
    var positional: [2][]const u8 = undefined;
    var n: usize = 0;
    var salt: []const u8 = "";
    var i: usize = 0;
    while (i < args.len) : (i += 1) {
        const arg = args[i];
        if (std.mem.eql(u8, arg, "--salt")) {
            i += 1;
            if (i == args.len) return error.MissingValue;
            salt = args[i];
        } else if (std.mem.startsWith(u8, arg, "--")) {
            return error.UnknownFlag;
        } else {
            if (n == positional.len) return error.TooManyArguments;
            positional[n] = arg;
            n += 1;
        }
    }
    if (n < positional.len) return error.MissingArgument;
    return .{ .src = positional[0], .out = positional[1], .salt = salt };
}

/// The bytes that end a salted file whose content (before salting) has the SHA-256 `digest`: the first `salt_len` bytes of
/// SHA-256(salt, digest). A file shorter than `salt_len` takes the first `size` of them.
pub fn saltTail(salt: []const u8, digest: [32]u8) [salt_len]u8 {
    var h = Sha256.init(.{});
    h.update(salt);
    h.update(&digest);
    var d: [32]u8 = undefined;
    h.final(&d);
    return d[0..salt_len].*;
}

/// Copies every entry below `src` to the same path below `out` (which must hold none of them): directories, regular files (with
/// their mode's permission bits) and symbolic links (their target, unchanged). Any other kind of entry is refused.
pub fn copyTree(a: std.mem.Allocator, src: std.fs.Dir, out: std.fs.Dir, salt: []const u8) !Totals {
    const buf = try a.alloc(u8, chunk);
    defer a.free(buf);
    var totals = Totals{};
    var walker = try src.walk(a);
    defer walker.deinit();
    // The walker gives a directory before what it holds, so each file's directory exists when the file is made.
    while (try walker.next()) |e| {
        switch (e.kind) {
            .directory => {
                try out.makePath(e.path);
                totals.dirs += 1;
            },
            .file => {
                const n = try copyFile(src, out, e.path, salt, buf);
                totals.files += 1;
                totals.bytes += n;
                if (n == 0) totals.empty += 1;
            },
            .sym_link => {
                var target: [std.fs.MAX_PATH_BYTES]u8 = undefined;
                try out.symLink(try src.readLink(e.path, &target), e.path, .{});
                totals.links += 1;
            },
            else => return error.UnsupportedKind,
        }
    }
    return totals;
}

/// Copies one file, read once: each piece is hashed and written as it is read; with a salt, its last bytes are then written over.
fn copyFile(src: std.fs.Dir, out: std.fs.Dir, path: []const u8, salt: []const u8, buf: []u8) !u64 {
    const in = try src.openFile(path, .{});
    defer in.close();
    const st = try in.stat();
    const dst = try out.createFile(path, .{ .exclusive = true, .mode = st.mode & 0o777 });
    defer dst.close();
    var h = Sha256.init(.{});
    var n: u64 = 0;
    while (true) {
        const k = try in.read(buf);
        if (k == 0) break;
        h.update(buf[0..k]);
        try dst.writeAll(buf[0..k]);
        n += k;
    }
    if (salt.len != 0 and n != 0) {
        var d: [32]u8 = undefined;
        h.final(&d);
        const tail = saltTail(salt, d);
        const k: usize = @intCast(@min(n, salt_len));
        try dst.pwriteAll(tail[0..k], n - k);
    }
    return n;
}

/// The program: copies `src` into `out` (made if absent) and writes the totals to `report` as one JSON line.
pub fn run(a: std.mem.Allocator, argv: []const []const u8, report: anytype) !void {
    const args = try parseArgs(argv);
    var src = try std.fs.cwd().openDir(args.src, .{ .iterate = true });
    defer src.close();
    try std.fs.cwd().makePath(args.out);
    var out = try std.fs.cwd().openDir(args.out, .{});
    defer out.close();
    const totals = try copyTree(a, src, out, args.salt);
    try std.json.stringify(.{ .salted = args.salt.len != 0, .totals = totals }, .{}, report);
    try report.writeByte('\n');
}

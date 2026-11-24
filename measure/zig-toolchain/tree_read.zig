//! The reader of the measurement (README.md): reads and hashes every file of a tree, the action input that the writer's output became,
//! and reports how long that took and where it ran.
//!
//! The tree is listed first (every directory, no file opened), then read in `passes` passes: in each, `threads` threads take the files
//! one at a time and read each whole, hashing it. The report holds, on the node's clocks: the wall time the program started, the time to
//! list the tree's root and the whole tree, and per pass the time until the first file was read and until the last was; the tree's
//! digest (over the sorted paths, sizes and file digests, so the same for any thread count); and the node it ran on, read from `proc`:
//! the boot id (one per running kernel, so per node), the host name, the load average, and the mount the tree is on.
const std = @import("std");
const Sha256 = std.crypto.hash.sha2.Sha256;

/// The size of each read.
pub const chunk = 1 << 20;

/// The arguments: `tree_read --out <file> [--threads N] [--passes N] [--proc <dir>] <tree>`.
pub const Args = struct {
    tree: []const u8,
    out: []const u8,
    threads: u32 = 16,
    passes: u32 = 2,
    proc: []const u8 = "/proc",
};

pub const ArgsError = error{ MissingArgument, UnknownFlag, TooManyArguments, MissingValue, BadNumber };

pub fn parseArgs(args: []const []const u8) ArgsError!Args {
    var tree: ?[]const u8 = null;
    var out: ?[]const u8 = null;
    var parsed = Args{ .tree = "", .out = "" };
    var i: usize = 0;
    while (i < args.len) : (i += 1) {
        const arg = args[i];
        const flags = [_][]const u8{ "--out", "--threads", "--passes", "--proc" };
        const which = for (flags, 0..) |f, k| {
            if (std.mem.eql(u8, arg, f)) break k;
        } else null;
        if (which) |k| {
            i += 1;
            if (i == args.len) return error.MissingValue;
            const v = args[i];
            switch (k) {
                0 => out = v,
                1 => parsed.threads = try positive(v),
                2 => parsed.passes = try positive(v),
                else => parsed.proc = v,
            }
        } else if (std.mem.startsWith(u8, arg, "--")) {
            return error.UnknownFlag;
        } else {
            if (tree != null) return error.TooManyArguments;
            tree = arg;
        }
    }
    parsed.tree = tree orelse return error.MissingArgument;
    parsed.out = out orelse return error.MissingArgument;
    return parsed;
}

fn positive(text: []const u8) ArgsError!u32 {
    const v = std.fmt.parseInt(u32, text, 10) catch return error.BadNumber;
    if (v == 0) return error.BadNumber;
    return v;
}

/// The entries of a tree, listed without opening a file.
pub const Listing = struct {
    /// The files' paths below the tree, sorted.
    files: []const []const u8,
    dirs: u64,
    /// The symbolic links (counted, not followed).
    others: u64,
    /// Nanoseconds to open the tree and list its root.
    root_ns: u64,
    /// Nanoseconds to list every directory, the root's included.
    walk_ns: u64,
};

/// Lists `tree` (allocations in `a`, which the caller frees as one: an arena). A symbolic link is counted in `others` and not followed;
/// an entry that is neither a file, a directory nor a link is refused.
pub fn list(a: std.mem.Allocator, tree: []const u8) !Listing {
    const t0 = try std.time.Instant.now();
    var dir = try std.fs.cwd().openDir(tree, .{ .iterate = true });
    defer dir.close();
    var it = dir.iterate();
    while (try it.next()) |_| {}
    const root_ns = (try std.time.Instant.now()).since(t0);
    var files = std.ArrayList([]const u8).init(a);
    var dirs: u64 = 0;
    var others: u64 = 0;
    var walker = try dir.walk(a);
    defer walker.deinit();
    while (try walker.next()) |e| {
        switch (e.kind) {
            .file => try files.append(try a.dupe(u8, e.path)),
            .directory => dirs += 1,
            .sym_link => others += 1,
            // Any other kind (a device, a pipe, an entry whose kind the file system does not give) would leave the read short: refused.
            else => return error.UnsupportedKind,
        }
    }
    const walk_ns = (try std.time.Instant.now()).since(t0);
    std.mem.sort([]const u8, files.items, {}, lessThan);
    return .{ .files = files.items, .dirs = dirs, .others = others, .root_ns = root_ns, .walk_ns = walk_ns };
}

fn lessThan(_: void, x: []const u8, y: []const u8) bool {
    return std.mem.lessThan(u8, x, y);
}

/// One pass over every file.
pub const Pass = struct {
    /// The threads that read.
    threads: usize,
    /// Nanoseconds from the pass's start until the first file was read whole.
    first_file_ns: u64,
    /// Nanoseconds from the pass's start until the last file was read whole.
    read_ns: u64,
    files: u64,
    bytes: u64,
    empty: u64,
    largest: u64,
    /// SHA-256, lowercase hex, of each file's path, size and digest, in path order (`treeDigest`).
    tree_sha256: []const u8,
};

const Shared = struct {
    dir: std.fs.Dir,
    paths: []const []const u8,
    sizes: []u64,
    digests: [][32]u8,
    next: std.atomic.Value(usize) = std.atomic.Value(usize).init(0),
    start: std.time.Instant,
    first_ns: std.atomic.Value(u64) = std.atomic.Value(u64).init(std.math.maxInt(u64)),
    lock: std.Thread.Mutex = .{},
    failure: ?anyerror = null,

    fn fail(s: *Shared, e: anyerror) void {
        s.lock.lock();
        defer s.lock.unlock();
        if (s.failure == null) s.failure = e;
        // The other readers take no further file.
        s.next.store(s.paths.len, .monotonic);
    }
};

fn worker(s: *Shared, buf: []u8) void {
    while (true) {
        const i = s.next.fetchAdd(1, .monotonic);
        if (i >= s.paths.len) return;
        readOne(s, i, buf) catch |e| {
            s.fail(e);
            return;
        };
    }
}

fn readOne(s: *Shared, i: usize, buf: []u8) !void {
    const f = try s.dir.openFile(s.paths[i], .{});
    defer f.close();
    var h = Sha256.init(.{});
    var n: u64 = 0;
    while (true) {
        const k = try f.read(buf);
        if (k == 0) break;
        h.update(buf[0..k]);
        n += k;
    }
    h.final(&s.digests[i]);
    s.sizes[i] = n;
    const at = (try std.time.Instant.now()).since(s.start);
    _ = s.first_ns.fetchMin(at, .monotonic);
}

/// Reads every file of `paths` (below `tree`) once, on `threads` threads (at most one per file, at least one). Its buffers (one
/// `chunk` per thread) and results are allocated in `a`, an arena the caller frees after all the passes.
pub fn readPass(a: std.mem.Allocator, tree: []const u8, paths: []const []const u8, threads: u32) !Pass {
    var dir = try std.fs.cwd().openDir(tree, .{});
    defer dir.close();
    var s = Shared{
        .dir = dir,
        .paths = paths,
        .sizes = try a.alloc(u64, paths.len),
        .digests = try a.alloc([32]u8, paths.len),
        .start = try std.time.Instant.now(),
    };
    const n: usize = @max(1, @min(threads, paths.len));
    const bufs = try a.alloc(u8, n * chunk);
    const spawned = try a.alloc(std.Thread, n - 1);
    var started: usize = 0;
    // The calling thread is one of the readers; the others are spawned. A spawn that fails stops the spawning, and the readers
    // already running do the work (`threads` of the pass says how many there were).
    while (started < spawned.len) : (started += 1) {
        spawned[started] = std.Thread.spawn(.{}, worker, .{ &s, bufs[(started + 1) * chunk ..][0..chunk] }) catch break;
    }
    worker(&s, bufs[0..chunk]);
    for (spawned[0..started]) |t| t.join();
    const read_ns = (try std.time.Instant.now()).since(s.start);
    if (s.failure) |e| return e;
    var p = Pass{
        .threads = started + 1,
        .first_file_ns = if (paths.len == 0) 0 else s.first_ns.load(.monotonic),
        .read_ns = read_ns,
        .files = paths.len,
        .bytes = 0,
        .empty = 0,
        .largest = 0,
        .tree_sha256 = undefined,
    };
    for (s.sizes) |size| {
        p.bytes += size;
        p.largest = @max(p.largest, size);
        if (size == 0) p.empty += 1;
    }
    const d = treeDigest(paths, s.sizes, s.digests);
    const hex = std.fmt.bytesToHex(d, .lower);
    p.tree_sha256 = try a.dupe(u8, &hex);
    return p;
}

/// SHA-256 over `path NUL size NUL hex-digest LF` of each file, in the order given.
pub fn treeDigest(paths: []const []const u8, sizes: []const u64, digests: []const [32]u8) [32]u8 {
    var h = Sha256.init(.{});
    for (paths, sizes, digests) |p, size, d| {
        var num: [20]u8 = undefined;
        h.update(p);
        h.update(&.{0});
        h.update(std.fmt.bufPrint(&num, "{d}", .{size}) catch unreachable);
        h.update(&.{0});
        const hex = std.fmt.bytesToHex(d, .lower);
        h.update(&hex);
        h.update("\n");
    }
    var out: [32]u8 = undefined;
    h.final(&out);
    return out;
}

/// The node, as `proc` shows it. A file that cannot be read is null (a system without one).
pub const Node = struct {
    boot_id: ?[]const u8,
    hostname: ?[]const u8,
    loadavg: ?[]const u8,
    /// The line of `self/mountinfo` of the mount the tree is on.
    mount: ?[]const u8,
};

pub fn node(a: std.mem.Allocator, proc: []const u8, tree: []const u8) Node {
    const real = std.fs.cwd().realpathAlloc(a, tree) catch tree;
    const mounts = readTrimmed(a, proc, "self/mountinfo");
    return .{
        .boot_id = readTrimmed(a, proc, "sys/kernel/random/boot_id"),
        .hostname = readTrimmed(a, proc, "sys/kernel/hostname"),
        .loadavg = readTrimmed(a, proc, "loadavg"),
        .mount = if (mounts) |m| mountFor(m, real) else null,
    };
}

fn readTrimmed(a: std.mem.Allocator, proc: []const u8, name: []const u8) ?[]const u8 {
    var dir = std.fs.cwd().openDir(proc, .{}) catch return null;
    defer dir.close();
    const text = dir.readFileAlloc(a, name, 1 << 24) catch return null;
    return std.mem.trim(u8, text, " \n");
}

/// The line of a mountinfo text whose mount point (its fifth field) is the longest that is `path` or a directory above it.
pub fn mountFor(mountinfo: []const u8, path: []const u8) ?[]const u8 {
    var best: ?[]const u8 = null;
    var best_len: usize = 0;
    var lines = std.mem.splitScalar(u8, mountinfo, '\n');
    while (lines.next()) |line| {
        var fields = std.mem.tokenizeScalar(u8, line, ' ');
        var k: usize = 0;
        const point = while (fields.next()) |f| : (k += 1) {
            if (k == 4) break f;
        } else continue;
        if (!under(path, point)) continue;
        // `>=`: of two mounts on one point, the later line is the one on top.
        if (best == null or point.len >= best_len) {
            best = line;
            best_len = point.len;
        }
    }
    return best;
}

/// Whether `path` is `dir` or below it.
fn under(path: []const u8, dir: []const u8) bool {
    if (!std.mem.startsWith(u8, path, dir)) return false;
    return path.len == dir.len or std.mem.eql(u8, dir, "/") or path[dir.len] == '/';
}

/// The program: lists and reads the tree, and writes the report as one JSON line to `--out` and to `report`.
pub fn run(parent: std.mem.Allocator, argv: []const []const u8, report: anytype) !void {
    var arena = std.heap.ArenaAllocator.init(parent);
    defer arena.deinit();
    const a = arena.allocator();
    const args = try parseArgs(argv);
    const started_ms = std.time.milliTimestamp();
    const l = try list(a, args.tree);
    const passes = try a.alloc(Pass, args.passes);
    for (passes) |*p| p.* = try readPass(a, args.tree, l.files, args.threads);
    const value = .{
        .version = 1,
        .started_ms = started_ms,
        .threads = args.threads,
        .dirs = l.dirs,
        .others = l.others,
        .root_ns = l.root_ns,
        .walk_ns = l.walk_ns,
        .passes = passes,
        .node = node(a, args.proc, args.tree),
    };
    var text = std.ArrayList(u8).init(a);
    try std.json.stringify(value, .{}, text.writer());
    try text.append('\n');
    const out = try std.fs.cwd().createFile(args.out, .{});
    defer out.close();
    try out.writeAll(text.items);
    try report.writeAll(text.items);
}

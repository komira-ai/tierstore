//! The reader of the measurement (README.md): reads and hashes every file of a tree, the action input that the writer's output became,
//! and reports how long that took and where it ran.
//!
//! The tree is listed first (every directory, no file opened), then read in `passes` passes: in each, `threads` threads take the files
//! one at a time and read each whole, hashing it. The report holds, on the node's clocks: the wall time the program started, the time to
//! list the tree's root and the whole tree, and per pass the time until the first file was read and until the last was; the tree's
//! digest (over the sorted paths, sizes and file digests, so the same for any thread count); and the node it ran on, read from `proc`:
//! the boot id (one per running kernel, so per node), the host name, the load average, and the mount the tree is on.
//!
//! So that a slow pass says where its time went, each pass also holds the time of each file split in two: its open (from the call of
//! `open` to its return: what a file system that fetches a file's bytes at its first open spends there) and its reads (the calls of
//! `read`, without the hashing), summed over the files, with the percentiles of the opens and of the whole files and the time of the
//! largest file; and the node's pressure stall information (`pressure/cpu`, `pressure/io`, `pressure/memory` of `proc`) as read just
//! before the pass and just after it, whose `total=` counters, in microseconds, say how long the node's tasks stalled on the CPU, on
//! I/O or on memory during the pass. The report also holds the number of CPUs the program may run on.
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
    /// Nanoseconds, summed over the files, from the call of `open` to its return.
    open_sum_ns: u64,
    /// Nanoseconds, summed over the files, in the calls of `read` (the hashing is not in it).
    read_sum_ns: u64,
    /// Nanoseconds, summed over the files, from the call of `open` to the last byte hashed.
    file_sum_ns: u64,
    /// The percentiles (by the nearest rank) and the most of one file's open.
    open_p50_ns: u64,
    open_p90_ns: u64,
    open_p99_ns: u64,
    open_max_ns: u64,
    /// The percentiles (by the nearest rank) and the most of one file, from the call of `open` to its last byte hashed.
    file_p50_ns: u64,
    file_p99_ns: u64,
    file_max_ns: u64,
    /// The largest file (the first in path order of that size), from the call of `open` to its last byte hashed; 0 for no file.
    largest_ns: u64,
    /// The node's pressure stall information, read just before the pass started and just after it ended.
    pressure_start: Pressure,
    pressure_end: Pressure,
};

/// The pressure stall information of the node, as `proc` shows it (each file's text, its lines `some ...` and `full ...`); a file
/// that cannot be read is null (a kernel without it).
pub const Pressure = struct {
    cpu: ?[]const u8,
    io: ?[]const u8,
    memory: ?[]const u8,
};

pub fn pressure(a: std.mem.Allocator, proc: []const u8) Pressure {
    return .{
        .cpu = readTrimmed(a, proc, "pressure/cpu"),
        .io = readTrimmed(a, proc, "pressure/io"),
        .memory = readTrimmed(a, proc, "pressure/memory"),
    };
}

/// The value at `percent` of `sorted` (ascending) by the nearest rank: the least value that at least `percent` of the values are at
/// or below. 0 for no value.
pub fn rank(sorted: []const u64, percent: u64) u64 {
    if (sorted.len == 0) return 0;
    const k = (sorted.len * percent + 99) / 100;
    return sorted[@min(sorted.len, @max(k, 1)) - 1];
}

const Shared = struct {
    dir: std.fs.Dir,
    paths: []const []const u8,
    sizes: []u64,
    digests: [][32]u8,
    /// Per file, nanoseconds: its open, its reads, and the whole file (see `Pass`).
    open_ns: []u64,
    read_ns: []u64,
    file_ns: []u64,
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
    const asked = try std.time.Instant.now();
    const f = try s.dir.openFile(s.paths[i], .{});
    defer f.close();
    const opened = try std.time.Instant.now();
    var h = Sha256.init(.{});
    var n: u64 = 0;
    var reading: u64 = 0;
    while (true) {
        const before = try std.time.Instant.now();
        const k = try f.read(buf);
        reading += (try std.time.Instant.now()).since(before);
        if (k == 0) break;
        h.update(buf[0..k]);
        n += k;
    }
    h.final(&s.digests[i]);
    const done = try std.time.Instant.now();
    s.sizes[i] = n;
    s.open_ns[i] = opened.since(asked);
    s.read_ns[i] = reading;
    s.file_ns[i] = done.since(asked);
    _ = s.first_ns.fetchMin(done.since(s.start), .monotonic);
}

/// Reads every file of `paths` (below `tree`) once, on `threads` threads (at most one per file, at least one), and reads the node's
/// pressure from `proc` just before and just after. Its buffers (one `chunk` per thread) and results are allocated in `a`, an arena
/// the caller frees after all the passes.
pub fn readPass(a: std.mem.Allocator, tree: []const u8, paths: []const []const u8, threads: u32, proc: []const u8) !Pass {
    var dir = try std.fs.cwd().openDir(tree, .{});
    defer dir.close();
    const sizes = try a.alloc(u64, paths.len);
    const digests = try a.alloc([32]u8, paths.len);
    const open_ns = try a.alloc(u64, paths.len);
    const read_ns_each = try a.alloc(u64, paths.len);
    const file_ns = try a.alloc(u64, paths.len);
    const pressure_start = pressure(a, proc);
    var s = Shared{
        .dir = dir,
        .paths = paths,
        .sizes = sizes,
        .digests = digests,
        .open_ns = open_ns,
        .read_ns = read_ns_each,
        .file_ns = file_ns,
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
    const pressure_end = pressure(a, proc);
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
        .open_sum_ns = 0,
        .read_sum_ns = 0,
        .file_sum_ns = 0,
        .open_p50_ns = 0,
        .open_p90_ns = 0,
        .open_p99_ns = 0,
        .open_max_ns = 0,
        .file_p50_ns = 0,
        .file_p99_ns = 0,
        .file_max_ns = 0,
        .largest_ns = 0,
        .pressure_start = pressure_start,
        .pressure_end = pressure_end,
    };
    for (s.sizes, open_ns, read_ns_each, file_ns) |size, o, r, f| {
        p.bytes += size;
        if (size > p.largest) {
            p.largest = size;
            p.largest_ns = f;
        }
        if (size == 0) p.empty += 1;
        p.open_sum_ns += o;
        p.read_sum_ns += r;
        p.file_sum_ns += f;
    }
    // The per-file times are sorted in place for their percentiles, after the sums and the largest file have read them by file.
    std.mem.sort(u64, open_ns, {}, std.sort.asc(u64));
    std.mem.sort(u64, file_ns, {}, std.sort.asc(u64));
    p.open_p50_ns = rank(open_ns, 50);
    p.open_p90_ns = rank(open_ns, 90);
    p.open_p99_ns = rank(open_ns, 99);
    p.open_max_ns = rank(open_ns, 100);
    p.file_p50_ns = rank(file_ns, 50);
    p.file_p99_ns = rank(file_ns, 99);
    p.file_max_ns = rank(file_ns, 100);
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
    for (passes) |*p| p.* = try readPass(a, args.tree, l.files, args.threads, args.proc);
    const value = .{
        .version = 1,
        .started_ms = started_ms,
        .threads = args.threads,
        // The CPUs the program may run on (its affinity): a job's share of the node.
        .cpus = std.Thread.getCpuCount() catch null,
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

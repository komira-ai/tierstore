//! `tree_read`: forwards its arguments to tree_read.zig, where all of the program is (and is tested).
const std = @import("std");
const tree_read = @import("tree_read.zig");

pub fn main() !void {
    const a = std.heap.page_allocator;
    try tree_read.run(a, (try std.process.argsAlloc(a))[1..], std.io.getStdOut().writer());
}

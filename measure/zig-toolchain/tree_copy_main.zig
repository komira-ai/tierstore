//! `tree_copy`: forwards its arguments to tree_copy.zig, where all of the program is (and is tested).
const std = @import("std");
const tree_copy = @import("tree_copy.zig");

pub fn main() !void {
    const a = std.heap.page_allocator;
    try tree_copy.run(a, (try std.process.argsAlloc(a))[1..], std.io.getStdErr().writer());
}

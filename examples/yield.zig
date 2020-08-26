const std = @import("std");
const zap = @import("zap");

pub fn main() !void {
    try (zap.Task.runAsync(asyncMain, .{}));
}

fn asyncMain() void {
    std.debug.warn("Hello world\n", .{});
}
const std = @import("std");
const builtin = @import("builtin");
const Task = @import("executor.zig").Task;
const zio = @import("../../zap.zig").zio;
const zuma = @import("../../zap.zig").zuma;

const expect = std.testing.expect;

pub const Reactor = struct {
    usingnamespace @import("./reactor/default.zig");
    usingnamespace @import("./reactor/uring.zig");

    pub const HandleType = enum {
        Socket,
    };

    pub const TypedHandle = union(HandleType) {
        Socket: usize,

        pub fn getValue(self: @This()) usize {
            return switch (self) {
                .Socket => |value| value,
            };
        }
    };

    inner: Inner,
    const Inner = union(enum) {
        Default: DefaultReactor,
        Uring: if (builtin.os == .linux) UringReactor else DefaultReactor,
    };

    pub const Error = zio.Event.Poller.Error;

    pub fn init(self: *@This(), allocator: *std.mem.Allocator) Error!void {
        if (builtin.os == .linux and UringReactor.isSupported()) {
            self.inner = Inner{ .Uring = undefined };
            return self.inner.Uring.init(allocator);
        } else {
            self.inner = Inner{ .Default = undefined };
            return self.inner.Default.init(allocator);
        }
    }

    pub fn deinit(self: *@This()) void {
        return switch (self.inner) {
            .Uring => |*uring| uring.deinit(),
            .Default => |*default| default.deinit(),
        };
    }

    pub const SocketError = zio.Socket.Error || zio.Event.Poller.RegisterError;

    pub fn socket(self: *@This(), flags: zio.Socket.Flags) SocketError!TypedHandle {
        return switch (self.inner) {
            .Uring => |*uring| uring.socket(flags),
            .Default => |*default| default.socket(flags),
        };
    }

    pub fn close(self: *@This(), typed_handle: TypedHandle) void {
        return switch (self.inner) {
            .Uring => |*uring| uring.close(typed_handle),
            .Default => |*default| default.close(typed_handle),
        };
    }

    pub fn getHandle(self: *@This(), typed_handle: TypedHandle) zio.Handle {
        return switch (self.inner) {
            .Uring => |*uring| uring.getHandle(typed_handle),
            .Default => |*default| default.getHandle(typed_handle),
        };
    }

    pub fn setsockopt(self: *@This(), typed_handle: TypedHandle, option: zio.Socket.Option) zio.Socket.OptionError!void {
        if (HandleType(typed_handle) != .Socket)
            return zio.Socket.OptionError.InvalidHandle;
        var sock = zio.Socket.fromHandle(self.getHandle(typed_handle), zio.Socket.Nonblock);
        return sock.setOption(option);
    }

    pub fn getsockopt(self: *@This(), typed_handle: TypedHandle, option: *zio.Socket.Option) zio.Socket.OptionError!void {
        if (HandleType(typed_handle) != .Socket)
            return zio.Socket.OptionError.InvalidHandle;
        var sock = zio.Socket.fromHandle(self.getHandle(typed_handle), zio.Socket.Nonblock);
        return sock.getOption(option);
    }

    pub fn bind(self: *@This(), typed_handle: TypedHandle, address: *const zio.Address) zio.Socket.BindError!void {
        if (HandleType(typed_handle) != .Socket)
            return zio.Socket.OptionError.InvalidHandle;
        var sock = zio.Socket.fromHandle(self.getHandle(typed_handle), zio.Socket.Nonblock);
        return sock.bind(address);
    }

    pub fn listen(self: *@This(), typed_handle: TypedHandle, backlog: c_uint) zio.Socket.ListenError!void {
        if (HandleType(typed_handle) != .Socket)
            return zio.Socket.OptionError.InvalidHandle;
        var sock = zio.Socket.fromHandle(self.getHandle(typed_handle), zio.Socket.Nonblock);
        return sock.listen(backlog);
    }

    pub const AcceptError = zio.Socket.RawAcceptError || error{Closed} || zio.Event.Poller.RegisterError;

    pub fn accept(self: *@This(), typed_handle: TypedHandle, flags: zio.Socket.Flags, address: *zio.Address) AcceptError!TypedHandle {
        return switch (self.inner) {
            .Uring => |*uring| uring.accept(typed_handle, flags, address),
            .Default => |*default| default.accept(typed_handle, flags, address),
        };
    }

    pub const ConnectError = zio.Socket.RawConnectError || error{Closed};

    pub fn connect(self: *@This(), typed_handle: TypedHandle, address: *const zio.Address) ConnectError!void {
        return switch (self.inner) {
            .Uring => |*uring| uring.connect(typed_handle, address),
            .Default => |*default| default.connect(typed_handle, address),
        };
    }

    pub const ReadError = zio.Socket.RawDataError || error{Closed};

    pub fn read(self: *@This(), typed_handle: TypedHandle, address: ?*zio.Address, buffer: []const []u8, offset: ?u64) ReadError!usize {
        return switch (self.inner) {
            .Uring => |*uring| uring.read(typed_handle, address, buffer, offset),
            .Default => |*default| default.read(typed_handle, address, buffer, offset),
        };
    }

    pub const WriteError = zio.Socket.RawDataError || error{Closed};

    pub fn write(self: *@This(), typed_handle: TypedHandle, address: ?*const zio.Address, buffer: []const []const u8, offset: ?u64) WriteError!usize {
        return switch (self.inner) {
            .Uring => |*uring| uring.write(typed_handle, address, buffer, offset),
            .Default => |*default| default.write(typed_handle, address, buffer, offset),
        };
    }

    pub const NotifyError = zio.Event.Poller.NotifyError;

    pub fn notify(self: *@This()) NotifyError!void {
        return switch (self.inner) {
            .Uring => |*uring| uring.notify(),
            .Default => |*default| default.notify(),
        };
    }

    pub const PollError = zio.Event.Poller.PollError;

    pub fn poll(self: *@This(), timeout_ms: ?u32) PollError!Task.List {
        return switch (self.inner) {
            .Uring => |*uring| uring.poll(timeout_ms),
            .Default => |*default| default.poll(timeout_ms),
        };
    }
};

test "Reactor - Socket" {
    var reactor: Reactor = undefined;
    try reactor.init(std.heap.direct_allocator);
    defer reactor.deinit();

    // create the server socket
    const sock_flags = zio.Socket.Ipv4 | zio.Socket.Tcp;
    const server_handle = try reactor.socket(sock_flags);
    defer reactor.close(server_handle);

    // set ReuseAddr on the server socket
    try reactor.setsockopt(server_handle, zio.Socket.Option{ .Reuseaddr = true });
    var option = zio.Socket.Option{ .Reuseaddr = false };
    try reactor.getsockopt(server_handle, &option);
    expect(option.Reuseaddr == true);

    // Bind the server to a local port
    const port = zuma.Thread.getRandom().intRangeLessThanBiased(u16, 1024, 65535);
    var address = zio.Address.fromIpv4(try zio.Address.parseIpv4("localhost"), port);
    try reactor.bind(server_handle, &address);
    try reactor.listen(server_handle, 1);

    // Create a client and try to connect to the server
    const client_handle = try reactor.socket(sock_flags);
    defer reactor.close(client_handle);
    try reactor.setsockopt(client_handle, zio.Socket.Option{ .RecvBufMax = 2048 });
    _ = try resolve(&reactor, Reactor.connect, &reactor, client_handle, &address);

    // Accept the incoming client from the server
    const server_client_handle = try resolve(&reactor, Reactor.accept, &reactor, server_handle, sock_flags, &address);
    defer reactor.close(server_client_handle);
    expect(address.isIpv4());
    try reactor.setsockopt(server_client_handle, zio.Socket.Option{ .SendBufMax = 2048 });

    const data = "a" ** (64 * 1024);
    const Stream = struct {
        pub fn push(r: *Reactor, handle: Reactor.TypedHandle) Reactor.WriteError!void {
            var transferred: usize = 0;
            while (transferred < data.len) {
                const buffer = [_][]const u8{data[transferred..]};
                const sent = try r.write(handle, null, buffer, null);
                expect(sent > 0);
                transferred += sent;
            }
        }

        pub fn poll(r: *Reactor, handle: Reactor.TypedHandle) Reactor.ReadError!void {
            var transferred: usize = 0;
            var buffer: [data.len]u8 = undefined;
            while (transferred < data.len) {
                const recieved = try r.read(handle, null, [_][]u8{buffer[0..]}, null);
                expect(recieved > 0);
                transferred += recieved;
            }
        }
    };

    var poll_result: ?Reactor.ReadError!void = null;
    var push_result: ?Reactor.WriteError!void = null;
    var poll_frame = async Task.withResult(Stream.poll, &poll_result, &reactor, client_handle);
    var push_frame = async Task.withResult(Stream.push, &push_result, &reactor, server_client_handle);

    while (poll_result == null or push_result == null) {
        const tasks_polled = try reactor.poll(500);
        expect(tasks_polled.size > 0);
        var task_iterator = tasks_polled.iter();
        while (task_iterator.next()) |task|
            resume task.frame;
    }

    try poll_result.?;
    try push_result.?;
}

fn resolve(reactor: *Reactor, comptime func: var, args: ...) !@typeInfo(@typeOf(func).ReturnType).ErrorUnion.payload {
    var result: ?@typeOf(func).ReturnType = null;
    var frame = async Task.withResult(func, &result, args);
    if (result != null)
        return (try result.?);

    const task_list = try reactor.poll(500);
    expect(task_list.size > 0);
    var tasks = task_list.iter();
    while (tasks.next()) |task|
        resume task.frame;

    expect(result != null);
    return (try result.?);
}

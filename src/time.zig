// Copyright (c) 2020 kprotty
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

const std = @import("std");
const zap = @import("./zap.zig");

pub const os = struct {
    pub usingnamespace core(zap.sync.os.Signal);

    pub const now = nanotime;
    pub const nanotime = @import("./time/clock.zig").nanotime;
};

pub const task = core(zap.sync.task.Signal);

pub fn core(comptime Signal: type) type {
    return struct {
        pub fn sleep(delay_ns: u64) void {
            var signal: Signal = undefined;
            signal.init();

            signal.timedWait(delay_ns) catch return signal.deinit();

            unreachable;
        }
    };
}



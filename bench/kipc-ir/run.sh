#!/bin/sh
# Instruction counts for the kernel IPC subsystems, under callgrind. Run inside WSL.
#
# The count is the verdict; a clock on this workload would be measuring the
# box. The checksum and the accepted count are printed so a compiler that
# removed the work, or a change that altered a verdict, shows up as a changed
# number rather than as a good one.
#
# THE FRESHNESS CHECK IS NOT OPTIONAL. Its sibling in rusty_rtos_heap was
# wrong once -- it profiled a stale binary left by an earlier run and reported
# an identical count across a real source change, which is exactly what a
# stale binary looks like.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"

bin=target/release/kipc-ir
cargo build --release
[ -f "$bin" ] || { echo "no binary at $bin" >&2; exit 1; }

# Every source that can change the count must be OLDER than the binary.
newest=$(find ../../crates/rusty_rtos_kernel-core/src src -name '*.rs' -newer "$bin" -print -quit)
if [ -n "$newest" ]; then
    echo "STALE: $newest is newer than $bin -- the build did not take" >&2
    exit 1
fi

rm -f callgrind.out
valgrind --tool=callgrind --callgrind-out-file=callgrind.out \
         --cache-sim=no --branch-sim=no "./$bin" 2>&1 | grep -E "checksum|rounds|refs:"
echo "--- per file ---"
callgrind_annotate callgrind.out 2>/dev/null | sed -n '18,26p'

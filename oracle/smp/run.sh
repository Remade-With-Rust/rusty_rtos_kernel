#!/bin/sh
# Build the pinned FreeRTOS kernel with TWO cores on the fake port and write
# the differential trace `tests/smp_differential.rs` replays. Run under WSL:
#   wsl -e sh -c 'cd /mnt/f/coding/rusty_RTOS/rusty_rtos_kernel/oracle/smp && sh run.sh'
set -eu
here=$(cd "$(dirname "$0")" && pwd)
kernel="$here/../../../oracle/FreeRTOS-Kernel"
[ -f "$kernel/tasks.c" ] || { echo "no FreeRTOS-Kernel at $kernel -- run \`kairos oracle fetch\` first" >&2; exit 1; }
build() {
  cc -O1 -g -Wall -Wno-unused-parameter "$@"      -I "$here" -I "$kernel/include"      "$kernel/tasks.c" "$kernel/list.c" "$kernel/queue.c" "$kernel/timers.c"      "$kernel/portable/MemMang/heap_3.c"      "$here/port.c" "$here/driver.c"
}
# S2a: every call returns before the step ends.
build -o "$here/smp_driver"
"$here/smp_driver" > "$here/smp.trace"
echo "wrote $(wc -l < "$here/smp.trace") lines to $here/smp.trace"
# S2a': takes may BLOCK, suspended in a ucontext coroutine until the task
# runs again (driver.c explains).
build -DBLOCKING_WAITS -o "$here/smp_driver_block"
"$here/smp_driver_block" > "$here/smp_block.trace"
echo "wrote $(wc -l < "$here/smp_block.trace") lines to $here/smp_block.trace"

#!/bin/sh
# Build the pinned FreeRTOS kernel with TWO cores on the fake port and write
# the differential trace `tests/smp_differential.rs` replays. Run under WSL:
#   wsl -e sh -c 'cd /mnt/f/coding/rusty_RTOS/rusty_rtos_kernel/oracle/smp && sh run.sh'
set -eu
here=$(cd "$(dirname "$0")" && pwd)
kernel="$here/../../../oracle/FreeRTOS-Kernel"
[ -f "$kernel/tasks.c" ] || { echo "no FreeRTOS-Kernel at $kernel -- run \`kairos oracle fetch\` first" >&2; exit 1; }
cc -O1 -g -Wall -Wno-unused-parameter \
   -I "$here" -I "$kernel/include" \
   -o "$here/smp_driver" \
   "$kernel/tasks.c" "$kernel/list.c" "$kernel/queue.c" "$kernel/timers.c" \
   "$kernel/portable/MemMang/heap_3.c" \
   "$here/port.c" "$here/driver.c"
"$here/smp_driver" > "$here/smp.trace"
echo "wrote $(wc -l < "$here/smp.trace") lines to $here/smp.trace"

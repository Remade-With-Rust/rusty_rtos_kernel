#!/bin/sh
# Build the API differential's C side -- the pinned FreeRTOS kernel on the
# fake port, ONE core and TWO -- and write the traces
# `tests/api_differential.rs` replays. Run under WSL, the oracle fetched by
# `kairos oracle fetch`:
#
#   wsl -e sh -c 'cd /mnt/f/coding/rusty_RTOS/rusty_rtos_kernel/oracle/api && sh run.sh'
#
# SEED and STEPS override the pinned script (the committed traces use the
# defaults below); a fresh seed is how a nightly run looks for new failures.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
kernel="$here/../../../oracle/FreeRTOS-Kernel"
[ -f "$kernel/tasks.c" ] || { echo "no FreeRTOS-Kernel at $kernel -- run \`kairos oracle fetch\` first" >&2; exit 1; }
SEED=${SEED:-0x2545f491}
STEPS=${STEPS:-20000}
for cores in 1 2; do
    cc -O1 -g -Wall -Wno-unused-parameter -DCORES=$cores \
        -I "$here" -I "$kernel/include" \
        "$kernel/tasks.c" "$kernel/list.c" "$kernel/queue.c" "$kernel/timers.c" \
        "$kernel/event_groups.c" "$kernel/stream_buffer.c" \
        "$kernel/portable/MemMang/heap_3.c" \
        "$here/port.c" "$here/driver.c" -o "$here/api$cores"
    "$here/api$cores" "$SEED" "$STEPS" > "$here/api$cores.trace"
    echo "wrote $(wc -l < "$here/api$cores.trace") lines to $here/api$cores.trace"
done

# xiao-s3-realtime

**Every Kairos kernel feature at once, on a XIAO ESP32-S3, measured in real time.**

One firmware running a realistic mixed workload — twelve application tasks, the
idle task and the timer daemon — on the **shipped** Xtensa port: real stacks, a
`SYSTIMER` tick interrupt, and `Software0` committing every switch. Each feature
carries its own measurement, taken with Xtensa `ccount` (one cycle = 4.17 ns at
240 MHz), and the run ends with pass/fail checks.

| feature | how it is exercised | measured |
|---|---|---|
| `notify_from_isr` / `queue_send_from_isr` / `semaphore_give_from_isr` | `SYSTIMER` alarm 1 every 997 us wakes three priority-5 tasks in turn | interrupt → task latency, per mechanism |
| `delay_until`, preemption | a 2 ms control loop at priority 4 doing 200 us of fixed work | release jitter, deadline misses |
| software timers + daemon | a 5 ms auto-reload timer; the callback arrives through `TickHook::timer` | callback jitter |
| mutex + priority inheritance | high (7 ms) / medium 3 ms CPU hog (11 ms) / low holding the mutex for 500 us (3 ms) | high's worst wait; inheritance observed |
| event groups | three tasks rendezvous through `event_group_sync` every 20 ms | release skew |
| message buffers | sequence-numbered, timestamped 16-byte messages every 1 ms | send → receive latency, loss, corruption |
| idle | the core halts (`waiti`) between events | CPU load |
| the tick | 1 kHz from `SYSTIMER` alarm 0 | ticks against wall time: no tick lost |

## Run it

```sh
# on the board -- 20 s after a 0.5 s warm-up; `--features long` runs 10 minutes
cargo +esp run --release

# the same workload on OS threads first: a functional check, no board needed
cd ../xiao-s3-realtime-host && cargo run --release
```

The workload lives in **one file**, [`src/workload.rs`](src/workload.rs), and is
`include!`d by both programs. [`src/main.rs`](src/main.rs) is only the S3 platform:
the clock, the kernel cell, stacks, the three interrupt handlers and startup.
[`../xiao-s3-realtime-host`](../xiao-s3-realtime-host) is the same thing on
`rusty_rtos_port-host`, so a pass there is a statement about the code that is
about to be flashed.

The host run checks **function only**. Its clock belongs to the operating system,
so its three timing checks print `skip`, and its histograms mean "it ran", not
"it was this fast". Expect `ok` on the other six:

```
      skip  the 1 kHz tick kept wall time: no tick lost (this clock is not real time)
      skip  the 2 ms control loop never missed a deadline (this clock is not real time)
      skip  priority inheritance bounded the high task's wait (no unbounded inversion) (this clock is not real time)
      ok    every feature produced measurements
      ok    no kernel stalls and no failed kernel calls
      ok    every interrupt was delivered to its task
      ok    the mutex really was contended (otherwise the bound proves nothing)
      ok    the low-priority holder was seen running at an inherited priority
      ok    no message lost, reordered or corrupted
RESULT: PASS -- every kernel feature ran at once and met its checks
```

## What a latency here does and does not include

Interrupt → task is stamped at the **first statement** of the alarm-1 handler, and
again in the woken task. It therefore includes the kernel's `_from_isr` call, the
handler's return, `Software0`'s entry, the switch and the task's resume —
everything Kairos and its port contribute. It **excludes** the hardware's vector
entry before that first statement, which no software clock on this part can see.

Idle time is counted in the switch handler, as FreeRTOS's run-time stats count at
`traceTASK_SWITCHED_IN`. That costs a few instructions inside every switch, and so
inside every latency above: the price of knowing the load.

## Predictions, written before the first board run

Recorded so the silicon numbers have something to be checked against:

| quantity | predicted | from |
|---|---|---|
| CPU load | ~52–56 % | control 10 % + medium hog 27.3 % + low holder ~14 % + high 0.7 %, plus the kernel |
| interrupt → task, p50 | ~3–5 us | `xiao-s3-cycles`: ISR wake 430 cycles + switch 166, plus handler return and resume |
| control-loop jitter, p99.9 | < 20 us | only the priority-5 tasks and the tick outrank it |
| timer-callback jitter, max | up to ~200 us | the daemon runs at priority 4, equal to the control loop, and waits out its 200 us of work |
| high task's mutex wait, max | < 1,150 us (the bound checked) | low's 500 us critical section + up to two control releases + 150 us |
| deadline misses, lost ticks, lost messages | 0 | |

## Three defects the host run caught before the board did

1. **A spurious switch on every wake.** The first version yielded after every
   `Blocked`. The kernel has already yielded inside the call by then, as the C's
   `portYIELD_WITHIN_API` does, so that was a second `Software0` per wake. It
   would have landed inside every latency measured here, and between tasks of
   equal priority it rotated the round robin a step the C never takes.
2. **Idle counted from inside idle.** Idle halts, and the interrupt that ends the
   halt switches straight to the woken task, so idle's next statement runs only
   after everyone else has finished. Timing the halt from idle counted the whole
   busy period as idle, and the load read **0.0 %**.
3. **A load computed in mixed units.** Idle time was in wall-clock microseconds,
   but the window was counted in ticks. On the host 10,050 ticks took 15,561 ms,
   and the load read **1.3 %**. The window is now measured on the wall clock, and
   "ticks = milliseconds" became a check of its own. On silicon that check
   catches a tick handler held off past its next alarm.

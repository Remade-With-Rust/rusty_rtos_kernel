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
| mutex + priority inheritance | staged every 12 ms: low takes the mutex on tick T and holds it 2.5 ms; high and a 3 ms medium hog both wake on T+1 | high's worst wait; inheritance observed |
| event groups | three tasks rendezvous through `event_group_sync` every 20 ms | release skew |
| message buffers | sequence-numbered, timestamped 16-byte messages every 1 ms | send → receive latency, loss, corruption |
| idle | the core halts (`waiti`) between events | CPU load |
| the tick | 1 kHz from `SYSTIMER` alarm 0 | ticks against wall time: no tick lost |

## Run it

```sh
# on the board -- 20 s after a 0.5 s warm-up; `--features long` runs 10 minutes
cargo +esp run --release

# the negative control: the mutex becomes a binary semaphore (no inheritance),
# and the two inheritance checks must FAIL
cargo +esp run --release --features no-inherit

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
| CPU load | ~56–60 % | control 10 % + medium hog 25 % + low holder 20.8 % + high 0.4 %, plus the kernel (staged scenario; the first prediction, ~52–56 %, was for the co-prime one) |
| interrupt → task, p50 | ~3–5 us | `xiao-s3-cycles`: ISR wake 430 cycles + switch 166, plus handler return and resume |
| control-loop jitter, p99.9 | < 20 us | only the priority-5 tasks and the tick outrank it |
| timer-callback jitter, max | up to ~200 us | the daemon runs at priority 4, equal to the control loop, and waits out its 200 us of work |
| high task's mutex wait | p50 ~1.5–1.9 ms; max < 3,050 us (the bound checked) | the ~1.5 ms low has left after T+1, plus control releases; the bound is low's whole 2.5 ms + two control releases + 150 us. With `no-inherit`, >= 4,500 us |
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

## Two more, caught by the first board run (2026-10-01)

The first run on silicon passed all nine checks and was **not admissible**. Two
of its numbers could not be true: the control loop's p50 (136.53 us) sat below
its own min (1,299.97 us), and the CPU load read 99.2 % against a predicted ~54 %.

4. **The CPU was at 80 MHz.** esp-hal's default `Config` does not select
   `CpuClock::max()`; it has to be asked for. Every time printed was
   `ccount / 240` of an 80 MHz count, so a third of the truth, and every
   `spin_us` ran three times as long. That alone accounts for the load. The
   arithmetic is exact: a 2 ms period is 160,000 cycles at 80 MHz, read as
   667 us, which is 1,333 us off nominal. The run measured 1,300–1,367 us. The
   firmware now requests 240 MHz, measures `ccount` against SYSTIMER for 10 ms
   at startup, and halts if the two disagree. That is the `clock` line in its
   output.
5. **The histogram was 136 us wide, and its overflow bin answered with its
   edge.** It now has a coarse tier to 8.9 ms, every percentile is clamped into
   `[min, max]`, and an overflowing percentile reports the max.

The run's counts are clock-independent, and they stand: 20,061/20,061
interrupts delivered, 0 of 18,457 messages bad, 909/909 contended waits
inherited, 0 stalls, 0 errors, and 20,050 ticks in 20,050 ms. Its times do not.

## The second board run (2026-10-01): clock right, inheritance never exercised

`ccount` measured 239 MHz against SYSTIMER, so this run's times are admissible.
It failed two checks: **0 contended waits out of 2,858**, and so no inheritance
was seen. The cause was the scenario, not the kernel. Every task wakes on a
tick; `hi` outranks `lo` when they wake together; and a 500 us hold almost
never spans the next tick. The first run had contended only because its spins
took three times as long at 80 MHz. The scenario is now staged (the table
above), with a negative control.

Staging it found one more defect, on the host: an anchor ahead of the tick
count makes `delay_until` never block -- FreeRTOS reads it as a tick overflow --
and `hi` starved everything below priority 3 (`pi_anchor`).

What that run measured, against the predictions made before any board run:

| quantity | predicted | measured | |
|---|---|---|---|
| CPU load | ~52–56 % | **52.8 %** | hit |
| interrupt → task, p50 | ~3–5 us | notify **6.26**, queue **7.46**, semaphore **7.33** us | missed: ~1,500–1,800 cycles, against 430 + 166 for the kernel's share in `xiao-s3-cycles`. The remainder is not yet decomposed; esp-hal's two trap entries (alarm 1, then `Software0`) are the first suspect, untested |
| control-loop jitter, p99.9 | < 20 us | **34.80** us (max 38.39) | missed |
| timer-callback jitter | up to ~200 us | p99 **221.86** us; max **1,021.19** us | p99 as predicted (the daemon shares priority 4 with the control loop's 200 us); the max is one tick late, once, unexplained |
| deadline misses, lost ticks, lost messages | 0 | **0, 0, 0** | hit |
| interrupts delivered | all | **20,060 / 20,060** | |
| message buffer, p99 | (not predicted) | **1,100.80** us | `mb_rx` shares priority 2 with the 3 ms hog and waits for the round robin |


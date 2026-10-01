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

## The third board run (2026-10-01): PASS, staged inheritance exercised

`ccount` 239 MHz against SYSTIMER; all nine checks `ok`.

| quantity | predicted | measured |
|---|---|---|
| high task's mutex wait | p50 ~1.5–1.9 ms, max < 3,050 us | p50 **1,749.33** us, max **2,017.02** us |
| contended / inheritance seen | every round | **1,667 / 1,667** |
| CPU load | ~56–60 % | **55.7 %** |
| deadline misses, lost ticks, lost messages | 0 | **0, 0, 0** (17,190 messages) |
| interrupts delivered | all | **20,060 / 20,060** |
| interrupt → task, p50 | ~3–5 us | notify **6.00**, queue **7.46**, semaphore **7.33** us -- missed, and identical to the second run |
| control-loop jitter, p99.9 | < 20 us | **34.80** us -- missed, and identical to the second run |
| timer-callback jitter | p99 ~200 us | p99 **221.86** us; max **998.22** us |
| event_group_sync skew | (73 us p99 in the second run) | p50 **11.73**, p99 **1,169.06** us |

The two misses reproduce to the hundredth of a microsecond, so they are fixed
costs to decompose, not noise. The event-group p99 moved with the staged
scenario. The hypothesis, untested: the event tasks share priority 3 with `hi`,
and during each round `lo` runs at priority 3 too, by inheritance. An event wake
inside that window waits a tick for its round-robin turn. With event wakes
cycling through three phases of the 12-tick round, about a third of
rendezvous would be exposed: p99 a tick, p50 untouched. FreeRTOS would behave
the same; the workload produces it, not the kernel.

## The negative control on the board (2026-10-01): FAILS, as it must

`--features no-inherit`, same workload, the mutex replaced by a binary
semaphore:

| quantity | with inheritance | without | |
|---|---|---|---|
| high task's mutex wait | p50 1,749.33, max 2,017.02 us | min **4,709.70**, p50 **4,744.53**, max **5,259.14** us | predicted >= 4,500: `mid`'s 3 ms burst lands inside the wait |
| contended / inheritance seen | 1,667 / 1,667 | 1,667 / **0** | |
| checks | 9 ok | inheritance bound **FAIL**, inherited priority **FAIL**, 7 ok | |
| event_group_sync skew, p99 | 1,169.06 us | **25.60** us | |

The two inheritance checks can fail, and fail only when inheritance is absent.
So the third run's PASS on them is evidence.

The event-group row is a second probe of the hypothesis above. In this run `lo`
never rises to priority 3, so it cannot share a round robin with the event tasks,
and the p99 drops from a tick to 25.6 us. The hypothesis survives a probe it
could have failed. It is not yet confirmed by a direct measurement.

## Interrupt -> task, decomposed (2026-10-01, `--features decompose`)

The headline row missed its 3–5 us prediction. That prediction added only the
kernel's inline share measured by `xiao-s3-cycles` (ISR wake 430 + switch 166),
which has no interrupt entry, no trap exit and no real switch, so it left out
the whole platform path. The decomposition stamps eleven points along the real
path. Run: 0 events rejected as incoherent; the segment p50s sum to 1,485 cycles
for notify, against a measured minimum total of 1,474. Instrument tax: the
headline p50 rose from 1,440 cycles (6.00 us) to 1,536 (6.40 us) with the stamps in.

p50 cycles at 240 MHz:

| segment | notify | queue | semaphore | owner |
|---|---:|---:|---:|---|
| A>B clear the alarm | 13 | 13 | 13 | demo |
| B>C `irq_fire` bookkeeping | 88 | 87 | 87 | demo |
| C>D kernel `_from_isr` | **312** | **460** | **441** | Kairos |
| D>E raise `Software0`, unwind | 20 | 17 | 17 | demo / port |
| E>F trap exit + `Software0` trap entry | **345** | 344 | 344 | esp-hal |
| F>G kernel `switch_context` | **103** | 103 | 103 | Kairos |
| G>H idle accounting | 24 | 24 | 24 | demo |
| H>I port context copy (twice) | **344** | 344 | 344 | port |
| I>J trap exit + task resume | 96 | 108 | 100 | esp-hal |
| J>K the call again: `Ready` | **140** | **340** | **308** | Kairos |
| **sum** | **1,485** | **1,840** | **1,781** | |

Not in the headline at all: the trap ENTRY before the handler's first
statement. A bare `FROM_CPU_INTR1`, dispatched by the same esp-hal path,
measured entry **465** (raise -> first statement, including the raise's own
register write) and exit **169**. So notify's true interrupt-to-task time is
about **1,935 cycles, 8.1 us**.

By owner, notify: esp-hal + port **785** (53 %), Kairos **555** (37 %), demo
glue **145** (10 %), plus ~450 of entry outside the headline.

Findings:

1. **Every wake takes two traps.** esp-hal's level-1 dispatcher serves one
   class per trap, CPU-internal first, so the alarm's trap returns and
   `Software0` takes a second one: 345 cycles.
2. **The context copy is 344 cycles.** `Context` carries 18 FP words
   (esp-hal's default `float-save-restore`), and the port copies it out and in.
   Nothing in this firmware uses floats.
3. **A peripheral interrupt's entry costs ~275 more than a CPU-internal one.**
   A peripheral entry is ~450. `Software0`'s exit + entry is 345, and a bare
   exit is 169, which leaves ~176 for an internal entry. The likely cause is
   esp-hal scanning the interrupt matrix for pending sources. It is upstream.
4. **Kairos's queue and semaphore wakes cost ~900 against notify's 555,** in
   `_from_isr` (+150) and in the call made again after the wake (+200).
5. **The tails sit in the kernel segments, not the trap ones.** C>D max 4,794
   and J>K max 6,130 cycles, while E>F and H>I never move off 344–345. Kernel
   code runs from flash through the cache, and the guess is cache misses. It is
   untested.

Levers, by predicted saving on notify:

| lever | predicted | where |
|---|---|---|
| Switch inside the interrupt's own trap. esp-hal already passes the trap frame to peripheral handlers. This is FreeRTOS's Xtensa shape: the switch happens on interrupt exit, with no second trap. | ~-360 (-19 %) | firmware, then the port |
| Drop FP save/restore when no task uses the FPU | ~-100..-150 on the copy, plus cheaper trap saves | `Cargo.toml` (esp-hal features) |
| Kernel queue/semaphore wake paths | up to ~-350 on those mechanisms | `rusty_rtos_kernel` |
| Interrupt-path kernel code in IRAM | the tails, if (5) holds | linker |

## The four fixes (built and measured 2026-10-01)

| fix | how to select it | predicted (notify) |
|---|---|---|
| 1. switch inside the interrupt's own trap | `--features switch-in-trap` | ~-360 cycles (the second trap, 345, plus the raise) |
| 2. no FP save/restore | `--no-default-features` (drops `fp-save`) | ~-100..-150 on the context copy, plus cheaper trap saves |
| 3. the kernel's receive path resolves the queue once | always on (`rusty_rtos_kernel` `42e183b`) | queue and semaphore only: the outlined `copy_data_from_queue` call leaves the call made again |
| 4. the interrupt path's code in IRAM | `--features iram` (`kairos_iram.x`) | the tails, if they are cache misses; little at p50 |

`board-sweep.ps1` flashes and runs all five combinations that matter (baseline, each firmware
fix alone, all three together), every one with `decompose`, and keeps each log:

```powershell
powershell -ExecutionPolicy Bypass -File .oard-sweep.ps1
```

Notes on each:

- **switch-in-trap** relies on esp-hal =1.2.1's dispatcher passing peripheral handlers the
  trap frame it restores. `#[esp_hal::handler]` accepts `fn(&mut Context)` but fails to
  type-check it in that version, so `framed()` wires the handler by hand. The pointer is
  stored as `extern "C" fn()` and called back as exactly the type it began as.
- **no FP save**: with `float-save-restore` off, xtensa-lx-rt traps with CPENABLE = 0, and the
  port zero-fills a new task's context, so every task runs with the FPU disabled. A float in a
  task faults (a coprocessor-disabled exception) instead of corrupting another task's FP state.
  This is right for this firmware, which uses no floats, and wrong for one that does.
- **iram**: `build.rs` passes `kairos_iram.x` in place of `linkall.x`. It is `linkall.x` with
  esp-hal's `esp32s3.x` inlined and one block added, because that block must follow esp-hal's
  last `INSERT` and precede its `.text`. D/IRAM is one physical SRAM, so the script reserves
  matching DRAM, and an `ASSERT` fails the link if `.data` could overlap the moved code.

### Measured: `board-sweep.ps1`, 2026-10-01 (all five PASS, 0 incoherent events)

Every build carries `decompose` (about 96 cycles of stamps), and the kernel fix is in all five.

| build | notify p50 | queue p50 | semaphore p50 | notify p99 | notify max | control p99.9 | notify cycles (sum) |
|---|---:|---:|---:|---:|---:|---:|---:|
| baseline | 6.26 us | 7.60 | 7.33 | 8.26 | 62.70 | 41.73 | 1,498 |
| + switch-in-trap | **5.06** | **6.26** | **6.00** | 9.20 | 63.50 | *60.80* | 1,203 |
| + no FP save | **5.86** | **7.06** | **6.93** | 13.73 | 69.82 | *65.06* | 1,356 |
| + iram | 6.26 | 7.60 | 7.33 | **6.40** | **17.87** | 38.53 | 1,496 |
| **all three** | **4.53** | **5.73** | **5.60** | **4.66** | **15.91** | 37.06 | **1,072** |

All three together: interrupt -> task **-28 % (notify), -25 % (queue), -24 % (semaphore)** at p50,
the p99 from 8.26 to 4.66 us, and the worst case from 62.7 to 15.9 us.

Against the predictions:

1. **switch-in-trap: -295 cycles, predicted ~-360.** The second trap is gone (E>F 345 -> 4). But
   trap exit + resume rose 100 -> 156: the alarm's trap now exits through esp-hal's peripheral
   dispatcher, which finishes its pending-source loop, where `Software0` exited by the short
   CPU-internal path.
2. **No FP save: -142 cycles, predicted -100..-150.** The context copy fell 347 -> 247, the trap
   pair 345 -> 314, and exit + resume 100 -> 84. A bare peripheral trap's entry and exit fell
   465 / 169 -> 451 / 152.
3. **The kernel's single resolve: the queue's and the semaphore's call made again fell 340 ->
   292 and 308 -> 260** (-48 each), against the run before it. Notify is unchanged at 140.
4. **IRAM: p50 unchanged, as predicted, and the tails were the instruction cache.** The kernel
   segments' worst cases collapse: `_from_isr` max 4,788 -> 314 cycles, the call made again
   5,142 -> 2,069, `irq_fire` 2,322 -> 87. Notify's max fell 62.7 -> 17.9 us and its p99
   8.26 -> 6.40.

Two findings the fixes did not predict:

- **Fixes 1 and 2 alone made the control loop's tail WORSE** (p99.9 41.7 -> 60.8 and 65.1 us), and
  **with IRAM it is better than baseline** (37.1 us). A change that moves code in flash moves the
  cache conflicts with it, so in a flash build the tail is set by layout rather than by the code.
  Ship them together.
- **The event-group skew's one-tick p99 (1,160 us) disappears in every IRAM build** (30.5 and
  11.1 us). The round-robin explanation recorded above does not predict that; it is reopened.

What remains in the tails: trap exit + task resume keeps a ~2,850-cycle max in every build,
and every mechanism's p99.9 stays at 11–15 us. The likely cause is the tick and the interrupt
source landing in the same window (997 us against 1,000 us beat about every third of a second).
That is untested.

### The second sweep (2026-10-01): fixes as the default, the tick test, one refutation

`board-sweep.ps1` after the defaults flipped; all five PASS. Run 1 is the default, and each
other run opts one fix back out. p50 cycles, notify (sum of segments):

| build | notify | queue | semaphore | headline notify p50 |
|---|---:|---:|---:|---:|
| default (all three fixes) | **1,061** | **1,371** | **1,317** | **4.40 us** |
| `software0-switch` | 1,328 | 1,638 | 1,584 | 5.60 |
| `fp-save` | 1,286 | 1,595 | 1,541 | 5.33 |
| `flash-code` | 8,306 * | 1,372 | 1,318 | 12.26 * |
| none of the three | 1,590 | 1,890 | 1,835 | 6.66 |

\* In this flash build, notify's call made again ran at **7,370 cycles at p50**: a layout whose
cache conflicts land on that path every time. The previous sweep's flash builds did not show
it. IRAM is what makes these numbers stable from build to build.

**The tick test confirms the tail hypothesis.** In every IRAM build, the events with no tick
handler inside them have **no tail at all**: default notify clean n = 6,613, p50 1,056, p99 1,098,
**max 1,098 cycles (4.6 us)**. Every event above that had a tick inside it (74 events, 1.1 %,
max 3,806). The same holds for queue (clean max 1,403) and semaphore (1,352). In flash builds the
clean events still reach 12,000+ cycles: that is the cache, on top of the tick. What is left in
the tail is real concurrent work. The tick's own trap and its wake-ups land inside the
interrupt's window, which is load the system has, not a defect in the path.

**The event-group skew's one-tick p99 is layout too.** It is 1,152–1,169 us in both flash builds
and 27–36 us in all three IRAM builds. The round-robin explanation is withdrawn.

**Measured and kept:**
- `irq_fire`'s bookkeeping: 87 -> 36 cycles. The tick test's own hook costs +9 in A>B (13 -> 22).

**Measured and reverted:**
- An in-line context copy in the port in place of `copy_nonoverlapping`. That call lowers to the
  S3's mask-ROM `memcpy`, and the ROM routine won: 247 vs 278 cycles without FP save, and 347 vs
  484 with it. It is recorded at the call in `rusty_rtos_port-xtensa`. With the ROM copy back,
  the default's notify path projects to about 1,030 cycles. That is a projection from the two
  sweeps, not a measurement.

From the first decomposition (1,485 cycles, notify, every fix off) to the default (1,061
measured, with the slower copy): **-29 %**, and the worst case without a coincident tick is
4.6 us. The earliest plain build, without the stamps, read 6.00 us.


# SMP for `rusty_rtos_kernel` (`configNUMBER_OF_CORES > 1`)

**Status, 2026-10-01: slices S1 and S3 pass. The scheduler core is SMP,**
**one kernel runs BOTH cores of a XIAO ESP32-S3, and the one-core build is**
**unchanged.** S1b landed 2026-10-02 (7 more two-core scenarios, 16 in all; one core +2 Ir, the new field's initialisation). S2a (the scheduler differential against the C, 20,000 steps) landed 2026-10-02; S2b (the threaded SMP demos) remains. This is the K8 SMP half of
`docs/plans/rtos-mission.md` in the umbrella.

## 0. Why

The ESP32-S3 is dual-core and IDF-FreeRTOS is SMP. Kairos on one core cannot
replace the kernel Janus Track A runs, whatever the C ABI (K6) does. So SMP
is a prerequisite for that, not a completeness item.

**The one-line test for every SMP change:** the one-core build must stay
conformance-identical to the C kernel *and* instruction-count-neutral.
Everything SMP is gated on the `const` `C::NUMBER_OF_CORES` and must fold
away to nothing at 1.

## 1. The model

Everything here is read from the pinned `tasks.c` (V11.3.1), with
`configRUN_MULTIPLE_PRIORITIES == 1` and no core affinity. Section 4 says
what is not implemented yet.

| C | Kairos |
|---|---|
| `pxCurrentTCBs[ core ]` | `current: [TaskHandle; MAX_CORES]` (plus the cached `current_priority` per core) |
| `xYieldPendings[ core ]` | `yield_pending: [bool; MAX_CORES]` |
| `pxTCB->xTaskRunState` | **not stored**. "Running on core `c`" IS `current[c] == task`. The one state that cannot be derived, `taskTASK_SCHEDULED_TO_YIELD`, is a per-core flag, `yield_requested[c]`. The TCB is unchanged, so one core pays nothing for it. |
| `portYIELD_CORE( x )` | the kernel sets bit `x` of `core_yields`; the port drains it with `Kernel::take_core_yields()` and raises the inter-processor interrupt. The kernel stays `forbid(unsafe)`: raising another core's interrupt is the port's act. |
| `prvCreateIdleTasks` | `IDLE0` and `IDLE1`, each the `current` of its core from `start_scheduler` on. SMP task creation never assigns `current`. |
| `prvSelectHighestPriorityTask` | `select_for_core`: move this core's task to the END of its ready list, then walk each level from its HEAD, skipping tasks another core holds. |
| `prvYieldForTask` | `yield_for_task`: yield the core running the lowest-priority task the new one outranks. A running idle task ranks one below priority 0. Ties go to the higher core. |
| tick | time slicing is per core; every other core owing a yield is asked through `prvYieldCore`. |

### Two tests corrected by the kernel

Both times, I wrote down the wrong expectation and the transcription was
right:

1. **Idle tasks migrate.** With no core affinity, core 1 walking level 0
   from the head takes `IDLE0` if core 0 has left it.
2. **Being asked to yield does not mean handing over the core.** A
   priority-0 task readied while core 1 idles makes core 1 yield. But the
   head of level 0 is an idle task, so core 1 takes that first. The new task
   runs only when the idle task yields (`configIDLE_SHOULD_YIELD`).

## 2. Evidence (S1)

- **One core, conformance:** `rusty_rtos_demo`'s `tests/conformance.rs` (all
  25 scenarios against the C kernel's pinned digests) passes against this
  kernel. This needed `rusty_rtos_demo` moved to port `0.3`; it had been
  resolving the published port and the published kernel since the 0.3.0
  release.
- **One core, instruction counts** (`bench/*-ir`, callgrind, WSL). Same
  binary shape, arm A = `main` before SMP; every checksum is identical:

  | bench | before | after | |
  |---|---:|---:|---:|
  | `ksched-ir` | 1,528,677 | 1,521,864 | −0.45 % |
  | `kdelay-ir` | 3,441,325 | 3,445,812 | +0.13 % |
  | `kipc-ir` | 4,978,074 | 4,972,911 | −0.10 % |
  | `khot-ir` | 14,854,739 | 14,848,045 | −0.05 % |
  | `kobj-ir` | 4,851,525 | 4,848,494 | −0.06 % |

  It took two measured fixes to get there. The first cut read **+2.55 %**
  on `ksched-ir`:
  1. **The `Option` form of the per-core accessors cost +14,985 Ir.**
     `current.get(core).copied().unwrap_or(..)` does not fold to the scalar
     load at a constant 0 index. Destructuring element 0 on the one-core
     path does fold; residue +2,985.
  2. **Three SMP fields placed beside `yield_pending` cost +24,000 Ir,
     with no SMP logic running.** `Kernel` is `#[repr(C)]`, and the fields
     moved every hot field behind them. Moved to the end of the struct,
     they cost nothing. A bisect over step 1 alone found this: the layout,
     not the code. Moving the SMP bodies out of line (`#[cold]`,
     `#[inline(never)]`) measured 0 and was kept anyway, for MIR-inliner
     hygiene.
- **Two cores, semantics:** `src/smp_tests.rs`, 9 scenarios with
  expectations read off the C. They cover:
  - start;
  - distinct tasks per core;
  - preemption of the lowest core;
  - one interrupt per request;
  - idle ranking;
  - time slicing on both cores;
  - yield order;
  - a semaphore wake landing on the lowest core;
  - one core never asks for a cross-core yield.

  These are transcription tests, not conformance. Section 3 is what turns
  them into conformance.

## 3. Remaining slices

| slice | what | kill test |
|---|---|---|
| **S1b -- DONE 2026-10-02** | The one-core-only sites, in priority order:<br>- `vTaskDelete` / `vTaskSuspend` of a task running on ANOTHER core (`prvYieldCore` that core, deferred delete);<br>- `vTaskPrioritySet` (`taskYIELD_TASK_CORE` / `ANY_CORE`);<br>- `vTaskResume`, notify-give, the pending-ready drain in `xTaskResumeAll`;<br>- `prvCheckForRunStateChange` on suspend/critical entry. | each with a `smp_tests` scenario from the C |
| **S2a -- DONE 2026-10-02: the scheduler differential.** `oracle/smp/` builds the pinned V11.3.1 with two cores on a fake port; a 20,000-step random script of create, delete, suspend, resume, priority-set, delay, give, give-from-ISR, take, tick and yield, each from a chosen core, writes `smp.trace`. `tests/smp_differential.rs` replays it against Kairos: **identical on every line, first run.** The instrument has been seen to fail: flipping the tie-break in `yield_for_task` from `<=` to `<` diverges at step 8. Coverage: 759-1,954 of each op, 2,455 cross-core yields, both cores on app tasks in 9,434 steps, idle migration in 3,715. Blocking event waits are not in the script (a single-threaded C driver cannot run a blocking take), and `configRUN_MULTIPLE_PRIORITIES` is 1 here. | the trace, line for line |
| **S2a' -- DONE 2026-10-02: blocking waits in the differential.** The same driver built with `-DBLOCKING_WAITS` (`smp_block.trace`): a take may now BLOCK for 1-6 ticks. The C runs each blocking take in a ucontext coroutine; `traceBLOCKING_ON_QUEUE_RECEIVE` marks the block and the yield that follows leaves the kernel, exactly where a real port does. When that task is next current, on EITHER core, the step is `cont`, and the C's own `for(;;)` carries on with its own timeout state. Kairos does the same through `Wait::Blocked`'s retry protocol. **Identical on every line, first run:** 688 blocking takes, of which 246 blocked; on continuation 170 were woken by a give, 40 timed out, 18 blocked again. S2a's trace is byte-identical with the new yield hook. **Poison:** a block that times out one tick early (`elapsed + 1 < held` in `check_for_timeout`) diverges at step 41 -- and S2a passes the same poison, so this script is what proves block and timeout on two cores. | the trace, line for line |
| **S2b** | **The C oracle with two cores.** FreeRTOS V11.3.1 built with `configNUMBER_OF_CORES 2` on a deterministic two-core sim port. The cores interleave at critical-section exits (sim contract v1, per core), the trace carries a core id, and `kairos conform` diffs it. | the SMP demo tasks (`FreeRTOS-SMP-Demos`) trace-identical |
| **S3 -- PASSED 2026-10-01** | **Silicon: both S3 cores** (`rusty_rtos_port/firmware/xiao-s3-smp`). Two spins on two cores take 342 ms against 301 ms alone (one core would need ~603). 2,000/2,000 cross-core ping-pong laps at 16.1 µs per hand-off, with 4,009 IPIs. The hand-off cost is untuned. Built from:<br>- start core 1 (`esp-hal` `CpuControl`);<br>- a cross-core spinlock around `with_kernel`;<br>- `Software0`/`Software1` per core as the switch;<br>- `take_core_yields` raising the other core's `FROM_CPU_INTR`. | the corpus checks on both cores for an hour; a cross-core wake measured in cycles |
| S4 | `configUSE_CORE_AFFINITY`, `configRUN_MULTIPLE_PRIORITIES == 0`, `configUSE_TASK_PREEMPTION_DISABLE` | the C oracle with each switched on |

## 4. Not implemented, on purpose until asked

- **Core affinity, and the `pxPreviousTCB` re-placement it implies:** S4.
- **`configRUN_MULTIPLE_PRIORITIES == 0`:** S4.
- **A stackless SMP runner.** `hand_over`'s unwinding marker is single, so
  SMP is for ports that commit their own switches (`COMMITS_SWITCH`), which
  is every silicon port. The S2 sim must be a stacked one, or carry one
  marker per core.

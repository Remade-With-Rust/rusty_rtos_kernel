### In The Wild with 142 Active Installs

FREE RAG Converter Online -- <a href="https://RAGconverter.com">RAGconverter.com</a>

# rusty_rtos_kernel

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![crates.io](https://img.shields.io/crates/v/rusty_rtos_kernel.svg)](https://crates.io/crates/rusty_rtos_kernel)
[![docs.rs](https://docs.rs/rusty_rtos_kernel/badge.svg)](https://docs.rs/rusty_rtos_kernel)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

The Kairos scheduler: FreeRTOS's kernel remade in Rust. Tasks, the tick,
queues, semaphores, mutexes with priority inheritance, task notifications,
software timers, event groups, and stream and message buffers. No C, no FFI.

Its correctness claim is not a test suite. It is a line-by-line diff against
the C kernel's own execution trace.

- **Proven**: **22 conformance scenarios** produce traces identical to the C
  kernel's own, and the same corpus runs on four architectures — the last of
  them silicon. On the two emulators it is **25 of 25**, identical field for
  field between them. Separately, **26 unmodified
  C demo files** from the FreeRTOS distribution link against it through
  [`rusty_rtos-capi`](https://github.com/Remade-With-Rust/rusty_rtos-capi) and
  pass their own checkers.
- **The switch is not here.** A context switch is a stack swap and a stack swap
  is `unsafe`, which this family allows only in `rusty_rtos_port-<arch>`. This
  crate decides *which* task runs and fires the same `TASK_SWITCHED_OUT` /
  `_IN` pair the C kernel fires; a port acts on it.

**Known gaps.** SMP is two cores only, with no core affinity; no MPU, no static
allocation (`xTaskCreateStatic` and friends — this kernel places objects in
arenas declared at compile time, so there is nothing for a caller's buffer to
be placed into), and no co-routines, which are a declared non-goal. A queue item
is at most eight bytes and `xQueueGenericCreate` refuses rather than
truncating.

- This package's plan: [docs/plans/rusty_rtos_kernel.md](https://github.com/Remade-With-Rust/rusty_rtos_kernel/blob/main/docs/plans/rusty_rtos_kernel.md)
- Every number: [docs/LEDGER.md](https://github.com/Remade-With-Rust/rusty_rtos_kernel/blob/main/docs/LEDGER.md)
- The family plan: Kairos [`docs/plans/rtos-mission.md`](https://github.com/Remade-With-Rust/kairos/blob/main/docs/plans/rtos-mission.md)

**Claims discipline:** this README makes no performance or capability claim that
is not backed by a test, a benchmark ledger entry, or a kill test recorded in
the plan. "Scaffold" means scaffold. "Sim only" means the sim port; "builds, not
flashed" means no chip has run it.

## Conformance

Every scenario is a FreeRTOS demo file remade as a state machine, run against
the C kernel compiled from pinned sources, with both sides emitting a trace
line per kernel event. Identical means identical: same lines, same order, same
counters.

| | |
|---|---|
| scenarios identical to the C kernel, on the host | **22** |
| ticks per scenario | 2,000 pinned · verified again at **100,000** |
| on each emulator, Cortex-M3 and RV32 | **25 of 25**, identical field for field |
| architectures | host, ARMv7-M, RV32, Xtensa LX7 |
| soak, both emulators | **RV32 25/25 · M3 25/25**, identical field for field (≈11.5 min each, run concurrently — a contended wall, not a timing) |
| soak, **on silicon** | **25/25** at 3,600,000 ticks on a XIAO ESP32-S3 — every row identical to both emulator hours (2026-09-23) |
| on silicon (XIAO ESP32-S3) | **25/25**, re-run 2026-09-23 — identical field for field to both emulators |
| unmodified C demo files linking against it | **26** |

```sh
kairos conform --all --ticks 100000      # from the Kairos umbrella
```

What that covers: tasks, the tick with pended-tick replay, the context-switch
decision, the ready/delayed/suspended lists, delay, delay-until, suspend,
resume, priority set, suspend-all and resume-all, abort-delay; queues at both
ends with blocking sends, receives and peek; binary and counting semaphores;
mutexes with priority inheritance, disinheritance and
disinheritance-after-timeout, and the recursive variant; task notifications;
software timers and their daemon; event groups; stream and message buffers;
and task deletion with its deferred reclamation.

**Still open:** `AbortDelay` is 1,945 of 2,549 lines identical and the next
line is a **contract** question rather than a kernel one — the C harness keys a
queue's trace ordinal on its malloc address, so a differently-sized successor
to a freed object gets a new ordinal where our arena reuses the freed index.
It is recorded, not hidden, and runs on its own with `kairos conform
AbortDelay`.

**Closed 2026-09-21, and it was never a kernel defect:** the 0.2.0 release
gate found `StreamBufferDemo` diverging on both emulators. The cause was in
the *scenario* — the echo client's send length wraps at
`sbSTREAM_BUFFER_LENGTH_BYTES - sizeof(size_t)`, and `sizeof(size_t)` was
spelled as the running machine's pointer width rather than the oracle's, so
the length walked 1..=22 on the host and 1..=26 on every 32-bit target. It was
not trace text: the scenario sent different data, and no counter could see it
because the number of sends never changed. Both emulators are now 25 of 25.

## SMP (two cores)

Set `Config::NUMBER_OF_CORES = 2` and the kernel schedules two cores, read
from the pinned FreeRTOS V11.3.1 `tasks.c` with `configRUN_MULTIPLE_PRIORITIES
= 1` and no core affinity (`docs/plans/smp.md`). Three independent proofs that
it does what the C does:

- **The standard demo tasks, on two cores, trace for trace.** Nine
  `Demo/Common/Minimal` scenarios run on two cores against FreeRTOS built
  with `configNUMBER_OF_CORES 2` (the umbrella's `oracle/harness-smp`, a
  deterministic two-core port): every line identical at 20,000 ticks, 900,000
  lines in all, pinned in `rusty_rtos_demo`'s `smp_conformance` test. Three
  scenarios fail their own checks on both sides -- they measure single-core
  timing or assert single-core exclusion -- and fail identically, at the same
  line.
- **The scheduler, step for step.** `tests/smp_differential.rs` replays two
  20,000-step random scripts the C ran first (`oracle/smp/`): one where every
  call returns, one where takes BLOCK and are continued later, possibly on
  the other core. Both identical; each has been seen to fail on a
  one-character change.
- **On silicon.** One kernel schedules both cores of an ESP32-S3
  (`rusty_rtos_port/firmware/xiao-s3-smp`): two spins in parallel, and 2,000
  cross-core hand-offs.

A one-core build is unchanged: conformance-identical and instruction-count
neutral.

**What two cores cost (0.3.2).** Fifteen measured changes to the SMP paths
(selection, the yield-for-task test, the core id, the reap), each kept only
if every gate above still held and the one-core count did not move:

| two-core shape (callgrind, the kernel's own instructions) | 0.3.1 | 0.3.2 | |
|---|---:|---:|---:|
| semtest, 20,000 ticks | 19,518,825 | 14,879,499 | **-23.8%** |
| BlockQ, 20,000 ticks | 22,197,142 | 17,684,287 | **-20.3%** |
| recmutex, 20,000 ticks | 5,226,170 | 3,947,934 | **-24.5%** |
| the two-core differential | 4,867,373 | 3,318,645 | **-31.8%** |

Measured on the 64-bit host, which is not a target: the changes were chosen
not to depend on pointer width, but no 32-bit two-core count has been taken,
and there is no C arm for these rows yet. Every change, and the fourteen
refuted on the way, is in the Kairos umbrella's `bench/smp-ir` and LEDGER.

**Not covered:** core affinity, `configRUN_MULTIPLE_PRIORITIES = 0` (the C's
default when SMP is off), and more than two cores.

A port drains cross-core yields with `Kernel::take_core_yields()` and raises
its inter-processor interrupt; the kernel stays `forbid(unsafe)`.

## Flash profile (`small`)

The default build inlines the hottest kernel paths for speed. The `small`
feature gives the queue take and send bodies and `xTaskResumeAll` one
out-of-line body each, for parts where flash is the constraint. On rv32,
against the C kernel's 13,924 B for the same operations: **17,318 B (1.24x)**
with `small`, 19,450 B (1.40x) without. Behaviour is identical (every test
and the conformance corpus pass with it on); the cost is instructions on the
short queue paths, e.g. `recv_empty` 36 -> 69 and `queue_roundtrip` 113 -> 198
retired instructions (`docs/LEDGER.md` has every row).

## Tickless idle

Three functions, each the C's, and all inert unless
`Config::USE_TICKLESS_IDLE` is set — with it off the conformance differential
is unchanged, byte for byte.

| ours | the C | note |
|---|---|---|
| `expected_idle_time` | `prvGetExpectedIdleTime` | zero unless the idle task is genuinely the only runnable thing |
| `step_tick` | `vTaskStepTick` | leaves the **last** tick *pended* rather than stepped, so `increment_tick` wakes the delayed task through the same code that would have woken it. The whole invariance rests on this line |
| `idle_suppress_ticks` | the `configUSE_TICKLESS_IDLE` block of `prvIdleTask` | double-sample and all |

A port that oversleeps is **clamped rather than trusted** — winding the clock
past a task's wake time loses the wake, where losing the extra sleep is
recoverable, and this kernel may not panic.

Poison-proved: stepping the last tick instead of pending it fails the
invariance test in unit tests, and on hardware makes every lap arrive one tick
late — 421 ticks where the arithmetic says 400 — **with the schedule digest
unchanged**, because nothing was ever out of order. An order check alone cannot
see a wake that is late; the cells bound the clock as well.

## Using it

A system declares its geometry; the kernel is sized for exactly what was
declared.

```rust
use rusty_rtos_kernel_core::{system, Kernel};

// Tasks and queues are declared, and the arithmetic is the declaration:
// no allocator, and the `.bss` cost is what you asked for.
system! {
    mod app use MyConfig;
    tasks { producer: 1, consumer: 2 }
    queues { work: u32; 8 }
}

type K = app::Kernel<MyPort, NoTrace, NoTickHook>;

fn main() {
    let mut k = K::new(MyPort::default(), NoTrace).expect("the geometry adds up");
    let system = app::System::build(&mut k).expect("nothing here can fail");

    k.start_scheduler().expect("start");

    // The queue came back typed, and carries what it was declared to.
    let _ = system.work.send(&mut k, 42u32, 0);
}
```

The FreeRTOS names are all present on the kernel itself — `queue_send_generic`,
`queue_receive`, `timer_create`, `event_group_wait_bits`, `task_notify` — with
the C's semantics and Rust's ownership.

## Performance

Two rows matter, and the second is the one that makes the first readable.

| Cortex-M3, both arms the `PendSV` handler | instructions |
|---|---:|
| FreeRTOS `xPortPendSVHandler` | 19 |
| Kairos `PendSV` | **19** |

**Exact parity.** On ARM both kernels reach the switch the same way — their
`portYIELD()` pends `PendSV` and so does ours — so the shapes match and so do
the counts.

| RV32, preemptive switch | instructions | vs C |
|---|---:|---:|
| FreeRTOS | 83 | 1.00× |
| Kairos | **74** | **1.12× cheaper** |

A cooperative Kairos switch on RV32 measures 2.77× cheaper, and on its own
that reads as a win. **It is not one**, and the ARM control above is what shows
that: the RISC-V gap is about *where a yield is taken* — FreeRTOS's
`portYIELD()` is `ecall`, which takes the interrupt trap and must save
everything an interrupt could have clobbered — not about one kernel moving
registers more cheaply.

### The tick and the scheduler, against the C, on one machine

Both arms run on QEMU `virt` under `-icount shift=0`, where `minstret` is
exactly reproducible. The C arm is **FreeRTOS V11.3.1 from the pinned oracle,
unmodified**, with the oracle's own first-party RISC-V port.

| rv32, retired instructions per call | FreeRTOS | Kairos | |
|---|---:|---:|---:|
| tick, empty delayed list | 15 | **9** | **0.60× — faster** |
| tick, one task delayed | 15 | **9** | **0.60× — faster** |
| scheduler selection | 27 | **45** | 1.67× against us |

**Selection is against us and is published as a finding, not a caveat.** Its
floor is 30, measured by deleting both handle validation and the stackless
bookkeeping — so the last 15 instructions are the safety and the RAM saving,
and below 30 is C's data representation, not ours. But a switch is selection
*plus* the register file, and the two kernels put their weight in opposite
halves, so quoting the selection row alone is quoting a third of the answer:

| whole switch, rv32 | FreeRTOS | Kairos | |
|---|---:|---:|---:|
| cooperative (a yield, a queue that blocks, a semaphore take) | 27 + 83 = 110 | 45 + 30 = **75** | **0.68× — 32% faster** |
| preemptive (a tick or ISR switches you out) | 27 + 83 = 110 | 45 + 74 = **119** | 1.08× against us |

**C pays 110 for every switch**, because its `portYIELD()` is a trap. Kairos
pays 75 for a yield and 119 for a preemption, and a real application's mix is
dominated by yields.

These rows hold on the **shipped** RISC-V port, not only on the simulator: the
bench takes `--features real-port`, which runs the kernel on `RiscvPort` itself.

### Memory and flash, against the C

| | FreeRTOS | Kairos | |
|---|---:|---:|---:|
| RAM per task | 596 B (TCB + 512 B stack) | **176 B** | **0.30×** |
| RAM per event group | 28 B | **8 B** | **0.29×** |
| RAM per queue | 72 B | **64 B** | 0.89× |
| static RAM, a blinker | 1,704 B | **1,640 B** | 0.96× |
| flash, kernel + RISC-V port | 13,924 B | 19,484 B | 1.40× against us |

**Flash is the price of the rows above it.** Decomposed to the byte, the
structural extra is handle validation (1,464 B — what turns a stale handle
into a typed error instead of undefined behaviour), the stackless resume
machinery that deletes the per-task stack, and `u16` list ids; that floor is
about 1.27×. The rest is speed already spent on the queue fast paths. The RAM
saved pays for the flash at roughly fourteen tasks — earlier still in the
resource a microcontroller actually runs out of.

Gated twice: identical work-parity anchors — with the tick count read back, so
FreeRTOS's `uxSchedulerSuspended` early-out cannot pass as a tick — and a
**poison** build of every arm that makes the measured call twice per bracket
and requires every row to move.

### On silicon

XIAO ESP32-S3, Xtensa `ccount` at one cycle of resolution, median of 512 with
the instrument's own tax measured and subtracted. Measured 2026-09-21, before
the instruction-count work in 0.2.1; not re-measured on silicon for this
release, so treat them as an upper bound:

| | cycles | at 240 MHz |
|---|---:|---:|
| tick | **54** | 225 ns |
| context switch | **166** | 691 ns |
| ISR-API wake, to the task holding the value | **430** | 1,792 ns |

A full scheduling round (queue send, queue receive, two switches) costs
**3,724 ns / 893 cycles**, which is **39 ppm** of the P-256 signature it was
measured beside.

> Those silicon figures replaced a set measured through a harness defect on
> 2026-09-21 — they read 131 / 623 / 949 and 1,995. No kernel code changed:
> the measurement cells hand-rolled a `NoTrace` that shadowed the one this
> family ships, inheriting `WANTS_NAMES = true`, so every traced event built a
> task name for a sink that discards it. Three instruments on three
> architectures agree on the correction, and the whole episode is written up
> in the umbrella's ledger — including the finding it withdrew.

```sh
bench/tick-work/run.sh       # the tick and selection rows, both arms
bench/kernel-ram/run.sh      # the RAM rows
bench/kernel-flash/run.sh    # the flash row
bench/switch-cost/run.sh     # the register half
```

## Portability

| target | corpus | notes |
|---|---|---|
| host (x86-64 Windows, Linux) | ✅ **22 identical to the C kernel** | the sim port, and a threaded host port |
| `thumbv7m-none-eabi` (Cortex-M3) | ✅ **25/25** | QEMU `mps2-an385` |
| `riscv32imac-unknown-none-elf` | ✅ **25/25** | QEMU `virt` |
| `xtensa-esp32s3-none-elf` | ✅ **25/25** | **on silicon**, a XIAO ESP32-S3 |

All three carry **identical rows, field for field** — same ticks, yields,
exits, line counts and digests on three instruction sets, two of them emulated
and one a real part.

Xtensa needed no context-switch port to run the corpus at all — a scenario is a
state machine and a task owns no stack, so the switch is what you need to host
tasks *with* stacks, not what you need to prove conformance on a part.

## Layout

```text
crates/rusty_rtos_kernel          facade: re-exports + prelude; the crate you depend on
crates/rusty_rtos_kernel-core     no_std (+ alloc); forbid(unsafe); types, traits, algorithms
firmware/                per-chip example projects, excluded from the workspace
docs/plans/              this package's plan and its hardening audit
docs/LEDGER.md           every number, with its method line
```

## Build

```sh
cargo test --workspace                                   # host: the tests
cargo check -p rusty_rtos_kernel-core --no-default-features \
  --target thumbv7em-none-eabihf                         # Cortex-M4F class, no alloc
cargo check -p rusty_rtos_kernel-core --no-default-features --features alloc \
  --target riscv32imac-unknown-none-elf                  # ESP32-C6 class, with alloc
```

CI holds the core to `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`,
`riscv32imac-unknown-none-elf` and `riscv32imafc-unknown-none-elf`, with and
without `alloc`, plus `cargo deny check`. Firmware examples (Xtensa needs the
esp toolchain; Cortex-M and RISC-V work on stable) are built from their own
directories under `firmware/`.

## Part of Remade With Rust

This crate is part of **[Kairos](https://github.com/Remade-With-Rust/kairos)** —
FreeRTOS remade in memory-safe Rust, as independent packages that expose the API
a FreeRTOS developer already knows and prove every scheduling decision against
the C kernel's own trace. `rusty_rtos_kernel` is the scheduler at the centre of it.

**Where this sits for Mata.** Kairos is the real-time layer on the device
itself, and [`rusty_rtos_mqtt`](https://crates.io/crates/rusty_rtos_mqtt) is the way out of it.
Paired with the **MATA distributed cloud**, robotics and sensor data has two
routes — read it on the machine, or reach it through the cloud — with the same
memory-safe crates at both ends.

The family:
[`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core) (the shared vocabulary),
[`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel) (the scheduler),
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port) (the architecture seam),
[`rusty_rtos_heap`](https://crates.io/crates/rusty_rtos_heap) (the allocators),
[`rusty_rtos_json`](https://crates.io/crates/rusty_rtos_json) (coreJSON),
[`rusty_rtos_sntp`](https://crates.io/crates/rusty_rtos_sntp) (coreSNTP),
[`rusty_rtos_mqtt`](https://crates.io/crates/rusty_rtos_mqtt) (coreMQTT),
[`rusty_rtos_backoff`](https://crates.io/crates/rusty_rtos_backoff) (backoffAlgorithm),
[`rusty_rtos-capi`](https://crates.io/crates/rusty_rtos-capi) (the C ABI) and
[`rusty_rtos_demo`](https://crates.io/crates/rusty_rtos_demo) (the conformance corpus).
All ten are on crates.io. Also check out
the rest of **[github.com/remade-with-rust](https://github.com/remade-with-rust)**.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

**[Mata Network](https://www.mata.network/)** builds sovereign, self-hostable
privacy infrastructure — *"stop sacrificing your privacy for convenience"*:
wallet & identity, a password manager, a contact manager, and a browser
extension that stops your information leaking as you browse.

**Remade With Rust** is our open-source home for the permissively-licensed
building blocks that work depends on — including
[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs) (the
FFmpeg alternative) and [FFAI](https://github.com/Remade-With-Rust/FFAI) (the
AI media toolkit).

→ **[www.mata.network](https://www.mata.network/)**

<!-- /ORG BOILERPLATE -->

## License

MIT OR Apache-2.0, at your option. FreeRTOS is MIT-licensed by Amazon.com,
Inc. or its affiliates; this crate remakes its API and behaviour from the
published sources and links no FreeRTOS code.

---

## Security

The **[threat model](docs/threat-model.md)** states what this unit protects,
who it protects it from, and — in a section of its own rather than as a
footnote — the **residual risks it does not cover**, each with the condition
that closes it.

Two are worth knowing before you adopt it:

- **There is no privilege separation between tasks.** That is what an RTOS
  is; the MPU package that would add one ships after 1.0.
- **No secret enters this crate by design**, so there is nothing here to
  zeroize. That is a constraint the model commits to, not an observation
  about today — if that ever stops being true, the model is wrong and says so.

<!-- HARDENING-TABLE:BEGIN generated by use-protection-please — edit docs/plans/use-protection-please.md, not this block -->
## Hardening status

**Tier** critical-path · **Audited** 2026-10-01 (deep) · **v1.0.0 gates** 15/16 · [Full checklist](docs/plans/use-protection-please.md)

`██████████████████░░` **94%** &nbsp;·&nbsp; 31 Completed · 0 Scheduled · 2 Incomplete · 22 N/A

| Phase | ✅ Completed | 🗓 Scheduled | ⬜ Incomplete | · N/A |
|---|--:|--:|--:|--:|
| 0 — Threat modeling | 2 | 0 | 0 | 0 |
| 1 — Toolchain | 3 | 0 | 0 | 1 |
| 2 — Supply chain | 8 | 0 | 0 | 0 |
| 3 — Code level | 7 | 0 | 0 | 0 |
| 4 — Static analysis | 1 | 0 | 0 | 0 |
| 5 — Dynamic analysis | 3 | 0 | 0 | 0 |
| 6 — Fuzzing and properties | 3 | 0 | 1 | 0 |
| 7 — Formal verification | 0 | 0 | 0 | 1 |
| 8 — Build and binary | 0 | 0 | 0 | 2 |
| 9 — Runtime privilege | 0 | 0 | 0 | 1 |
| 10 — Cryptography | 0 | 0 | 0 | 3 |
| 11 — CI/CD, release, and operations | 4 | 0 | 1 | 0 |
| 12 — Compliance controls | 0 | 0 | 0 | 14 |
| **Total** | **31** | **0** | **2** | **22** |

**Architect** — [Tim Almond](https://github.com/Ttimmahlax) — accountable for this unit's security design; rendered
<!-- HARDENING-TABLE:END -->

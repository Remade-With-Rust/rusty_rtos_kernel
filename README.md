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
  them silicon. On the two emulators it is 24 of 25 today, with the one
  exception named and numbered under Conformance. Separately, **26 unmodified
  C demo files** from the FreeRTOS distribution link against it through
  [`rusty_rtos-capi`](https://github.com/Remade-With-Rust/rusty_rtos-capi) and
  pass their own checkers.
- **The switch is not here.** A context switch is a stack swap and a stack swap
  is `unsafe`, which this family allows only in `rusty_rtos_port-<arch>`. This
  crate decides *which* task runs and fires the same `TASK_SWITCHED_OUT` /
  `_IN` pair the C kernel fires; a port acts on it.

**Known gaps.** No SMP (`configNUMBER_OF_CORES > 1`), no MPU, no static
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
| on each emulator, Cortex-M3 and RV32 | **24 of 25** — see below |
| architectures | host, ARMv7-M, RV32, Xtensa LX7 |
| soak, both emulators | RV32 18/18 in 58 min · M3 18/18 in 75 min |
| on silicon (XIAO ESP32-S3) | 18/18, measured 2026-09-11 |
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

**Also open, and found by the 0.2.0 release gate:** `StreamBufferDemo` passes
on the host — identical to the C kernel — and **diverges on both emulators**,
identically. Every counter and the line count match (2,000 ticks, 2,424
yields, 28,002 exits, 20,927 lines); only the trace *text* differs, by 194
bytes. A scheduling defect moves a counter, and none moved. It looks like a
value whose digit count depends on `size_of::<usize>()` reaching the trace,
which is a 32-bit-target text defect rather than a kernel one — but it is
named here with its numbers rather than left out.

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
| tick, empty delayed list | 15 | 56 | 3.73× |
| tick, one task delayed | 15 | 56 | 3.73× |
| scheduler selection | 27 | 79 | 2.93× |

**Two of those are against us and are published as findings, not caveats.**
But a switch is selection *plus* the register file, and the two kernels put
their weight in opposite halves — so quoting the selection row alone is
quoting a third of the answer:

| whole switch, rv32 | FreeRTOS | Kairos | |
|---|---:|---:|---:|
| cooperative | 27 + 83 = 110 | 79 + 30 = **109** | **parity** |
| preemptive | 27 + 83 = 110 | 79 + 74 = **153** | 1.39× against us |

Gated twice: identical work-parity anchors — with the tick count read back, so
FreeRTOS's `uxSchedulerSuspended` early-out cannot pass as a tick — and a
**poison** build of every arm that makes the measured call twice per bracket
and requires every row to move.

### On silicon

XIAO ESP32-S3, Xtensa `ccount` at one cycle of resolution, median of 512 with
the instrument's own tax measured and subtracted:

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
bench/switch-cost/run.sh     # the register half
```

## Portability

| target | corpus | notes |
|---|---|---|
| host (x86-64 Windows, Linux) | ✅ **22 identical to the C kernel** | the sim port, and a threaded host port |
| `thumbv7m-none-eabi` (Cortex-M3) | ✅ 18/18 | QEMU `mps2-an385` |
| `riscv32imac-unknown-none-elf` | ✅ 18/18 | QEMU `virt` |
| `xtensa-esp32s3-none-elf` | ✅ 18/18 | **on silicon**, a XIAO ESP32-S3 |

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

**Tier** critical-path · **Audited** 2026-09-21 (survey) · **v1.0.0 gates** 13/16 · [Full checklist](docs/plans/use-protection-please.md)

`███████████░░░░░░░░░` **58%** &nbsp;·&nbsp; 21 Completed · 0 Scheduled · 15 Incomplete · 19 N/A

| Phase | ✅ Completed | 🗓 Scheduled | ⬜ Incomplete | · N/A |
|---|--:|--:|--:|--:|
| 0 — Threat modeling | 1 | 0 | 1 | 0 |
| 1 — Toolchain | 2 | 0 | 2 | 0 |
| 2 — Supply chain | 7 | 0 | 1 | 0 |
| 3 — Code level | 7 | 0 | 0 | 0 |
| 4 — Static analysis | 0 | 0 | 1 | 0 |
| 5 — Dynamic analysis | 1 | 0 | 2 | 0 |
| 6 — Fuzzing and properties | 1 | 0 | 3 | 0 |
| 7 — Formal verification | 0 | 0 | 1 | 0 |
| 8 — Build and binary | 0 | 0 | 1 | 1 |
| 9 — Runtime privilege | 0 | 0 | 0 | 1 |
| 10 — Cryptography | 0 | 0 | 0 | 3 |
| 11 — CI/CD, release, and operations | 2 | 0 | 3 | 0 |
| 12 — Compliance controls | 0 | 0 | 0 | 14 |
| **Total** | **21** | **0** | **15** | **19** |

**Architect** — [Tim Almond](https://github.com/Ttimmahlax) — accountable for this unit's security design; rendered
<!-- HARDENING-TABLE:END -->

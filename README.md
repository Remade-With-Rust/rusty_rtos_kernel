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

- **Proven**: 19 conformance scenarios produce traces **byte-identical** to the
  C kernel's for 100,000 ticks each, on four architectures — and the same
  corpus runs on silicon. Separately, **26 unmodified C demo files** from the
  FreeRTOS distribution link against it through
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
| scenarios byte-identical to the C kernel | **19** |
| ticks per scenario | 100,000 |
| architectures | host, ARMv7-M, RV32, Xtensa LX7 |
| soak, both emulators | RV32 18/18 in 58 min · M3 18/18 in 75 min |
| on silicon (XIAO ESP32-S3) | **18/18 byte-identical** |
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

| RV32, preemptive switch | cycles | vs C |
|---|---:|---:|
| FreeRTOS | 83 | 1.00× |
| Kairos | **74** | **1.12× cheaper** |

A cooperative Kairos switch on RV32 measures 2.77× cheaper, and on its own
that reads as a win. **It is not one**, and the ARM control above is what shows
that: the RISC-V gap is about *where a yield is taken* — FreeRTOS's
`portYIELD()` is `ecall`, which takes the interrupt trap and must save
everything an interrupt could have clobbered — not about one kernel moving
registers more cheaply.

On a real part, a full scheduling round (queue send, queue receive, two context
switches) costs **8,313 ns / 1,995 cycles** on a XIAO ESP32-S3, which is 88 ppm
of the P-256 signature it was measured beside.

```sh
bench/switch-cost/run.sh     # from the Kairos umbrella
```

## Portability

| target | corpus | notes |
|---|---|---|
| host (x86-64 Windows, Linux) | ✅ 19/19 | the sim port, and a threaded host port |
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
itself, and [`rusty_rtos_mqtt`](https://github.com/Remade-With-Rust/rusty_rtos_mqtt) is the way out of it.
Paired with the **MATA distributed cloud**, robotics and sensor data has two
routes — read it on the machine, or reach it through the cloud — with the same
memory-safe crates at both ends.

The family:
[`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core) (the shared vocabulary),
[`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel) (the scheduler),
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port) (the architecture seam),
[`rusty_rtos_heap`](https://crates.io/crates/rusty_rtos_heap) (the allocators),
[`rusty_rtos_json`](https://github.com/Remade-With-Rust/rusty_rtos_json) (coreJSON),
[`rusty_rtos_sntp`](https://github.com/Remade-With-Rust/rusty_rtos_sntp) (coreSNTP),
[`rusty_rtos_mqtt`](https://github.com/Remade-With-Rust/rusty_rtos_mqtt) (coreMQTT),
[`rusty_rtos_backoff`](https://github.com/Remade-With-Rust/rusty_rtos_backoff) (backoffAlgorithm),
[`rusty_rtos-capi`](https://github.com/Remade-With-Rust/rusty_rtos-capi) (the C ABI) and
[`rusty_rtos_demo`](https://github.com/Remade-With-Rust/rusty_rtos_demo) (the conformance corpus).
The last six are on GitHub and not yet on crates.io. Also check out
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

<!-- HARDENING-TABLE:BEGIN generated by use-protection-please — edit docs/plans/use-protection-please.md, not this block -->
## Hardening status

**Tier** critical-path · **Audited** 2026-09-16 (v0.1.0 release pass) · **v1.0.0 gates** 10/17 · [Full checklist](https://github.com/Remade-With-Rust/rusty_rtos_kernel/blob/main/docs/plans/use-protection-please.md)

`██████████░░░░░░░░░░` **50%** &nbsp;·&nbsp; 18 Completed · 0 Scheduled · 18 Incomplete · 19 N/A

| Phase | ✅ Completed | 🗓 Scheduled | ⬜ Incomplete | · N/A |
|---|--:|--:|--:|--:|
| 0 — Threat modeling | 0 | 0 | 2 | 0 |
| 1 — Toolchain | 2 | 0 | 2 | 0 |
| 2 — Supply chain | 7 | 0 | 1 | 0 |
| 3 — Code level | 6 | 0 | 1 | 0 |
| 4 — Static analysis | 0 | 0 | 1 | 0 |
| 5 — Dynamic analysis | 1 | 0 | 2 | 0 |
| 6 — Fuzzing and properties | 1 | 0 | 3 | 0 |
| 7 — Formal verification | 0 | 0 | 1 | 0 |
| 8 — Build and binary | 0 | 0 | 1 | 1 |
| 9 — Runtime privilege | 0 | 0 | 0 | 1 |
| 10 — Cryptography | 0 | 0 | 0 | 3 |
| 11 — CI/CD, release, and operations | 1 | 0 | 4 | 0 |
| 12 — Compliance controls | 0 | 0 | 0 | 14 |
| **Total** | **18** | **0** | **18** | **19** |

Gates waived for 0.x are listed with their reasons in the plan's "v0.1.0 release decision" section — an Incomplete gate not listed there is an omission, not a decision.

**Architect** — [Tim Almond](https://github.com/Ttimmahlax) — accountable for this unit's security design; rendered
<!-- HARDENING-TABLE:END -->

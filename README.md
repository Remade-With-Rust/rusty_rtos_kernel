# rusty_rtos_kernel

[![crates.io](https://img.shields.io/crates/v/rusty_rtos_kernel.svg)](https://crates.io/crates/rusty_rtos_kernel)
[![docs.rs](https://docs.rs/rusty_rtos_kernel/badge.svg)](https://docs.rs/rusty_rtos_kernel)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

FreeRTOS-Kernel remade in Rust: the fixed-priority preemptive scheduler, task notifications, queues, semaphores, mutexes with priority inheritance, queue sets, software timers, event groups, stream and message buffers — a pure state machine over the Port seam, forbid(unsafe), traced against the C kernel.

Part of **Kairos**, the Remade-With-Rust programme that rebuilds the FreeRTOS
portfolio in memory-safe Rust, as independent packages that expose the API a
FreeRTOS developer already knows and prove every scheduling decision against
the C kernel's own trace.

- This package's plan: [docs/plans/rusty_rtos_kernel.md](docs/plans/rusty_rtos_kernel.md)
- Every number: [docs/LEDGER.md](docs/LEDGER.md)
- The family plan: Kairos `docs/plans/rtos-mission.md` (umbrella repo)

**Claims discipline:** this README makes no performance or capability claim that
is not backed by a test, a benchmark ledger entry, or a kill test recorded in the
plan. "Scaffold" means scaffold. "Sim only" means the sim port; "builds, not
flashed" means no chip has run it.

## Status

**K2 in progress — 13 of its 18 scenarios agree; K1 passed.**

Since K1 the kernel has grown the whole `FromISR` surface (queue send,
receive, peek, overwrite, semaphore give, task notify), queue sets with
real `cTxLock` / `cRxLock` counts, task notifications, stream and message
buffers, the software timers with their daemon task, and event groups. The
conformance corpus is fifteen scenarios, all trace-identical to the C
kernel for 100,000 ticks each — 12,688,209 lines. Thirteen of them are on
K2's list; `IntQueue` is out of scope for a signal-driven host port. Four
remain — `QueueSet`, `StreamBufferDemo`, `MessageBufferDemo` and
`MessageBufferAMP` — and none of them needs new kernel: each is blocked on
the sim contract, on a stack-address PRNG seed, or on a second oracle
binary.

**K1 — the scheduler agrees with the C kernel. Passed.**

All nine scenarios of the conformance corpus produce traces **identical to
the C kernel's for 100,000 ticks each** — 8,408,764 lines, with the tick,
yield and critical-section-exit counters equal on both sides, every time.
Miri is green over the whole corpus. Nothing has run on a chip and nothing
here is timed.

```sh
kairos conform --all --ticks 100000      # from the Kairos umbrella
```

What that covers: tasks, the tick (pended-tick replay included), the
context-switch decision, the ready and delayed and suspended lists, delay,
delay-until, suspend, resume, priority set, suspend-all and resume-all,
abort-delay; queues at both ends with blocking sends and receives and peek,
binary and counting semaphores, mutexes with priority inheritance,
disinheritance and disinheritance-after-timeout, and the recursive variant.
Notifications, timers, event groups and stream buffers are K2.

One number did not come out where the plan hoped. The arena-and-list cost
row is **2.08×** the C list (46.45 against 22.32 instructions per list
operation, callgrind), where 1.25× was the line at which the mission plan
said to reopen "handles are indices, never pointers". The row, its method
and where the cost actually goes are in the umbrella's `docs/LEDGER.md`; the
decision is the owner's and nothing here assumes an answer.

**The kernel never performs a context switch.** A switch is a stack swap
and a stack swap is `unsafe`, which this family allows only in
`rusty_rtos_port-<arch>`. This crate decides *which* task should run and
fires the same `TASK_SWITCHED_OUT` / `_IN` pair the C kernel fires; a port
acts on that with its fenced assembly, and the sim's runner acts on it by
stepping the next task's body.

## What it is

- A pure-Rust remake of the corresponding FreeRTOS component. Same job, same
  names, same semantics, new code, permissive licence, `forbid(unsafe)` in
  the core.
- Arch-agnostic: the core crate is `no_std` (+ `alloc`) and knows nothing about
  a CPU, an allocator or an operating system. Ports and backends are thin,
  feature-gated WRAP crates.

## What it is not

- Not a fork of FreeRTOS and not a binding to it. The C kernel is the
  **oracle** this package is measured against, never a dependency.
- Not a rewrite of a radio blob, a ROM or a vendor driver. Where silicon must
  be touched, a port crate **wraps** `cortex-m-rt` / `riscv-rt` / `esp-hal`
  and says so.

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

## License

MIT OR Apache-2.0, at your option. FreeRTOS is MIT-licensed by Amazon.com,
Inc. or its affiliates; this crate remakes its API and behaviour from the
published sources and links no FreeRTOS code.

---

<!-- HARDENING-TABLE:BEGIN generated by use-protection-please — edit docs/plans/use-protection-please.md, not this block -->
## Hardening status

**Tier** critical-path · **Audited** 2026-09-09 (survey) · **v1.0.0 gates** 9/16 · [Full checklist](docs/plans/use-protection-please.md)

`████████░░░░░░░░░░░░` **42%** &nbsp;·&nbsp; 15 Completed · 0 Scheduled · 21 Incomplete · 19 N/A

| Phase | ✅ Completed | 🗓 Scheduled | ⬜ Incomplete | · N/A |
|---|--:|--:|--:|--:|
| 0 — Threat modeling | 0 | 0 | 2 | 0 |
| 1 — Toolchain | 2 | 0 | 2 | 0 |
| 2 — Supply chain | 4 | 0 | 4 | 0 |
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
| **Total** | **15** | **0** | **21** | **19** |

**Architect** — [Tim Almond](https://github.com/Ttimmahlax) — accountable for this unit's security design; rendered
<!-- HARDENING-TABLE:END -->

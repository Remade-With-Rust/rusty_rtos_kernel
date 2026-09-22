# `esp32c6-cycles` — K3's cycle rows on the part the clause names

> **STATUS: BUILDS, NEVER RUN.** No ESP32-C6 has been on this bench. This
> cell compiles for `riscv32imac-unknown-none-elf` and has never been
> flashed, so it carries **no numbers** and claims none. It exists so the
> clause waits on hardware alone.

K3 has said since it was written that the context-switch, tick and latency
cycle rows come **from a C6, not from QEMU**. This is that cell: the twin of
[`xiao-s3-cycles`](../xiao-s3-cycles), measuring the same four rows the same
way so the two parts read side by side.

```sh
cargo run --release      # an ESP32-C6 on a serial port. That is the whole procedure.
```

## Why a C6 and not the S3 already on the bench

Three reasons, and only the first is the plan's.

1. **A second architecture of record.** The S3 is Xtensa LX7; the C6 is RV32.
   A kernel that costs the same on both has a cost of its own rather than one
   chip's.
2. **`mcycle` is architectural** — a CSR in the RISC-V base spec, not optional
   debug hardware. On real silicon it is also a real clock, which under QEMU
   it is not: there `-icount` makes it a deterministic instruction count, and
   `bench/tick-work` says so rather than calling it a cycle row.
3. **It builds on stable.** No esp toolchain, no `build-std`. Every Xtensa
   cell in this family needs `cargo +esp`; this one does not, so CI could
   build it even though CI can never run it.

## What it will measure

The same four rows as the S3 cell, median of 512, with the instrument's own
bracket tax measured and subtracted, and min/max beside the median rather
than a mean — a chip's interrupts add time and never remove it, so the floor
and the middle say more than the average. A row that does not exceed the tax
is reported as *below resolution* rather than given a number.

| row | what it brackets |
|---|---|
| tick (nothing delayed) | `increment_tick` with an empty delayed list |
| tick (one task delayed) | the same with a non-empty one — the steady state of anything using `vTaskDelay` |
| switch | `switch_context`, the scheduler choosing |
| ISR-API wake | `queue_send_from_isr` through to the task holding the value |

## Two traps already avoided here

**The hand-rolled `NoTrace`.** This cell uses
`rusty_rtos_core::trace::NoTrace`, not a local one. A local
`struct NoTrace` inherits the trait default `WANTS_NAMES = true`, so every
traced event builds a 16-byte task name for a sink that discards it — the
defect that made the S3 twin report 131 / 623 / 949 cycles where the truth
was 54 / 166 / 430, and stood for ten days.

**`-nostartfiles`.** The Xtensa cells pass it; `rust-lld` rejects it on
RISC-V, where `riscv-rt` provides the startup. It is absent from this cell's
`.cargo/config.toml` deliberately.

## What it will NOT contain

- **A C arm**, which the clause also asks for. A C arm on a C6 needs ESP-IDF
  as a platform layer — but the oracle's own `portable/GCC/RISC-V` port is
  **first-party**, where its Xtensa port is ThirdParty, so this part is the
  better host for that arm than the S3.
- **A real interrupt's vector entry and exit.** The wake row calls
  `queue_send_from_isr` inline, so it is the kernel's share of a wake.
- **A register-file swap.** Kairos tasks are stackless; the register cost is
  `bench/switch-cost`'s row.

## When it first runs

Put the numbers in `docs/LEDGER.md` with a date, and replace the status
banner at the top of this file. Until then every row it prints is
unverified.

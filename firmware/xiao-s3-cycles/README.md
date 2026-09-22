# `xiao-s3-cycles` — K3's cycle rows, on silicon

```
clock   Xtensa ccount, 1 cycle of resolution at 240 MHz
method  median of 512, bracket tax measured and subtracted

  bracket tax            1 cycles (an empty ccount pair, subtracted below)

cycles per operation, on the part:
  tick (nothing delayed) median=54     min=54     max=1308
  tick (one task delayed) median=55     min=55     max=1949
  switch                 median=166    min=165    max=5841
  ISR-API wake -> task has it median=430    min=430    max=9614

  at 240 MHz, one cycle is 4.17 ns.
  tick   ~225 ns idle, ~229 ns with a task delayed
  switch ~691 ns
```

> **These numbers replaced a set measured through a harness defect on
> 2026-09-21** — tick was 131/129, switch 623, ISR wake 949. Nothing in the
> kernel changed. See *The defect* below.

Run it:

```sh
cargo run --release        # a XIAO ESP32-S3 on USB
```

## Why this cell, and why it is not QEMU

The mission plan says cycle rows come from a part, and it is right. DWT is
unimplemented on the Cortex-M cells; SysTick's deltas there are host wall time
and *shrink* as the work grows; RV32's `mcycle` is only reproducible under
`-icount`, which makes it a **work** counter rather than a clock. So
`bench/switch-cost` reports retired instructions and says so.

This is the other half. Xtensa's `ccount` is a real cycle counter on a real
240 MHz part with **one cycle of resolution**, where `esp_hal::time::Instant`
has one microsecond — 240 cycles, larger than every number in the table above.
That resolution gap is why the sibling `xiao-s3-signing` cell had to amortise
20,000 rounds to say anything, and why this one does not.

## The instrument measures itself first

Two `ccount` reads back to back cost **1 cycle**, measured the same way and
subtracted from every row. A row whose median does not exceed that tax is
reported as *below resolution* rather than given a number — an instrument that
cannot out-resolve itself has not earned a figure.

Medians with min and max beside them, never means: a chip's interrupts add
time and never remove it, so the floor and the middle say more than the
average. The `max` column is those interrupts — note the delayed-tick row's
max of 201 against the idle row's 3,901, which is the same work with fewer
interruptions landing in the window.

## The refuted check, which is the most interesting line here

The cell originally asserted `tick_delayed >= tick`, on the obvious reasoning
that a non-empty delayed list can only add work. **The board said otherwise:
129 cycles against 131.** The assumption was what was wrong, so the check was
replaced and the number kept.

**★ And on 2026-09-21 the finding itself was withdrawn.** With the harness
defect below removed, the ordering came back the obvious way round —
**55 against 54**, the delayed tick costing one cycle *more*. The original
2-cycle inversion was noise inside a much larger number that should not have
been there at all.

The paragraph that used to sit here explained the inversion confidently, in
terms of ready-list population dominating the delayed-list check. It was a
good story about an artefact. The cell now prints whichever sentence its own
numbers support, rather than carrying a conclusion in prose that the next run
can contradict.

## A cross-check against the sibling cell

`xiao-s3-signing` measured a scheduling round — queue send, queue receive, two
context switches — at **1,995 cycles**, by amortising 20,000 of them through a
1 µs clock. Against the old rows this cell predicted `2 × 623 + (949 − 623) ≈
1,570`: same order, about 20% apart, which read as agreement.

**That agreement was between two measurements of the same defect.** The
sibling cell hand-rolls the same shadowed `NoTrace`, so both were paying the
name lookup. Against the corrected rows the prediction is
`2 × 166 + (430 − 166) ≈ 596`, and until the sibling is re-run at its own
corrected numbers there is **no cross-check here** — only a prediction that
the sibling should fall by roughly the same factor this cell did.

That is the honest state of it. Two instruments agreeing is worth as much as
either alone *only* when they are independent, and a common-mode defect makes
them one instrument wearing two hats.

## What this does NOT claim

**No C arm ON THIS PART.** A C arm now exists as a **work** row on rv32 —
`bench/tick-work`, FreeRTOS V11.3.1 out of the pinned oracle, unmodified,
under `minstret` — so the clause's comparison is no longer entirely open. But
*cycles* here still have nothing beside them: building FreeRTOS for the S3
needs the ESP-IDF header tree and a generated `sdkconfig.h`, and its Xtensa
port's `#if`s key off `CONFIG_FREERTOS_*`, so a stubbed build would not be the
kernel anyone runs. These are our cycles on silicon; the cycle comparison is
not done.

## The defect

This cell hand-rolled its own `NoTrace` rather than using the one
`rusty_rtos_core::trace` ships. The crate's carries
`const WANTS_NAMES: bool = false`; the trait's default is `true`. So every
traced event built a 16-byte task name, validated it as UTF-8, and handed it
to a sink whose body is `{}`.

Deleting the twin — no kernel change of any kind — moved every row:

| row | through the defect | corrected | |
|---|---:|---:|---:|
| tick (idle) | 131 | **54** | 2.43x |
| tick (delayed) | 129 | **55** | 2.35x |
| switch | 623 | **166** | 3.75x |
| ISR-API wake | 949 | **430** | 2.21x |

Corroborated on a different architecture by a different instrument: the same
fix moved `riscv32-qemu-tick-work`'s selection row from **305 to 79 retired
instructions, 3.86x**, against this cell's **3.75x** in Xtensa cycles. Two
instruments that share no code agreeing on the size of the defect is what
makes it a defect rather than a story.

**Ten other sites in the repo still hand-roll the same twin** — see
`docs/LEDGER.md` for the survey.

**Not the full ISR-to-task latency the clause names.** No interrupt is taken:
`queue_send_from_isr` is the API an ISR would call, invoked inline. The row is
therefore the *kernel's share* of a wake and excludes the vector entry and exit
a real interrupt pays on either side of it. It is named "ISR-API wake" so that
it cannot be quoted as something it is not.

**No register-file swap.** Kairos tasks are stackless, so `switch` here is the
scheduler choosing and committing the next task, not saving registers. The
register cost is `bench/switch-cost`'s row and is counted there — 30
instructions on RV32, 19 on ARM.

**One part, one clock.** Nothing here transfers to the C6, which is a different
core at a different clock.

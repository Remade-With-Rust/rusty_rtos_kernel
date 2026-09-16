# `xiao-s3-cycles` — K3's cycle rows, on silicon

```
clock   Xtensa ccount, 1 cycle of resolution at 240 MHz
method  median of 512, bracket tax measured and subtracted

  bracket tax            1 cycles (an empty ccount pair, subtracted below)

cycles per operation, on the part:
  tick (nothing delayed) median=131    min=131    max=3901
  tick (one task delayed) median=129   min=129    max=201
  switch                 median=623    min=621    max=6994
  ISR-API wake -> task has it median=949   min=949    max=12388

  at 240 MHz, one cycle is 4.17 ns.
  tick   ~545 ns idle, ~537 ns with a task delayed
  switch ~2595 ns
```

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

The explanation is that blocking a task takes it *off the ready list*. With two
runnable tasks at one priority the tick has to make a time-slice round-robin
decision; with one, it does not. That saving is larger than the cost of looking
at a single delayed entry whose wake time is far away.

So the two figures are not "idle vs loaded" — they are "two ready tasks" vs
"one ready task and one delayed", and the ready-list population dominates.

## A cross-check against the sibling cell

`xiao-s3-signing` measured a scheduling round — queue send, queue receive, two
context switches — at **1,995 cycles**, by amortising 20,000 of them through a
1 µs clock. The rows here predict roughly `2 × 623 + (949 − 623) ≈ 1,570` for
the same shape. Same order, about 20% apart, by two different instruments on
two different arrangements. That agreement is worth as much as either number
alone; a factor-of-several disagreement would have meant one of them was
measuring something else.

## What this does NOT claim

**There is no C arm, and that is the clause's other half still open.** K3 asks
for these rows *against the C demo*. Building FreeRTOS for the S3 needs the
ESP-IDF header tree and a generated `sdkconfig.h`, and its Xtensa port's `#if`s
key off `CONFIG_FREERTOS_*` — a stubbed build would not be the kernel anyone
runs, so the number would be of our own construction. These are our numbers on
silicon; the comparison is not done.

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

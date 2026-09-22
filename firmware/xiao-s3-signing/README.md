# `xiao-s3-signing` — what the Kairos kernel costs a real Janus workload

K5a's measurement clause, answered on silicon.

```
SIGN kernel_only rounds=20000 batch_us=74490 per_round_ns=3724 per_round_cycles=893
SIGN kernel_only switches=40000 per_round=2
SIGN kernel_share_of_one_signature = 39 ppm  (3724 ns of 94390000 ns)
SIGN work_parity=OK bare=100s/100v/100ok scheduled=100s/100v/100ok
```

**One full scheduling round — a queue send, a queue receive and two real
context switches — costs 3,724 ns (893 cycles at 240 MHz), which is 39
parts per million of a single P-256 signature.**

> **Re-measured 2026-09-21.** This cell used to report 8,313 ns / 1,995 cycles
> / 88 ppm. It hand-rolled its own `NoTrace` instead of the one
> `rusty_rtos_core::trace` ships, inheriting the trait default
> `WANTS_NAMES = true`, so every traced event built a 16-byte task name and
> UTF-8 validated it for a sink whose body is `{}`. **The round fell 2.23x
> with no kernel change**, and the sibling `xiao-s3-cycles` fell by 2.2-3.8x
> on the same one-line fix. The conclusion this cell exists to support is
> unchanged and strengthened: the kernel is noise against the workload.

## The workload is not ours

P-256 ECDSA over a fixed 32-byte prehash, from `p256 = "0.13"` (RustCrypto) —
the same crate and major version `rusty_esp_mid-core` signs with, on the same
part. That firmware's own ledger (M1, 2026-09-06) measured **95 ms per
signature and 151 ms per verification** on a `xiao-esp32s3-sense`. This cell
measures **94.3 ms and 149.4 ms** on the same part, which is the sanity check
that the workload really is the same one.

That comparison is **cross-binary and labelled as such** in the output: their
build pins esp-hal `=1.2.0` and ours `=1.2.1`, with different features and a
different link. It is a reference, never the result.

## Why the answer is a direct measurement and not a difference

The obvious experiment — time the workload with the kernel and without, and
subtract — does not work here, and the first version of this cell proved it
by producing a **scheduled arm faster than the bare one**. Two ~24.5-second
batches cannot resolve a few microseconds; `codec-measurement` §5 says never
to take a differential of two same-sized numbers, and this is why.

So the kernel is timed **on its own**: the identical send / yield / receive /
yield sequence the scheduled arm performs around every signature, with the
signature removed. A small number measured as a small number.

The bare-vs-scheduled arms are still run, ABBA-interleaved over four rounds,
but they are there for **work parity** — 100 signs, 100 verifications, 100
successful — not for the headline. Their batch delta is printed with an
explicit note that it is not the overhead figure.

## Three guards, and two of them fired

- **Work parity.** The first run reported the scheduled arm finishing 75×
  faster. It had completed **zero** operations: `TIMER_TASK_PRIORITY` was 3,
  above the workers at 2, and nothing steps the daemon's body so it never
  blocked and starved them. `work_parity=VIOLATED` named it immediately.
- **Symmetric timed regions.** The bare arm signed once *inside* its batch
  window and the scheduled arm did not — 101 signatures timed against 100,
  about 94 ms of pure asymmetry, enough to make the arm doing more work look
  faster. The warm-up signature is now outside both windows.
- **Switch counting.** A yield that returns to the same task is cheaper than
  a switch, so a loop that never changed task would report a small number and
  look like a good result. The cell counts switches and asserts two per
  round: `switches=40000 per_round=2` over 20,000 rounds.

## Clock resolution

`esp_hal::time::Instant` resolves to one microsecond and a scheduling round
is far below that, so timing rounds individually would print a column of
zeroes and call it proof. The batch of 20,000 rounds lands at 166 ms, five
orders of magnitude above the quantum, and is divided. The method line prints
the resolution so a reader can check.

## What it does NOT claim

It is **not** `xiao-s3-keys` itself running on a Kairos kernel — that joining
act lives in a Janus repository and is the owner's. This is the same workload,
same crate, same part, measured on our side of the fence.

It uses a **fixed key** rather than the chip's TRNG, deliberately: a fixed key
makes both arms bit-identical in work, the cost under measurement is curve
arithmetic which does not depend on the scalar, and this cell therefore never
touches the identity partition.

It claims nothing about `esp-radio`, Wi-Fi or the `esp-radio-rtos-driver`
joint. Those are K5b.

## Running it

Needs the `esp` toolchain and a board on a serial port, so it is never started
by a gate.

```sh
cargo +esp run --release
```

# rusty_rtos_kernel — the ledger

Every number this package claims, with the run that produced it. A row
without a method is not a number. Counters before clocks; an external oracle
before a self-metric; the method line names the machine, the pinning, the arm
order and the null-arm floor for anything timed.

## Conformance against the C kernel (2026-09-09, K1)

| gate | result | method |
|---|---|---|
| `dynamic`, 100,000 ticks | **1,219,231 trace lines identical** to the C kernel's, verdict line included | `kairos conform dynamic --ticks 100000` from the umbrella: runs this kernel through `rusty_rtos_demo`'s sim and the instrumented C kernel (FreeRTOS-Kernel V11.3.1 @ `3a22924e`, Posix port under the sim-contract-v1 patch), and compares line for line, failing at the first difference |
| counters at 100,000 ticks | ticks 100000, yields 179588, exits 1066689, lines 1219230 — equal on both sides | the harness's `KAIROS_RESULT` line, from the patched C port's counters and from `SimPort`'s |
| `dynamic`, 2000 ticks | 24,403 lines identical | `kairos conform dynamic`; pinned as a regression in `rusty_rtos_demo/crates/rusty_rtos_demo-core/tests/conformance.rs` (counters + an FNV-1a/64 digest of the trace), so drift fails without a C toolchain |
| scenarios covered | 1 of 9 | `dynamic`; the rest land with their ports |

An outermost critical-section exit is sim time (`ORACLES.md`, contract v1),
so agreeing on the trace means agreeing on *when* a decision was made, not
only on what it was.

## The build fact (2026-09-09)

| gate | result | method |
|---|---|---|
| `cargo test --workspace` | 4 tests pass | the `Name` truncation tests; the scheduler's own tests are the conformance run above, which needs the corpus and therefore lives in `rusty_rtos_demo` |
| `cargo check -p rusty_rtos_kernel-core --no-default-features` and `--features alloc` on `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`, `riscv32imac-unknown-none-elf`, `riscv32imafc-unknown-none-elf` | all 8 rungs pass | `kairos check rusty_rtos_kernel --fmt --clippy --test --deny`, exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean | same run, under the workspace lint policy: `unsafe_code` denied, `unwrap`/`expect`/`panic`/`todo`/`unimplemented` denied, `indexing_slicing` + `arithmetic_side_effects` warned under `-D warnings` |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok | same run |
| Miri | green | `cargo +nightly miri test --lib`, miri 0.1.0 of 2026-09-08 |

No speed number, no size number: nothing here has been timed or sized, and
nothing has run on a chip. The arena-and-list cost row the family plan asks
for needs a measurement arm that does not exist yet; it lands with the first
silicon (K3).

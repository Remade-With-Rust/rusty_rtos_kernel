# rusty_rtos_kernel — the ledger

Every number this package claims, with the run that produced it. A row
without a method is not a number. Counters before clocks; an external oracle
before a self-metric; the method line names the machine, the pinning, the arm
order and the null-arm floor for anything timed.

## Conformance against the C kernel (2026-09-09, K1)

| scenario | lines identical at 100,000 ticks | ticks | yields | exits |
|---|---|---|---|---|
| `dynamic` | 1,219,231 | 100000 | 179588 | 1066689 |
| `PollQ` | 117,417 | 100051 | 2056 | 105608 |
| `BlockQ` | 1,344,460 | 100011 | 195466 | 1282272 |
| `semtest` | 1,503,634 | 100000 | 64837 | 1163649 |
| `countsem` | 966,326 | 100000 | 21001 | 1280002 |
| `recmutex` | 1,385,862 | 100000 | 40923 | 1068657 |
| `blocktim` | 131,384 | 100062 | 4505 | 114313 |
| `QPeek` | 486,871 | 100000 | 88964 | 414030 |
| `GenQTest` | 1,253,579 | 100000 | 150435 | 1299809 |
| **all nine** | **8,408,764** | equal on both sides | equal | equal |

| gate | result | method |
|---|---|---|
| scenarios covered | **9 of 9** | the K1 corpus complete |
| the gate | `kairos conform --all --ticks 100000` from the umbrella | runs this kernel through `rusty_rtos_demo`'s sim and the instrumented C kernel (FreeRTOS-Kernel V11.3.1 @ `3a22924e`, Posix port under the sim-contract-v1 patch), and compares line for line, failing at the first difference. The verdict line is compared with the rest |
| counters | ticks, yields, exits and line counts equal on both sides, every scenario | the harness's `KAIROS_RESULT` line, from the patched C port's counters and from `SimPort`'s |
| the same nine at 2000 ticks | identical | `kairos conform --all`; pinned as a regression in `rusty_rtos_demo/crates/rusty_rtos_demo-core/tests/conformance.rs` (counters, byte count and an FNV-1a/64 digest of the C kernel's own trace file), so drift fails without a C toolchain |

An outermost critical-section exit is sim time (`ORACLES.md`, contract v1),
so agreeing on the trace means agreeing on *when* a decision was made, not
only on what it was.

## Conformance against the C kernel (2026-09-09, K2)

| gate | result | method |
|---|---|---|
| scenarios covered | **16** through `rusty_rtos_demo`, 12,808,722 lines identical at 100,000 ticks | `kairos conform --all --ticks 100000` |
| subsystems added since K1 | the whole `FromISR` surface, queue sets with real `cTxLock`/`cRxLock` counts, task notifications, stream and message buffers over a byte arena with a free list, software timers with the daemon task, event groups, and `sbSEND_COMPLETED` as a hook an application can replace | each proved by the scenario that exercises it, not by a unit test |
| no-panic gate | **passed** | `tests/no_panic.rs`: 64 kernels, 4,000 arbitrary calls each over the whole public surface, with handles from other arenas, handles from nowhere and stale handles. It asserts the calls landed — the run must trace more than 100,000 lines |

## The build fact (2026-09-09)

| gate | result | method |
|---|---|---|
| `cargo test --workspace` | 4 tests pass | the `Name` truncation tests; the scheduler's own tests are the conformance run above, which needs the corpus and therefore lives in `rusty_rtos_demo` |
| `cargo check -p rusty_rtos_kernel-core --no-default-features` and `--features alloc` on `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`, `riscv32imac-unknown-none-elf`, `riscv32imafc-unknown-none-elf` | all 8 rungs pass | `kairos check rusty_rtos_kernel --fmt --clippy --test --deny`, exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean | same run, under the workspace lint policy: `unsafe_code` denied, `unwrap`/`expect`/`panic`/`todo`/`unimplemented` denied, `indexing_slicing` + `arithmetic_side_effects` warned under `-D warnings` |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok | same run |
| Miri | green | `cargo +nightly miri test --workspace`, miri 0.1.0 of 2026-09-08. The scheduler itself is put through the interpreter by `rusty_rtos_demo`'s corpus run, which covers all nine scenarios |

No speed number, no size number: nothing here has been timed or sized, and
nothing has run on a chip. The arena-and-list cost row the family plan asks
for needs a measurement arm that does not exist yet; it lands with the first
silicon (K3).

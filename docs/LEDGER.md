# rusty_rtos_kernel — the ledger

Every number this package claims, with the run that produced it. A row
without a method is not a number. Counters before clocks; an external oracle
before a self-metric; the method line names the machine, the pinning, the arm
order and the null-arm floor for anything timed.

## The build fact (2026-09-09)

| gate | result |
|---|---|
| `cargo check --workspace` on the host | passes at scaffold |
| `cargo check -p rusty_rtos_kernel-core --no-default-features` and `--features alloc` on `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`, `riscv32imac-unknown-none-elf`, `riscv32imafc-unknown-none-elf` | passes at scaffold (`kairos check`) |
| `cargo clippy --workspace --all-targets -- -D warnings` under the workspace lint policy | clean at scaffold |
| `cargo deny check` | see the hardening plan's H-08 row |

No speed number, no size number: nothing here has been measured. Nothing has
run on a chip.

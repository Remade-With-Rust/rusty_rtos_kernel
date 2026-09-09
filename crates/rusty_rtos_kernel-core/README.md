# rusty_rtos_kernel-core

The pure `no_std` core of [`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel):
types, traits and algorithms with no CPU, no allocator and no operating system.
`forbid(unsafe)`. Tests run on the host; the crate compiles for Cortex-M and
RISC-V bare metal with `--no-default-features`.

Feature ladder: `std` ⊃ `alloc` ⊃ core-only.

Part of Kairos (Remade With Rust). Plan: `docs/plans/rusty_rtos_kernel.md` in the repo.

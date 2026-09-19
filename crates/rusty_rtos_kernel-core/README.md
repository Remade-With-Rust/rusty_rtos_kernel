# rusty_rtos_kernel-core

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![crates.io](https://img.shields.io/crates/v/rusty_rtos_kernel-core.svg)](https://crates.io/crates/rusty_rtos_kernel-core)
[![docs.rs](https://docs.rs/rusty_rtos_kernel-core/badge.svg)](https://docs.rs/rusty_rtos_kernel-core)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

The pure `no_std` core of
[`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel): the whole
FreeRTOS scheduler as types, traits and algorithms, with **no CPU, no allocator
and no operating system**. `#![forbid(unsafe_code)]`.

- **What is in here**: tasks and the tick, the context-switch *decision*,
  ready/delayed/suspended lists, queues, semaphores, mutexes with priority
  inheritance, task notifications, software timers and their daemon, event
  groups, and stream and message buffers.
- **What is not**: the switch itself. A stack swap is `unsafe`, which this
  family allows only in `rusty_rtos_port-<arch>`. This crate fires the same
  `TASK_SWITCHED_OUT` / `_IN` pair the C kernel fires and a port acts on it.

**Known gaps.** No SMP, no MPU, no static allocation, no co-routines.

## Conformance

**19 conformance scenarios produce traces byte-identical to the C kernel's**
at 100,000 ticks each, on host, ARMv7-M, RV32 and Xtensa LX7 — the last on
silicon. Separately, 26 unmodified C demo files from the FreeRTOS distribution
run against this kernel through the C ABI.

```sh
kairos conform --all --ticks 100000      # from the Kairos umbrella
```

The full table, the open `AbortDelay` contract question and every defect the
corpus found are in the
[repository README](https://github.com/Remade-With-Rust/rusty_rtos_kernel#conformance).

## Using it

```rust
use rusty_rtos_kernel_core::{system, Kernel};

// The geometry is the declaration: no allocator, and the `.bss` cost is
// exactly what was asked for.
system! {
    mod app use MyConfig;
    tasks { producer: 1, consumer: 2 }
    queues { work: u32; 8 }
}
```

Feature ladder: `std` ⊃ `alloc` ⊃ core-only. Build with
`--no-default-features` for bare metal.

## Performance

| Cortex-M3 `PendSV` | instructions |
|---|---:|
| FreeRTOS | 19 |
| Kairos | **19** — exact parity |

The list this kernel walks costs **34.22 instructions per operation, 1.533×**
`list.c`, by callgrind with both arms printing the same checksum. Full method
in the [repository README](https://github.com/Remade-With-Rust/rusty_rtos_kernel#performance).

## Portability

Builds and tests on host; compiles for `thumbv7m-none-eabi`,
`riscv32imac-unknown-none-elf` and `xtensa-esp32s3-none-elf` with
`--no-default-features`. The corpus runs on all four.

## Part of Remade With Rust

This crate is part of **[Kairos](https://github.com/Remade-With-Rust/kairos)** — FreeRTOS remade in memory-safe
Rust, as independent packages that expose the API a FreeRTOS developer already
knows and prove every scheduling decision against the C kernel's own trace.

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

MIT OR Apache-2.0, at your option. FreeRTOS is MIT-licensed by Amazon.com, Inc.
or its affiliates; this crate remakes its API and behaviour from the published
sources and links no FreeRTOS code.

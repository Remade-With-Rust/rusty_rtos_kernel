# firmware/

Per-chip example projects for `rusty_rtos_kernel`. Each directory here is a **separate
cargo project**, excluded from the workspace, because every chip needs its own
target triple, linker script and (for Xtensa parts) its own toolchain. n0's
iroh-on-ESP32 work and the Janus family both reached the same conclusion: keep
the firmware projects out of the library workspace so architecture-specific
patches never leak into it.

Naming: `<board>-<demo>/`, for example `lm3s6965-qemu-flash/` or
`esp32c6-devkitc-blink/`.

| Chip class | Runtime | Target |
|---|---|---|
| Cortex-M3 (QEMU `lm3s6965evb`) | `cortex-m-rt` + `rusty_rtos_port-cortex-m` | `thumbv7m-none-eabi` |
| Cortex-M4F / M7 | same | `thumbv7em-none-eabihf` |
| Cortex-M33 | same | `thumbv8m.main-none-eabihf` |
| RISC-V RV32 (QEMU `virt`) | `riscv-rt` + `rusty_rtos_port-riscv` | `riscv32imac-unknown-none-elf` |
| ESP32-C6 / P4 | `esp-hal` + `rusty_rtos_port-riscv` | `riscv32imac-unknown-none-elf` / `riscv32imafc-unknown-none-elf` |
| ESP32 / ESP32-S3 | `esp-hal` (esp toolchain) + `rusty_rtos_port-xtensa` | `xtensa-esp32-none-elf` / `xtensa-esp32s3-none-elf` |

## The cells

| cell | what it claims | needs |
|---|---|---|
| [`xiao-s3-signing`](xiao-s3-signing) | **what the kernel costs a real Janus workload**: a scheduling round is **3,724 ns / 893 cycles, 39 ppm** of a P-256 signature (K5a). **Re-measured 2026-09-21**: 8,313 ns / 1,995 / 88 ppm until the shadowed `NoTrace` was found | a XIAO ESP32-S3 on a serial port, and the `esp` toolchain. Never started by a gate: its runner is `espflash` |
| [`xiao-s3-cycles`](xiao-s3-cycles) | **K3's cycle rows on silicon**: a tick is **54** cycles, a switch **166**, an ISR wake **430** -- `ccount` at one cycle of resolution, with the instrument's own tax measured and subtracted. **Re-measured 2026-09-21**: the figures here were 131 / 623 / 949 until a hand-rolled `NoTrace` shadowing the crate's own was found, which made every traced event build a task name for a sink that drops it. No C arm on this part: cycles here still have nothing beside them, blocked on ESP-IDF | a XIAO ESP32-S3 on a serial port, and the `esp` toolchain. Never started by a gate: its runner is `espflash` |
| [`riscv32-qemu-tick-work`](riscv32-qemu-tick-work) | **K3's tick and switch rows AGAINST THE C**, in retired instructions: tick 15 (C) against 56, selection 27 against 79, and a whole cooperative switch 110 against 109 -- parity -- once `bench/switch-cost`'s register half is added. The C arm is FreeRTOS V11.3.1 out of the pinned oracle, unmodified. Driven by `bench/tick-work/run.sh`, which gates on work-parity anchors AND a poison build | `qemu-system-riscv32`, plus `clang` and `ld.lld` for the C arm. Runs headless, gates on an exit code |

Rules:

- Depend on this repo's crates by **path** (`../../crates/rusty_rtos_kernel`) inside a
  firmware example; depend on siblings by git URL as usual.
- Release profile for a chip: `opt-level = "s"` (or `"z"`), `lto = true`,
  `codegen-units = 1`, `panic = "abort"`, `overflow-checks = true`.
- A firmware example is not a test. The library's tests run on the host and
  on the sim port.

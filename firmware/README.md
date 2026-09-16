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
| [`xiao-s3-signing`](xiao-s3-signing) | **what the kernel costs a real Janus workload**: a scheduling round is 8,313 ns / 1,995 cycles, 88 ppm of a P-256 signature (K5a) | a XIAO ESP32-S3 on a serial port, and the `esp` toolchain. Never started by a gate: its runner is `espflash` |
| [`xiao-s3-cycles`](xiao-s3-cycles) | **K3's cycle rows on silicon**: a tick is 131 cycles, a switch 623, an ISR wake 949 — `ccount` at one cycle of resolution, with the instrument's own tax measured and subtracted. No C arm: that half of the clause is blocked on ESP-IDF | a XIAO ESP32-S3 on a serial port, and the `esp` toolchain. Never started by a gate: its runner is `espflash` |

Rules:

- Depend on this repo's crates by **path** (`../../crates/rusty_rtos_kernel`) inside a
  firmware example; depend on siblings by git URL as usual.
- Release profile for a chip: `opt-level = "s"` (or `"z"`), `lto = true`,
  `codegen-units = 1`, `panic = "abort"`, `overflow-checks = true`.
- A firmware example is not a test. The library's tests run on the host and
  on the sim port.

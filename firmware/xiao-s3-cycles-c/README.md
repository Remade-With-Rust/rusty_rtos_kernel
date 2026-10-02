# `xiao-s3-cycles-c`: the C arm of `xiao-s3-cycles`

**FreeRTOS as ESP-IDF ships it, on the same XIAO ESP32-S3, timed the same
way as the Kairos rows (2026-10-01).** It closes the K3 row that read "no C
arm ON THIS PART".

| cycles, median of 512 | Kairos (shipped `XtensaPort`) | FreeRTOS V10.5.1 (IDF default) | FreeRTOS V11.1.0 (upstream, IDF `FREERTOS_SMP`, 1 core) |
|---|---:|---:|---:|
| tick (nothing delayed) | **36** | 69 | 69 |
| tick (one task delayed) | **35** | 68 | 68 |
| switch (choose and commit) | 96 | **90** | 98 |
| ISR-API wake → task has it | **310** | 431 | 399 |

Kairos's own cell (`../xiao-s3-cycles`) on the stackless sim port reads
35 / 35 / 102 / 293. The column above uses `--features xtensa-port`, the
port Kairos actually ships on this part: real `rsil` critical sections and
switches committed by `Software0`. That is the like-for-like arm against a C
kernel paying its own critical sections.

## The same instrument, both sides

- **Clock:** `CCOUNT` at 240 MHz, one cycle of resolution. Each row is the
  median of 512 samples, with the bracket tax (an empty `CCOUNT` pair,
  1 cycle) measured and subtracted.
- **What is timed:** the kernel entry points called directly with interrupts
  masked, so the real tick and scheduler cannot run inside the bracket:
  - `xTaskIncrementTick()`;
  - `vTaskSwitchContext()`;
  - `xQueueSendFromISR(); vTaskSwitchContext(); xQueueReceive(q, 0)`.

  These are exactly the Kairos rows (`increment_tick`, `switch_context`, the
  ISR-API sequence).
- **Task set mirrored:** the measuring task and one more ready task at
  priority 2, so time slicing sees a list of two. The delayed row puts a
  task on the DELAYED list (a finite `vTaskDelay`, not `portMAX_DELAY`); the
  cell checks it is blocked.

## Read with these caveats

- **Optimisation is not identical.** Kairos is `opt-level = 3` with fat LTO,
  so a kernel call can be inlined into the measuring loop. ESP-IDF builds
  this C at `-O2` without LTO, so each C call pays a real call and return.
  Some of Kairos's margin on the small rows may be call overhead, not kernel
  work. This has not been separated yet.
- **The IDF kernel is not vanilla FreeRTOS.** V10.5.1-IDF takes spinlocks in
  its critical sections and queues even when configured for one core. That
  is the kernel S3 users actually ship, so it is the honest arm. The V11.1.0
  column is the closer relative of the V11.3.1 Kairos is transcribed from.
- **Port-optimised task selection** (`NSAU`) is on in the IDF build,
  because that is the default. Kairos walks its ready lists.

## Build and run

```powershell
./idf.ps1 build                                   # V10.5.1, the IDF default
./idf.ps1 -B build-smp -D SDKCONFIG=build-smp/sdkconfig `
    -D "SDKCONFIG_DEFAULTS=sdkconfig.defaults;sdkconfig.smp.defaults" build   # V11.1.0
espflash flash -p COM4 --monitor build/xiao_s3_cycles_c.elf
```

`idf.ps1` runs `idf.py` without `export.ps1`. The ESP-IDF v5.5.1 on this box
has every tool a build needs, but its export script refuses to activate
while optional tools (openocd, ccache, dfu-util, idf-exe) are missing.

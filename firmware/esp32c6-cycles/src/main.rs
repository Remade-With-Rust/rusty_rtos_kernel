//! K3's cycle rows on an ESP32-C6: the clause's own named part.
//!
//! The mission plan has said since it was written that the context-switch,
//! tick and latency cycle rows come **from a C6, not from QEMU**. This is
//! that cell. It is the twin of `xiao-s3-cycles` and deliberately measures
//! the same four rows the same way, so the two parts can be read side by
//! side.
//!
//! # Why a C6 and not the S3 we already have
//!
//! Three reasons, and only the first is the plan's.
//!
//! 1. **It is a second architecture of record.** The S3 is Xtensa LX7; the
//!    C6 is RV32. A kernel that costs the same on both is a kernel whose
//!    cost is its own rather than one chip's.
//! 2. **`mcycle` is architectural.** On RISC-V the retired-cycle counter is
//!    a CSR in the base spec, not optional debug hardware — the same reason
//!    the QEMU cells can carry a *work* row where the Cortex-M cells cannot.
//!    On real silicon it is also a real clock, which under QEMU it is not:
//!    there `-icount` makes it a deterministic instruction count and the
//!    firmware README says so.
//! 3. **It builds on stable.** No esp toolchain, no `build-std`. Every
//!    Xtensa cell in this family needs `cargo +esp`; this one does not,
//!    which means CI could build it even though CI can never run it.
//!
//! # Status: BUILDS, NEVER RUN
//!
//! **No C6 has been on this bench.** This cell compiles for
//! `riscv32imac-unknown-none-elf` and has never been flashed, so it carries
//! no numbers and this file claims none. It exists so that the clause is
//! waiting on *hardware alone* rather than on hardware and then a day of
//! bring-up — when a C6 arrives, `cargo run --release` is the whole
//! procedure.
//!
//! Treat every row it prints as unverified until someone has run it and
//! written the numbers into `docs/LEDGER.md` with a date.
//!
//! # The method, which is the S3 cell's
//!
//! Median of 512 with the instrument's own tax measured and subtracted, and
//! min/max beside the median rather than a mean: a chip's interrupts add
//! time and never remove it, so the floor and the middle say more than the
//! average. A row whose median does not exceed the bracket tax is reported
//! as below resolution rather than given a number.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use esp_backtrace as _;
use esp_println::println;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::tick::Bits32;
// The crate's own no-op sink, NOT a hand-rolled one.
//
// A local `struct NoTrace` inherits the trait default `WANTS_NAMES = true`,
// so every traced event builds a 16-byte task name for a sink that drops it.
// That defect made this cell's S3 twin report 131 / 623 / 949 cycles where
// the truth was 54 / 166 / 430, and it stood for ten days. Do not re-roll it
// here.
use rusty_rtos_core::trace::NoTrace;
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::{list_slots_for, lists_for, Kernel};
use rusty_rtos_port_core::sim::SimPort;
use rusty_rtos_port_riscv::mcycle;

esp_bootloader_esp_idf::esp_app_desc!();

/// How many samples per row. The same 512 the S3 cell uses.
const SAMPLES: usize = 512;

/// Matched to `xiao-s3-cycles` field for field, so the two parts' rows
/// differ by the chip and nothing else.
#[derive(Debug, Clone, Copy, Default)]
pub struct CycleConfig;

impl Config for CycleConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 4;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 1;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 2;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
}

const TASKS: usize = 6;
const QUEUES: usize = 2;
const SLOTS: usize = 8;
const TIMERS: usize = 1;
const GROUPS: usize = 1;

type K = Kernel<
    CycleConfig,
    SimPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { list_slots_for(TASKS, TIMERS, lists_for(CycleConfig::MAX_PRIORITIES, QUEUES, GROUPS)) },
    { lists_for(CycleConfig::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    1,
    8,
    TIMERS,
    GROUPS,
>;

/// Median, min and max with the bracket tax already taken off each sample.
struct Row {
    median: u32,
    min: u32,
    max: u32,
    below_resolution: bool,
}

fn summarise(samples: &mut [u32], tax: u32) -> Row {
    samples.sort_unstable();
    let raw_median = samples[samples.len() / 2];
    let sub = |v: u32| v.saturating_sub(tax);
    Row {
        median: sub(raw_median),
        min: sub(samples[0]),
        max: sub(samples[samples.len() - 1]),
        below_resolution: raw_median <= tax,
    }
}

fn report(name: &str, row: &Row) {
    if row.below_resolution {
        println!("  {name:<28} below the instrument's own resolution");
    } else {
        println!(
            "  {:<28} median={:<7} min={:<7} max={}",
            name, row.median, row.min, row.max
        );
    }
}

fn stop(why: &str) -> ! {
    println!("RESULT: FAIL -- {why}");
    loop {
        core::hint::spin_loop();
    }
}

#[esp_hal::main]
fn main() -> ! {
    let _p = esp_hal::init(esp_hal::Config::default());

    println!();
    println!("=== the Kairos kernel's cycle rows, on ESP32-C6 SILICON ===");
    println!("clock   RISC-V mcycle, an architectural CSR");
    println!("method  median of {SAMPLES}, bracket tax measured and subtracted");
    println!();
    println!("NOTE: if this is the first run, the numbers below have never");
    println!("      been recorded. Put them in docs/LEDGER.md with a date.");
    println!();

    // ---------------------------------------------------- the instrument --
    // Two reads back to back. Whatever this costs is in every row below, so
    // it comes off every row below.
    let mut tax_samples = [0u32; SAMPLES];
    for slot in tax_samples.iter_mut() {
        let a = mcycle();
        let b = mcycle();
        *slot = b.wrapping_sub(a) as u32;
    }
    tax_samples.sort_unstable();
    let tax = tax_samples[SAMPLES / 2];
    println!("  bracket tax                  {tax} cycles (an empty mcycle pair)");
    println!();

    // ----------------------------------------------------------- set up --
    let mut kernel = match K::new(SimPort::new(), NoTrace) {
        Ok(k) => k,
        Err(_) => stop("the kernel geometry was refused"),
    };
    let queue = match kernel.queue_create(1) {
        Ok(q) => q,
        Err(_) => stop("the queue was refused"),
    };
    // Two tasks so the scheduler has a decision to make. A switch with one
    // ready task is not a switch.
    let waiter = match kernel.create_task("wait", 2) {
        Ok(t) => t,
        Err(_) => stop("task creation was refused"),
    };
    if kernel.create_task("other", 2).is_err() || kernel.start_scheduler().is_err() {
        stop("the scheduler would not start");
    }
    let _ = waiter;

    // ------------------------------------------------- a tick, idle list --
    let mut tick_samples = [0u32; SAMPLES];
    for slot in tick_samples.iter_mut() {
        let a = mcycle();
        let _ = kernel.increment_tick();
        let b = mcycle();
        *slot = b.wrapping_sub(a) as u32;
    }
    let tick = summarise(&mut tick_samples, tax);

    // ------------------------------------------------------- a switch ----
    let mut switch_samples = [0u32; SAMPLES];
    for slot in switch_samples.iter_mut() {
        let a = mcycle();
        kernel.switch_context();
        let b = mcycle();
        *slot = b.wrapping_sub(a) as u32;
    }
    let switch = summarise(&mut switch_samples, tax);

    // --------------------------------------- a tick with work to do ------
    // The row above is the tick's FLOOR: nothing was waiting on a delay, so
    // `increment_tick` had an empty delayed list to look at. That is not the
    // steady state of a system that uses `vTaskDelay`, and quoting it alone
    // would flatter the kernel.
    let delayed_ok = kernel.delay(1_000_000).is_ok();
    let mut tickd_samples = [0u32; SAMPLES];
    for slot in tickd_samples.iter_mut() {
        let a = mcycle();
        let _ = kernel.increment_tick();
        let b = mcycle();
        *slot = b.wrapping_sub(a) as u32;
    }
    let tick_delayed = summarise(&mut tickd_samples, tax);

    // ------------------------------------------------ an ISR-API wake ----
    // The bracket opens before the ISR-side give and closes when a task
    // holds the value. `queue_send_from_isr` is the API an ISR would call,
    // invoked INLINE: no interrupt is taken, so this is the kernel's share
    // of a wake and not the full ISR-to-task latency.
    let mut wake_samples = [0u32; SAMPLES];
    let mut wakes_reached = 0u32;
    for slot in wake_samples.iter_mut() {
        let a = mcycle();
        let sent = kernel.queue_send_from_isr(queue, 0x5A5A_5A5A);
        kernel.switch_context();
        let got = kernel.queue_receive(queue, 0);
        let b = mcycle();
        if sent.is_ok() && matches!(got, Ok(Wait::Ready(_))) {
            wakes_reached += 1;
        }
        *slot = b.wrapping_sub(a) as u32;
    }
    let wake = summarise(&mut wake_samples, tax);

    // ------------------------------------------------------- the rows ----
    println!("cycles per operation, on the part:");
    report("tick (nothing delayed)", &tick);
    report("tick (one task delayed)", &tick_delayed);
    report("switch", &switch);
    report("ISR-API wake -> task has it", &wake);
    println!();

    println!("what this row does NOT contain:");
    println!("  * a C arm. The clause asks for these AGAINST the C demo.");
    println!("    A C arm on a C6 needs ESP-IDF as a platform layer; the");
    println!("    oracle's own portable/GCC/RISC-V port is first-party, so");
    println!("    this part is the better host for that arm than the S3.");
    println!("  * the vector entry and exit of a REAL interrupt. The wake");
    println!("    row calls queue_send_from_isr inline.");
    println!("  * a register-file swap. Kairos tasks are stackless, so the");
    println!("    switch above is the scheduler moving `current`. The");
    println!("    register cost is bench/switch-cost's row.");
    println!();

    // --------------------------------------------------------- checks ----
    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            println!("      ok    {what}");
        } else {
            println!("      FAIL  {what}");
            failed += 1;
        }
    };
    check(tax > 0, "the bracket tax is measurable, so mcycle advances");
    check(!tick.below_resolution, "a tick costs more than the instrument");
    check(delayed_ok, "a task really was put on the delayed list");
    check(!switch.below_resolution, "a switch costs more than the instrument");
    check(
        wakes_reached == SAMPLES as u32,
        "every ISR wake reached the task",
    );

    println!();
    if failed == 0 {
        println!(
            "RESULT: PASS -- tick {}/{} cycles (idle/delayed), switch {}, ISR-API wake {}",
            tick.median, tick_delayed.median, switch.median, wake.median
        );
    } else {
        println!("RESULT: FAIL -- {failed} check(s) failed");
    }
    loop {
        core::hint::spin_loop();
    }
}

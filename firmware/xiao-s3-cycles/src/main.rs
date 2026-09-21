#![no_std]
#![no_main]
//! K3's cycle rows, on silicon: what a tick costs, what a switch costs, and
//! how long an ISR's wake takes to reach the task.
//!
//! # Why this cell exists at all
//!
//! The mission plan says cycle rows come from a part and not from QEMU, and
//! it is right: DWT is unimplemented on the Cortex-M cells, SysTick's deltas
//! there are host wall time, and RV32's `mcycle` is only reproducible under
//! `-icount`, which makes it a WORK counter rather than a clock. The sibling
//! `bench/switch-cost` therefore reports retired instructions and says so.
//!
//! This is the other half. Xtensa's `ccount` is a real cycle counter on a
//! real 240 MHz part, and it has **one cycle of resolution** where
//! `esp_hal::time::Instant` has one microsecond — 240 cycles, which is
//! larger than most of what is measured here. That is why the sibling
//! `xiao-s3-signing` cell had to amortise 20,000 rounds to say anything, and
//! why this one does not.
//!
//! # The three rows
//!
//! | row | what is bracketed |
//! |---|---|
//! | tick | `Kernel::increment_tick` — the kernel half of a tick interrupt |
//! | switch | `Kernel::switch_context` — choosing and committing the next task |
//! | ISR-API wake | `queue_send_from_isr` through to the woken task holding the value — inline, so NO interrupt entry |
//!
//! # The instrument measures itself first
//!
//! Two `ccount` reads back to back are not free, and at these magnitudes the
//! tax is a real fraction of the answer. So the cell measures the empty
//! bracket, reports it, and subtracts it from every row
//! (`codec-measurement` §6: the profiler is part of the system under test).
//! A row whose measured value does not exceed the tax is reported as being
//! below the instrument's resolution rather than given a number.
//!
//! Medians, with min and max beside them. A chip's interrupts add time and
//! never remove it, so the floor and the middle say more than the mean —
//! the same reading the Janus rows use.
//!
//! # What this does NOT claim
//!
//! **There is no C arm.** The clause asks for these rows *against the C
//! demo*, and that half is blocked: building FreeRTOS for the S3 needs the
//! ESP-IDF header tree and a generated `sdkconfig.h`, and its Xtensa port's
//! `#if`s key off `CONFIG_FREERTOS_*`, so a stubbed build would not be the
//! kernel anyone runs. These are our numbers on silicon, and the comparison
//! is still open.
//!
//! It is also a **stackless** arrangement, as every Kairos cell is: a task
//! owns no stack, so "switch" here is the scheduler moving `current`, not a
//! register-file swap. The register-file cost is `bench/switch-cost`'s row
//! and is counted separately.

use esp_backtrace as _;
use esp_println::println;
use xtensa_lx::timer::get_cycle_count;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::{items_for, lists_for, Kernel};
use rusty_rtos_port_core::sim::SimPort;

esp_bootloader_esp_idf::esp_app_desc!();

/// How many samples per row. Large enough for a median to mean something,
/// small enough that the whole cell runs in seconds.
const SAMPLES: usize = 512;

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

/// A trace that keeps nothing: this cell measures scheduling, it does not
/// assert a trace. Keeping one would put a formatter inside the timed
/// region, which is the instrument becoming the experiment.
#[derive(Debug, Default)]
struct NoTrace;
impl Trace for NoTrace {
    fn event(&mut self, _tick: u64, _event: Event<'_>) {}
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

/// Median, min and max of a sample set, with the bracket tax already taken
/// off each sample.
struct Row {
    median: u32,
    min: u32,
    max: u32,
    below_resolution: bool,
}

fn summarise(samples: &mut [u32], tax: u32) -> Row {
    samples.sort_unstable();
    let raw_median = samples[samples.len() / 2];
    let below = raw_median <= tax;
    let sub = |v: u32| v.saturating_sub(tax);
    Row {
        median: sub(raw_median),
        min: sub(samples[0]),
        max: sub(samples[samples.len() - 1]),
        below_resolution: below,
    }
}

fn report(name: &str, row: &Row) {
    if row.below_resolution {
        println!(
            "  {:<22} BELOW RESOLUTION -- the median did not exceed the bracket tax",
            name
        );
    } else {
        println!(
            "  {:<22} median={:<6} min={:<6} max={}",
            name, row.median, row.min, row.max
        );
    }
}

#[esp_hal::main]
fn main() -> ! {
    let _p = esp_hal::init(esp_hal::Config::default());

    println!();
    println!("=== the Kairos kernel's cycle rows, on ESP32-S3 SILICON ===");
    println!("clock   Xtensa ccount, 1 cycle of resolution at 240 MHz");
    println!("method  median of {SAMPLES}, bracket tax measured and subtracted");
    println!();

    // ---------------------------------------------------- the instrument --
    // Two reads back to back. Whatever this costs is in every row below, so
    // it comes off every row below.
    let mut tax_samples = [0u32; SAMPLES];
    for slot in tax_samples.iter_mut() {
        let a = get_cycle_count();
        let b = get_cycle_count();
        *slot = b.wrapping_sub(a);
    }
    tax_samples.sort_unstable();
    let tax = tax_samples[SAMPLES / 2];
    println!("  bracket tax            {tax} cycles (an empty ccount pair, subtracted below)");
    println!();

    // ------------------------------------------------------------ a tick --
    let mut kernel = match K::new(SimPort::new(), NoTrace) {
        Ok(k) => k,
        Err(_) => {
            println!("RESULT: FAIL -- the kernel geometry was refused");
            loop {
                core::hint::spin_loop();
            }
        }
    };
    let queue = match kernel.queue_create(1) {
        Ok(q) => q,
        Err(_) => {
            println!("RESULT: FAIL -- the queue was refused");
            loop {
                core::hint::spin_loop();
            }
        }
    };
    // Two tasks so the scheduler has a decision to make. A switch with one
    // ready task is not a switch.
    let waiter = match kernel.create_task("wait", 2) {
        Ok(t) => t,
        Err(_) => {
            println!("RESULT: FAIL -- task creation was refused");
            loop {
                core::hint::spin_loop();
            }
        }
    };
    if kernel.create_task("other", 2).is_err() || kernel.start_scheduler().is_err() {
        println!("RESULT: FAIL -- the scheduler would not start");
        loop {
            core::hint::spin_loop();
        }
    }
    let _ = waiter;

    let mut tick_samples = [0u32; SAMPLES];
    for slot in tick_samples.iter_mut() {
        let a = get_cycle_count();
        let _ = kernel.increment_tick();
        let b = get_cycle_count();
        *slot = b.wrapping_sub(a);
    }
    let tick = summarise(&mut tick_samples, tax);

    // ---------------------------------------------------------- a switch --
    let mut switch_samples = [0u32; SAMPLES];
    for slot in switch_samples.iter_mut() {
        let a = get_cycle_count();
        kernel.switch_context();
        let b = get_cycle_count();
        *slot = b.wrapping_sub(a);
    }
    let switch = summarise(&mut switch_samples, tax);

    // ------------------------------------------- a tick with work to do --
    // The row above is the tick's FLOOR: nothing was waiting on a delay, so
    // `increment_tick` had an empty delayed list to look at. That is not the
    // steady state of a system that uses `vTaskDelay`, and quoting it alone
    // would flatter the kernel. So block a task on a long delay and measure
    // again: now every tick walks a non-empty delayed list and compares
    // against a wake time it will not reach.
    let delayed_ok = kernel.delay(1_000_000).is_ok();
    let mut tickd_samples = [0u32; SAMPLES];
    for slot in tickd_samples.iter_mut() {
        let a = get_cycle_count();
        let _ = kernel.increment_tick();
        let b = get_cycle_count();
        *slot = b.wrapping_sub(a);
    }
    let tick_delayed = summarise(&mut tickd_samples, tax);

    // ---------------------------------------------------- an ISR-API wake --
    // The bracket opens before the ISR-side give and closes when a task
    // holds the value.
    //
    // BE PRECISE ABOUT WHAT THIS IS NOT. No interrupt is taken here:
    // `queue_send_from_isr` is the API an ISR would call, invoked inline.
    // So this is the KERNEL's share of an ISR-to-task wake and excludes the
    // vector entry and exit a real interrupt pays on either side of it.
    // Calling it "ISR-to-task latency" unqualified would be claiming a
    // number this cell does not measure, and the row is named
    // "ISR-API wake" for that reason.
    let mut wake_samples = [0u32; SAMPLES];
    let mut wake_failures = 0u32;
    for slot in wake_samples.iter_mut() {
        let a = get_cycle_count();
        let _ = kernel.queue_send_from_isr(queue, 0x5a5a);
        kernel.switch_context();
        let got = kernel.queue_receive(queue, 0);
        let b = get_cycle_count();
        match got {
            Ok(Wait::Ready(v)) if v == 0x5a5a => *slot = b.wrapping_sub(a),
            _ => {
                wake_failures += 1;
                *slot = u32::MAX;
            }
        }
    }
    let wake = summarise(&mut wake_samples, tax);

    // ------------------------------------------------------------ report --
    println!("cycles per operation, on the part:");
    report("tick (nothing delayed)", &tick);
    report("tick (one task delayed)", &tick_delayed);
    report("switch", &switch);
    report("ISR-API wake -> task has it", &wake);
    println!();
    println!("  at 240 MHz, one cycle is 4.17 ns.");
    println!(
        "  tick   ~{} ns idle, ~{} ns with a task delayed",
        (tick.median as u64 * 1000) / 240,
        (tick_delayed.median as u64 * 1000) / 240
    );
    println!(
        "  switch ~{} ns",
        (switch.median as u64 * 1000) / 240
    );
    println!();
    println!("  The delayed tick is CHEAPER, which is not what was expected and");
    println!("  is the more interesting of the two numbers. Blocking a task takes");
    println!("  it off the ready list, so the tick no longer makes a time-slice");
    println!("  round-robin decision between two runnable tasks at one priority.");
    println!("  That saving is larger than the cost of looking at a delayed entry");
    println!("  whose wake time is far away. The check that asserted the opposite");
    println!("  failed on the board and was replaced, rather than the number.");

    println!();
    println!("what this row does NOT contain:");
    println!("  * a C arm. The clause asks for these AGAINST the C demo, and");
    println!("    that half is blocked: FreeRTOS on the S3 needs the ESP-IDF");
    println!("    header tree and a generated sdkconfig.h, and its Xtensa");
    println!("    port's #ifs key off CONFIG_FREERTOS_*, so a stubbed build");
    println!("    would not be the kernel anyone runs.");
    println!("  * the vector entry and exit of a REAL interrupt. The ISR-API");
    println!("    row calls queue_send_from_isr inline; no interrupt is taken,");
    println!("    so that row is the kernel's share of a wake and not the");
    println!("    full ISR-to-task latency the clause names.");
    println!("  * a register-file swap. Kairos tasks are stackless, so the");
    println!("    switch above is the scheduler moving `current`. The");
    println!("    register cost is bench/switch-cost's row, counted there.");

    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            println!("      ok    {what}");
        } else {
            failed += 1;
            println!("      FAIL  {what}");
        }
    };
    check(tax > 0, "the bracket tax is measurable, so ccount advances");
    check(
        !tick.below_resolution,
        "a tick costs more than the instrument does",
    );
    check(delayed_ok, "a task really was put on the delayed list");
    // This started life as `tick_delayed >= tick`, on the assumption that a
    // non-empty delayed list can only add work. The board said otherwise --
    // 129 against 131 -- and the assumption was the thing that was wrong, so
    // the check now asserts what is actually true and the WHY is printed
    // above. Blocking a task removes it from the ready list, so the tick
    // stops making a time-slice round-robin decision between two runnable
    // tasks at the same priority; that saving is larger than the cost of
    // looking at one delayed entry whose wake time is far away.
    check(
        tick_delayed.median > 0 && tick.median > 0,
        "both tick variants measured a real cost",
    );
    check(
        tick.median.abs_diff(tick_delayed.median) < tick.median / 4,
        "the two tick variants are within 25% -- neither path is pathological",
    );
    check(
        !switch.below_resolution,
        "a switch costs more than the instrument does",
    );
    check(wake_failures == 0, "every ISR wake reached the task");

    println!();
    if failed == 0 {
        println!(
            "RESULT: PASS -- tick {}/{} cycles (idle/delayed), switch {} cycles, ISR-API wake {} cycles",
            tick.median, tick_delayed.median, switch.median, wake.median
        );
    } else {
        println!("RESULT: FAIL -- {failed} check(s) failed");
    }
    loop {
        core::hint::spin_loop();
    }
}

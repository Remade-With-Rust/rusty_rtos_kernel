//! K3's tick and switch WORK rows on rv32 — the Kairos arm.
//!
//! This is the opposite number of `bench/tick-work/c`, which runs FreeRTOS
//! V11.3.1 from the pinned `oracle/` checkout, unmodified, on this same
//! machine with the same instrument and the same sample discipline. Together
//! they are the "vs the C demo" half of K3's measurement clause, on the one
//! basis where the two kernels can honestly be compared.
//!
//! # Why retired instructions, and not cycles
//!
//! QEMU is not a clock. It was measured six ways on the Cortex-M cell and it
//! has no cycle counter worth the name. But `minstret` is an architectural
//! CSR, and under `-icount shift=0` it is exactly reproducible — the sibling
//! cell `riscv32-qemu-switch` read 199,902 three times running with it, and
//! swung 20% without it.
//!
//! So this is a **work** row, the same basis as `bench/switch-cost`, and it
//! sits beside those rows rather than pretending to be the silicon cycle
//! rows in `xiao-s3-cycles`. Cycles on a part remain a separate question.
//!
//! # Why a bracket, and not a profiler
//!
//! A host callgrind comparison of these two kernels was built and REFUTED on
//! 2026-09-21 (`docs/LEDGER.md`): FreeRTOS's `xTaskIncrementTick` self cost is
//! **2.7%** of its inclusive and ours is **13.1%**, so a self-cost ratio
//! measures how differently two codebases factor a tick, not what a tick
//! costs. Work parity was perfect and did not save it — the anchors prove the
//! arms did the same WORK, not that the boundary encloses the same THING.
//!
//! Two CSR reads around the call define the boundary explicitly. That is the
//! one change that makes the arms comparable, and it is why this row exists
//! at all.
//!
//! # What is matched, deliberately
//!
//! | | |
//! |---|---|
//! | config | 5 priorities, 16-byte names, 100 Hz tick, 10-deep timer queue — the shared `bench/kernel-ram/c/FreeRTOSConfig.h` |
//! | optimisation | `-O2`, no LTO, both arms — the C arm compiles per file, so cross-crate inlining here would measure the build |
//! | samples | 512, median, with the instrument's own tax measured and subtracted |
//! | machine | QEMU `virt`, rv32imac, `-icount shift=0` |
//! | ready tasks | two at the measured priority, so the scheduler has a decision — a switch with one ready task is not a switch |
//!
//! # What this does NOT claim
//!
//! Kairos is **stackless**: a task owns no stack, so `switch_context` here is
//! the scheduler moving `current`, exactly as `vTaskSwitchContext` moves
//! `pxCurrentTCB`. Neither row includes the register file. That half is
//! `bench/switch-cost`, which already prices it — 30 against 83 cooperative,
//! 74 against 83 preemptive — and the two rows answer different questions.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use panic_halt as _;
use riscv_semihosting::{debug, hprintln};

// Pulled in for the `critical-section` implementation the semihosting layer
// needs and which nothing here references by name.
use riscv as _;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::NoTrace;
use rusty_rtos_kernel_core::{list_slots_for, lists_for, Kernel};
use rusty_rtos_port_core::sim::SimPort;
use rusty_rtos_port_riscv::minstret;

/// How many samples per row. The same 512 the C arm uses, and the same 512
/// the silicon cell uses.
const SAMPLES: usize = 512;

/// How many times the measured call is made INSIDE one bracket. The poison
/// build makes it twice, and the row must move by one call.
const REPEAT: usize = if cfg!(feature = "poison") { 2 } else { 1 };

/// The config, matched to `bench/kernel-ram/c/FreeRTOSConfig.h` field for
/// field. A comparison at two different geometries is not a comparison.
#[derive(Debug, Clone, Copy, Default)]
pub struct MatchedConfig;

impl Config for MatchedConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 100;
    const MAX_PRIORITIES: u8 = 5;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 16;
    const TIMER_TASK_PRIORITY: u8 = 4;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 10;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
}

const TASKS: usize = 6;
const QUEUES: usize = 2;
const SLOTS: usize = 16;
const TIMERS: usize = 1;
const GROUPS: usize = 1;

type K = Kernel<
    MatchedConfig,
    SimPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { list_slots_for(TASKS, TIMERS, lists_for(MatchedConfig::MAX_PRIORITIES, QUEUES, GROUPS)) },
    { lists_for(MatchedConfig::MAX_PRIORITIES, QUEUES, GROUPS) },
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
}

fn summarise(samples: &mut [u32], tax: u32) -> Row {
    samples.sort_unstable();
    let sub = |v: u32| v.saturating_sub(tax);
    Row {
        median: sub(samples[samples.len() / 2]),
        min: sub(samples[0]),
        max: sub(samples[samples.len() - 1]),
    }
}

fn report(name: &str, row: &Row) {
    hprintln!(
        "ROW {} median={} min={} max={}",
        name,
        row.median,
        row.min,
        row.max
    );
}

fn fail(why: &str) -> ! {
    hprintln!("RESULT: FAIL -- {}", why);
    debug::exit(debug::EXIT_FAILURE);
    loop {
        core::hint::spin_loop();
    }
}

#[riscv_rt::entry]
fn main() -> ! {
    hprintln!();
    hprintln!("=== the Kairos kernel's tick/switch WORK rows, rv32 ===");
    hprintln!("clock   minstret under -icount shift=0 (retired instructions)");
    hprintln!("method  median of {}, bracket tax measured and subtracted", SAMPLES);
    hprintln!();

    // ------------------------------------------------------ the instrument --
    // Two reads back to back. Whatever this costs is in every row below, so
    // it comes off every row below.
    let mut tax_samples = [0u32; SAMPLES];
    for slot in tax_samples.iter_mut() {
        let a = minstret();
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    tax_samples.sort_unstable();
    let tax = tax_samples[SAMPLES / 2];
    hprintln!("TAX median={}", tax);

    // ----------------------------------------------------------- the setup --
    let mut kernel = match K::new(SimPort::new(), NoTrace) {
        Ok(k) => k,
        Err(_) => fail("the kernel geometry was refused"),
    };

    // Two tasks at one priority, so the scheduler has a decision to make.
    // The C arm calls them `mate` and `meas` and does exactly this.
    if kernel.create_task("mate", 3).is_err() {
        fail("mate was refused");
    }
    if kernel.create_task("meas", 3).is_err() {
        fail("measure was refused");
    }
    if kernel.start_scheduler().is_err() {
        fail("the scheduler would not start");
    }

    // ---------------------------------- ROW 1: a tick, delayed list EMPTY --
    let mut tick_samples = [0u32; SAMPLES];
    for slot in tick_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.increment_tick();
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let tick_idle = summarise(&mut tick_samples, tax);

    // Put something in the delayed list and keep it there. 1,000,000 ticks at
    // this config is 10,000 seconds, and the run is 1,024 ticks long, so it
    // never fires — the row measures a tick that must LOOK at a delayed task
    // and decide it is not due, which is the steady state of any system that
    // uses a delay.
    if kernel.delay(1_000_000).is_err() {
        fail("the delay was refused");
    }

    // ------------------------------ ROW 2: a tick, delayed list NON-EMPTY --
    let mut tickd_samples = [0u32; SAMPLES];
    for slot in tickd_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.increment_tick();
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let tick_delayed = summarise(&mut tickd_samples, tax);

    // --------------------------------- ROW 3: the scheduler's selection ----
    let mut switch_samples = [0u32; SAMPLES];
    for slot in switch_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            kernel.switch_context();
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let switch_select = summarise(&mut switch_samples, tax);

    report("tick_idle", &tick_idle);
    report("tick_delayed", &tick_delayed);
    report("switch_select", &switch_select);

    // Work-parity anchors: these must match the C arm exactly, or the two
    // arms did different work and the comparison is void.
    hprintln!();
    hprintln!(
        "ANCHOR samples={} tick_calls={} switch_calls={} tick_count={}",
        SAMPLES,
        2 * SAMPLES * REPEAT,
        SAMPLES * REPEAT,
        kernel.tick_count()
    );

    // A row that cannot out-resolve the instrument has not earned a figure.
    if tick_idle.median == 0 || switch_select.median == 0 {
        fail("a row did not exceed the bracket tax");
    }

    hprintln!("RESULT: PASS");
    debug::exit(debug::EXIT_SUCCESS);
    loop {
        core::hint::spin_loop();
    }
}

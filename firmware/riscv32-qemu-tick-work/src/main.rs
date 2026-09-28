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
use rusty_rtos_kernel_core::kernel::NotifyAction;
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
    /// `configUSE_QUEUE_SETS 0`, which that header sets explicitly.
    ///
    /// It was the one field of the match that had no Rust side to match: the
    /// kernel compiled queue sets unconditionally, so every row here carried a
    /// `set_container` test on each send — and the C arm these rows are
    /// compared against has no queue-set machinery at all. Matching it is what
    /// the doc comment above already claims.
    const USE_QUEUE_SETS: bool = false;
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
    { <MatchedConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
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

    // The work-parity anchor is captured HERE, not at the end.
    //
    // `run.sh` pairs exactly the three rows above against the C arm; every row
    // below is Kairos-only and has no C counterpart. Those rows take critical
    // sections, and on the sim an exit is the clock — so reading the tick
    // count after them compares a number the C arm was never asked to produce.
    // It read 1,025 against the C's 1,024 the moment the instrument was
    // widened, which is the anchor doing its job on the wrong quantity.
    let anchor_ticks = kernel.tick_count();

    // ------------------------ ROWS 4-7: the IPC paths, OURS against OURS --
    //
    // No C arm, on purpose. These exist to make our own hot API measurable so
    // a change to it has a deterministic before/after, and `run.sh` only pairs
    // the three rows above. The switch and the tick were the only measurable
    // surface in the kernel, which is why every win so far landed on them —
    // an instrument that covers two functions can only find wins in two
    // functions.
    //
    // Every row is the NON-blocking, non-waking case: a send to a queue with
    // room, a receive from one with an item, a give and a take that never
    // park. That is the steady state, and it is the path a blocking one pays
    // before it decides to block.
    let sq = match kernel.queue_create(4) {
        Ok(h) => h,
        Err(_) => fail("the measurement queue was refused"),
    };
    // NOT a semaphore: `QUEUES` is 2 here, the timer daemon holds one and the
    // queue above holds the other. Raising it would grow `LISTS` and `ITEMS`
    // and so move the three paired rows, breaking the match with the C arm's
    // FreeRTOSConfig — the geometry is part of the experiment. The event group
    // is declared (`GROUPS = 1`) and otherwise unused, so it costs nothing to
    // reach.
    let grp = match kernel.event_group_create() {
        Ok(h) => h,
        Err(_) => fail("the measurement event group was refused"),
    };

    let mut send_samples = [0u32; SAMPLES];
    for slot in send_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            // Send then receive, so the queue neither fills nor empties and
            // every iteration measures the same work.
            let _ = kernel.queue_send(sq, 1, 0);
            let _ = kernel.queue_receive(sq, 0);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let queue_roundtrip = summarise(&mut send_samples, tax);

    let mut grp_samples = [0u32; SAMPLES];
    for slot in grp_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            // Set then clear, so the group returns to its start state and
            // every iteration measures identical work.
            let _ = kernel.event_group_set_bits(grp, 0x1);
            let _ = kernel.event_group_clear_bits(grp, 0x1);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let group_roundtrip = summarise(&mut grp_samples, tax);

    // ------------- ROW 8: the SCAFFOLDING alone, to decompose the others --
    //
    // `queue_spaces_available` is `enter_critical` + one `resolve` +
    // `exit_critical` and nothing else. Subtracting it from the rows above
    // says how much of them is the critical section and the handle lookup —
    // which on the sim is the TIME SOURCE and so is not removable — and how
    // much is queue logic, which might be.
    let mut scaf_samples = [0u32; SAMPLES];
    for slot in scaf_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.queue_spaces_available(sq);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let scaffolding = summarise(&mut scaf_samples, tax);

    // ------------------- ROW 9: the BLOCKING path, a two-task ping-pong --
    //
    // The richest call-site counts in the whole binary are inside
    // `queue_take_blocking` and `queue_send_blocking` — six calls to
    // `tick_on_exit`, four to `port_yield`, three each to `unlock_queue`,
    // `drain_pending_ready_walk` and `unwind_pended_ticks_loop` — and none of
    // the rows above reach any of them, because none of them ever blocks.
    //
    // A stackless kernel cannot be made to block in a plain `REPEAT` loop: a
    // blocking call answers `Wait::Blocked` and PARKS the caller, so calling
    // it again from the parked task measures a sequence no system performs.
    // What loops cleanly is a two-task hand-off, which is what the
    // conformance runner does:
    //
    //   1. the current task receives on an empty queue  -> Blocked, parked
    //   2. switch                                       -> the other task runs
    //   3. it sends                                     -> the waiter is woken
    //   4. switch                                       -> back to the first
    //   5. it receives again                            -> resumes, drains
    //
    // After (5) the queue is empty, nobody waits, and the first task is
    // current again — the same state as before (1), so the cycle repeats and
    // `min == max` is the check that it really does.
    let mut block_samples = [0u32; SAMPLES];
    for slot in block_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.queue_receive(sq, 10);
            kernel.switch_context();
            let _ = kernel.queue_send(sq, 7, 0);
            kernel.switch_context();
            let _ = kernel.queue_receive(sq, 0);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let block_cycle = summarise(&mut block_samples, tax);

    // ------------------- ROWS 10 and 11: the FAILURE family --------------
    //
    // Every row above is a SUCCEEDING call, and a call-count census proved it:
    // `trace_failure_or_owe` — the single funnel eight call sites reach when
    // an operation fails — was entered **zero** times across all nine rows.
    // So the whole `OwedTrace` deferral machinery was invisible to this
    // instrument, and a change to it read as pure code layout.
    //
    // A failed queue operation is not an error path in the exceptional sense.
    // `xQueueSend(q, &v, 0)` on a full queue is how a producer polls, and
    // FreeRTOS prices it as a first-class call. It is also IDEMPOTENT — a
    // refused send stores nothing and a refused receive removes nothing — so
    // unlike the blocking cycle it loops cleanly in a plain `REPEAT` bracket,
    // and `min == max` proves the state really is unchanged.
    //
    // The queue is EMPTY here: `block_cycle` step 5 drained it.
    let mut recv_fail_samples = [0u32; SAMPLES];
    for slot in recv_fail_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.queue_receive(sq, 0);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let recv_empty = summarise(&mut recv_fail_samples, tax);

    // Fill it to the brim so every send below is refused.
    for _ in 0..4 {
        if kernel.queue_send(sq, 9, 0).is_err() {
            fail("the queue would not fill");
        }
    }
    let mut send_fail_samples = [0u32; SAMPLES];
    for slot in send_fail_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.queue_send(sq, 9, 0);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let send_full = summarise(&mut send_fail_samples, tax);

    // ------------------- ROWS 18/19: PEEK, and a pure counter -------------
    //
    // `queue_peek` is `queue_take::<true>` — a whole second monomorphisation
    // of the receive path that no row has ever priced, and the one place the
    // `PEEK` const generic earns or loses its keep. The queue is FULL here
    // (the row above filled it), so the peek succeeds and, because a peek
    // does not consume, it loops as cleanly as the failure rows.
    let mut peek_samples = [0u32; SAMPLES];
    for slot in peek_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.queue_peek(sq, 0);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let peek_ok = summarise(&mut peek_samples, tax);

    // A pure counter read: one resolve, one field, one critical section. The
    // cheapest queue call there is, and the floor the rows above sit on.
    let mut mw_samples = [0u32; SAMPLES];
    for slot in mw_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.queue_messages_waiting(sq);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let messages_waiting = summarise(&mut mw_samples, tax);

    // ------------------- ROW 12: the OWE FILTER, nothing owed -------------
    //
    // `resume_pending` is asked before every step the runner takes — 214,488
    // times in a 20,000-tick `StreamBufferDemo` — and answers "nothing owed"
    // almost every time. It is the single most-called entry point in the
    // kernel, and until this row existed the only instrument that could see
    // it was a two-minute host callgrind rebuild.
    //
    // Nothing here owes anything, so this is the fast path and nothing else:
    // the `unwinding` test, the `owes_anything` load, and the return.
    let mut owe_samples = [0u32; SAMPLES];
    for slot in owe_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.resume_pending();
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let owe_filter = summarise(&mut owe_samples, tax);

    // ------------------- ROW 13: an event-group wait that FAILS -----------
    //
    // The third member of the failure family, and the one that reaches
    // `take_event_resume` and the funnel's `events.rs` call site. Bit 0x2 is
    // never set by anything here, so the wait finds its condition unmet and,
    // with a zero block time, returns at once — leaving the group exactly as
    // it found it, so this loops as cleanly as the two rows above.
    let mut ev_fail_samples = [0u32; SAMPLES];
    for slot in ev_fail_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.event_group_wait_bits(grp, 0x2, false, true, 0);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let event_wait_fail = summarise(&mut ev_fail_samples, tax);

    // ------------------- ROWS 14-16: TASK NOTIFICATIONS -------------------
    //
    // The lightest thing a task can block on: no object, no event list, just
    // a slot in the TCB. FreeRTOS calls a notification "a lightweight
    // alternative to a binary semaphore", and the stream buffers are built on
    // one — so it is on the hot path of a whole family of the kernel, and
    // until now no row reached it at all. It needs no geometry: adding a
    // semaphore would move `QUEUES` and break the three rows matched to the C
    // arm's FreeRTOSConfig, and a notification costs nothing to declare.
    let me = kernel.current();

    // A give and a take that cancel, so the value returns to zero.
    let mut nfy_samples = [0u32; SAMPLES];
    for slot in nfy_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.notify(me, 0, 1, NotifyAction::Increment);
            let _ = kernel.notify_take(0, true, 0);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let notify_roundtrip = summarise(&mut nfy_samples, tax);

    // The failure-family member: nothing pending, zero block time, so the
    // take finds a zero value and returns at once. Idempotent.
    let mut nfy_fail_samples = [0u32; SAMPLES];
    for slot in nfy_fail_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.notify_take(0, true, 0);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let notify_take_empty = summarise(&mut nfy_fail_samples, tax);

    // A pure getter over the task arena, for the same reason `scaffolding`
    // exists: it decomposes the rows above into the arena lookup and the
    // rest.
    let mut prio_samples = [0u32; SAMPLES];
    for slot in prio_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.task_priority_get(None);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let priority_get = summarise(&mut prio_samples, tax);

    // ------------------- ROW 17: notify_wait, nothing pending -------------
    //
    // The other half of the notification API, and the one the STREAM BUFFERS
    // are built on — `stream_buffer_send` and `stream_buffer_receive` both
    // block through `notify_wait`. Nothing is pending and the block time is
    // zero, so it finds no notification and returns at once, leaving the
    // state as it found it.
    let mut nw_samples = [0u32; SAMPLES];
    for slot in nw_samples.iter_mut() {
        let a = minstret();
        for _ in 0..REPEAT {
            let _ = kernel.notify_wait(0, 0, 0, 0);
        }
        let b = minstret();
        *slot = b.wrapping_sub(a) as u32;
    }
    let notify_wait_empty = summarise(&mut nw_samples, tax);
    #[cfg(feature = "census")]
    hprintln!(
        "CENSUS owe_calls_total={}",
        rusty_rtos_kernel_core::kernel::CENSUS_OWE.load(core::sync::atomic::Ordering::Relaxed)
    );


    report("block_cycle", &block_cycle);
    report("recv_empty", &recv_empty);
    report("send_full", &send_full);
    report("peek_ok", &peek_ok);
    report("messages_waiting", &messages_waiting);
    report("owe_filter", &owe_filter);
    report("event_wait_fail", &event_wait_fail);
    report("notify_roundtrip", &notify_roundtrip);
    report("notify_take_empty", &notify_take_empty);
    report("priority_get", &priority_get);
    report("notify_wait_empty", &notify_wait_empty);
    report("scaffolding", &scaffolding);
    report("tick_idle", &tick_idle);
    report("tick_delayed", &tick_delayed);
    report("queue_roundtrip", &queue_roundtrip);
    report("group_roundtrip", &group_roundtrip);
    report("switch_select", &switch_select);

    // Work-parity anchors: these must match the C arm exactly, or the two
    // arms did different work and the comparison is void.
    hprintln!();
    hprintln!(
        "ANCHOR samples={} tick_calls={} switch_calls={} tick_count={}",
        SAMPLES,
        2 * SAMPLES * REPEAT,
        SAMPLES * REPEAT,
        anchor_ticks
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

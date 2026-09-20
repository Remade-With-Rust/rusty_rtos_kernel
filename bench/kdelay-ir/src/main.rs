//! Instruction counts for the kernel when tasks actually BLOCK.
//!
//! This one exists because none of its four siblings does it. khot-ir,
//! kipc-ir and kobj-ir pass `0` as the timeout on every queue, stream-buffer
//! and event-group call, so nothing is ever parked; ksched-ir suspends and
//! resumes and yields and ticks, which moves tasks between the ready and
//! suspended lists and never puts one on the delayed list. ksched-ir's own
//! header says as much about khot-ir and then does the same thing.
//!
//! What that leaves unmeasured is the single most expensive shape in
//! `list.c`: **`vListInsert`, the one function in the whole kernel with a
//! loop whose length is the number of blocked tasks.** Everything else the
//! lists do is O(1) -- `vListInsertEnd` is a push, `uxListRemove` is an
//! unlink, `listGET_OWNER_OF_NEXT_ENTRY` is a step. The sorted insert is the
//! only walk, and until this file nothing at the kernel level ran it.
//!
//! That gap is not academic. A change to the sorted insert measured -12% on
//! the list instrument and +4.25% on ksched-ir, because ksched-ir pays
//! whatever the change costs the other list operations and collects none of
//! what it saves. An instrument that cannot see a path cannot price it, and
//! it will happily return a number that looks like a verdict.
//!
//! **The workload.** Each round the running task calls `vTaskDelay` and the
//! scheduler picks another, so the delayed list gains depth; the tick entry
//! then walks it and wakes whatever is due. The delays are a spread rather
//! than a constant on purpose. A constant delay makes every wake time
//! `now + k` with `now` increasing, which arrives strictly ASCENDING and
//! lands at the tail every single time -- one arm of the insert, measured as
//! if it were the whole thing. The spread below lands in the middle and at
//! the head as well.
//!
//! Two ticks per delay keeps the delayed list bounded well short of the task
//! count, so the idle task is never the one asking to block.
//!
//! A deterministic counter, not a clock. The trace's event count and the
//! operation tallies are the work parity anchors.

use rusty_rtos_core::config::PosixDemoConfig;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::trace::CountTrace;

use rusty_rtos_kernel_core::kernel::{Kernel, TaskState};
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_port_core::SimPort;

type SchedKernel = Kernel<
    PosixDemoConfig,
    SimPort,
    CountTrace,
    NoTickHook,
    24,
    80,
    41,
    12,
    128,
    8,
    2048,
    32,
    4,
>;

const ROUNDS: u32 = 3_000;

/// How many tasks there are to block, and how fast time runs against the
/// delays -- which between them set how DEEP the delayed list gets.
///
/// The `deep` arm is not a different experiment, it is the same one at a
/// different depth. The sorted insert is the only walk in the kernel and its
/// cost IS its depth, so a change to it can only be priced against a stated
/// depth. The default reaches 4; `--features deep` reaches into the teens.
#[cfg(not(feature = "deep"))]
const TASKS: usize = 10;
#[cfg(feature = "deep")]
const TASKS: usize = 20;

/// Ticks per blocked task. Fewer ticks means a task stays parked longer,
/// which means more of them are parked at once.
#[cfg(not(feature = "deep"))]
const TICKS_PER_ROUND: u32 = 2;
#[cfg(feature = "deep")]
const TICKS_PER_ROUND: u32 = 1;

/// Wake-time spread, so the sorted insert is measured at every depth.
///
/// `wake_at` is `now + ticks` and `now` only increases, so a CONSTANT delay
/// arrives ascending and always belongs at the tail. These do not: a short
/// delay behind a long one sorts into the middle, and the shortest of all
/// sorts to the head, which is the walk at its longest.
#[cfg(not(feature = "deep"))]
const DELAYS: [u64; 10] = [7, 3, 11, 5, 13, 2, 9, 4, 15, 6];
#[cfg(feature = "deep")]
const DELAYS: [u64; 10] = [17, 9, 23, 13, 27, 7, 19, 11, 29, 15];

fn main() {
    let Ok(mut k) = SchedKernel::new(SimPort::new(), CountTrace::default()) else {
        println!("kernel geometry refused");
        return;
    };

    // Several priorities, so the ready lists are plural and a reselection
    // has somewhere to go -- and so the scheduler really does pick a
    // different task when the running one blocks.
    let names = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t",
    ];
    let mut tasks = [None; TASKS];
    for (i, slot) in tasks.iter_mut().enumerate() {
        let name = names.get(i).copied().unwrap_or("z");
        let priority = u8::try_from(i % 4).unwrap_or(0);
        *slot = k.create_task(name, priority).ok();
    }

    let _ = k.start_scheduler();

    // A queue nothing ever sends to, so a receive with a timeout parks the
    // caller on BOTH lists and the timeout is what wakes it.
    let queue = k.queue_create(4).ok();

    let mut delays = 0u64;
    let mut ticks = 0u64;
    let mut blocked_on_queue = 0u64;
    let mut deepest = 0usize;

    for round in 0..ROUNDS {
        let i = usize::try_from(round).unwrap_or(0) % DELAYS.len();
        let want = DELAYS.get(i).copied().unwrap_or(1);
        if k.delay(want).is_ok() {
            delays = delays.wrapping_add(1);
        }

        // ---- block on an EVENT LIST, not just the clock ----------------
        //
        // `vTaskDelay` puts a task on the delayed list and nothing else.
        // Every other way a task waits -- a queue, a semaphore, an event
        // group, a notification -- puts it on an EVENT LIST as well, through
        // `vTaskPlaceOnEventList`, which is `vListInsert` sorted by PRIORITY
        // rather than by wake time. That is a second sorted insert, on a
        // second list, and the tick has to unlink both when the timeout
        // fires.
        //
        // Receiving from a queue nobody sends to is the cheapest way to
        // reach it: the receive always times out, so the path runs every
        // time and the workload stays deterministic.
        if let Some(q) = queue {
            if matches!(k.queue_receive(q, 3), Ok(Wait::Blocked)) {
                blocked_on_queue = blocked_on_queue.wrapping_add(1);
            }
        }

        // ---- prove the list has DEPTH ---------------------------------
        //
        // Blocking tasks is not enough. An instrument that parks one task
        // and wakes it before parking the next measures the sorted insert on
        // a list of ONE, which is the same blind spot as never blocking at
        // all, moved one step further in -- and it would look identical in
        // the output. So the depth goes IN the output, and a reader can see
        // whether the walk had anything to walk.
        //
        // Sampled every 16th round: `task_state_get` is not free, and a
        // probe taken every round would be a tenth of the workload measuring
        // itself.
        if round % 16 == 0 {
            let mut blocked = 0usize;
            for slot in &tasks {
                if let Some(task) = *slot {
                    if matches!(k.task_state_get(task), Ok(TaskState::Blocked)) {
                        blocked = blocked.wrapping_add(1);
                    }
                }
            }
            if blocked > deepest {
                deepest = blocked;
            }
        }

        // Ticks per block. The delayed list settles well short of the task
        // count, so the idle task is never the one asking to block -- and the
        // tick entry gets to walk a list that is genuinely occupied rather
        // than empty.
        for _ in 0..TICKS_PER_ROUND {
            k.tick_from_isr();
            ticks = ticks.wrapping_add(1);
        }
    }

    let events = k.into_trace().events;
    println!("checksum {events}");
    println!(
        "rounds {ROUNDS} delays {delays} queue_blocks {blocked_on_queue} ticks {ticks} deepest {deepest} events {events}"
    );
}

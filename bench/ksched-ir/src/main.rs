//! Instruction counts for the SCHEDULER, where khot-ir measures the queues.
//!
//! khot-ir never blocks, so the ready and delayed lists barely move and
//! `switch_context` hardly runs -- the queue machinery dominates it. This
//! drives the other half: suspend and resume walk tasks between the ready and
//! suspended lists, `task_yield` forces a reselection, and the tick entry walks
//! the delayed lists and makes the time-slice decision.
//!
//! Same shape as its sibling: a counting trace, a no-op tick hook, the
//! conformance corpus's geometry, and a deterministic counter rather than a
//! clock. The trace's event count and the operation tallies are the work
//! parity anchors.

use rusty_rtos_core::config::PosixDemoConfig;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::trace::CountTrace;

use rusty_rtos_kernel_core::kernel::Kernel;
use rusty_rtos_port_core::SimPort;

type SchedKernel = Kernel<
    PosixDemoConfig,
    SimPort,
    CountTrace,
    NoTickHook,
    24,
    // Derived, never restated: `ITEMS` is the list SLOT count, which
    // must be a power of two and is what `Kernel::new` checks against
    // `list_slots_for(TASKS, TIMERS, LISTS)`. A literal here was 80,
    // which stopped being valid when slots became the unit, and broke
    // every one of these benches at compile time.
    { rusty_rtos_kernel_core::list_slots_for(24, 32, 41) },
    41,
    12,
    128,
    8,
    2048,
    32,
    4,
>;

const ROUNDS: u32 = 3_000;
const TASKS: usize = 10;

fn main() {
    let Ok(mut k) = SchedKernel::new(SimPort::new(), CountTrace::default()) else {
        println!("kernel geometry refused");
        return;
    };

    // Several priorities, so the ready lists are genuinely plural and a
    // reselection has somewhere to go.
    let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
    let mut tasks = [None; TASKS];
    for (i, slot) in tasks.iter_mut().enumerate() {
        let name = names.get(i).copied().unwrap_or("z");
        let priority = u8::try_from(i % 4).unwrap_or(0);
        *slot = k.create_task(name, priority).ok();
    }

    let _ = k.start_scheduler();

    let mut suspends = 0u64;
    let mut resumes = 0u64;
    let mut yields = 0u64;
    let mut ticks = 0u64;
    let mut states = 0u64;

    for round in 0..ROUNDS {
        // Walk one task out of the ready lists and back in. This is the
        // insert/remove path the scheduler spends its life in.
        let which = usize::try_from(round).unwrap_or(0) % TASKS;
        if let Some(Some(task)) = tasks.get(which).copied() {
            if k.suspend(Some(task)).is_ok() {
                suspends = suspends.wrapping_add(1);
            }
            if k.task_state_get(task).is_ok() {
                states = states.wrapping_add(1);
            }
            if k.resume(task).is_ok() {
                resumes = resumes.wrapping_add(1);
            }
        }

        // A reselection at the current priority.
        k.task_yield();
        yields = yields.wrapping_add(1);

        // The tick entry: the delayed lists and the time slice.
        k.tick_from_isr();
        ticks = ticks.wrapping_add(1);
    }

    let events = k.into_trace().events;
    println!("checksum {events}");
    println!(
        "rounds {ROUNDS} suspends {suspends} resumes {resumes} states {states} yields {yields} ticks {ticks} events {events}"
    );
}

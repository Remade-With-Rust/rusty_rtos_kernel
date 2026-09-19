//! Instruction counts for event groups and mutexes -- the last two kernel
//! subsystems nothing measured.
//!
//! khot-ir has the queues, ksched-ir the scheduler, kipc-ir the notifications,
//! buffers and timers. These two are what is left: the bit-set synchroniser
//! and the ownership primitive with priority inheritance.
//!
//! Same shape as its siblings: a counting trace, a no-op tick hook, the
//! conformance corpus's geometry, a deterministic counter. The operation
//! tallies and the trace event count are the work parity anchors.

use rusty_rtos_core::config::PosixDemoConfig;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::trace::CountTrace;

use rusty_rtos_kernel_core::kernel::Kernel;
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_port_core::SimPort;

type ObjKernel = Kernel<
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

fn main() {
    let Ok(mut k) = ObjKernel::new(SimPort::new(), CountTrace::default()) else {
        println!("kernel geometry refused");
        return;
    };

    for name in ["a", "b", "c"] {
        let _ = k.create_task(name, 1);
    }
    let _ = k.start_scheduler();

    let group = k.event_group_create().ok();
    let mutex = k.mutex_create().ok();
    let recursive = k.mutex_create_recursive().ok();

    let mut sets = 0u64;
    let mut clears = 0u64;
    let mut reads = 0u64;
    let mut waits = 0u64;
    let mut takes = 0u64;
    let mut gives = 0u64;

    for round in 0..ROUNDS {
        if let Some(g) = group {
            // Set and clear a rotating bit pattern, so the wait-list scan the
            // setter runs has something to look at each time.
            let bits = 1u32 << (round % 24);
            if k.event_group_set_bits(g, bits).is_ok() {
                sets = sets.wrapping_add(1);
            }
            if k.event_group_bits(g).is_ok() {
                reads = reads.wrapping_add(1);
            }
            // A non-blocking wait, both ways round: the condition met and
            // not met, which are the two arms of the wait path.
            if k.event_group_wait_bits(g, bits, false, false, 0).is_ok() {
                waits = waits.wrapping_add(1);
            }
            if k.event_group_wait_bits(g, !bits, false, true, 0).is_ok() {
                waits = waits.wrapping_add(1);
            }
            if k.event_group_clear_bits(g, bits).is_ok() {
                clears = clears.wrapping_add(1);
            }
        }

        // The plain mutex: take and give, which is where the holder is
        // recorded and priority inheritance is decided.
        if let Some(m) = mutex {
            if matches!(k.semaphore_take(m, 0), Ok(Wait::Ready(()))) {
                takes = takes.wrapping_add(1);
            }
            if matches!(k.semaphore_give(m), Ok(Wait::Ready(()))) {
                gives = gives.wrapping_add(1);
            }
        }

        // The recursive one, twice deep, which is the counted path.
        if let Some(r) = recursive {
            if matches!(k.mutex_take_recursive(r, 0), Ok(Wait::Ready(()))) {
                takes = takes.wrapping_add(1);
            }
            if matches!(k.mutex_take_recursive(r, 0), Ok(Wait::Ready(()))) {
                takes = takes.wrapping_add(1);
            }
            if k.mutex_give_recursive(r).is_ok() {
                gives = gives.wrapping_add(1);
            }
            if k.mutex_give_recursive(r).is_ok() {
                gives = gives.wrapping_add(1);
            }
        }

        k.tick_from_isr();
    }

    let events = k.into_trace().events;
    println!("checksum {events}");
    println!(
        "rounds {ROUNDS} sets {sets} clears {clears} reads {reads} waits {waits} takes {takes} gives {gives} events {events}"
    );
}

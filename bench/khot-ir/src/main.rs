//! Instruction counts for the SHIPPED kernel, with nothing else in the frame.
//!
//! The conformance corpus is the gate and it is worth measuring, but about 90%
//! of it is demo task scaffolding and the trace sink turning kernel events into
//! text. A firmware runs neither. This drives the kernel API directly over a
//! counting trace and a no-op tick hook, so what the counter sees is the
//! scheduler, the queues and the lists.
//!
//! A deterministic counter, not a clock. The trace's event count and the
//! operation tallies are the work parity anchors: a change that moves any of
//! them changed behaviour.

use rusty_rtos_core::config::PosixDemoConfig;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::trace::CountTrace;

use rusty_rtos_kernel_core::kernel::Kernel;
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_port_core::SimPort;

/// The geometry the conformance corpus is built at, so the arenas and lists
/// are the same shapes the gated kernel uses.
type HotKernel = Kernel<
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

const ROUNDS: u32 = 4_000;

fn main() {
    let Ok(mut k) = HotKernel::new(SimPort::new(), CountTrace::default()) else {
        println!("kernel geometry refused");
        return;
    };

    let mut blocks = 0u64;
    let mut sends = 0u64;
    let mut receives = 0u64;
    let mut takes = 0u64;
    let mut gives = 0u64;
    let mut ticks = 0u64;

    // A handful of tasks so the ready lists have real occupants and a switch
    // has somewhere to go.
    for i in 0..6u8 {
        let name = match i {
            0 => "a",
            1 => "b",
            2 => "c",
            3 => "d",
            4 => "e",
            _ => "f",
        };
        let _ = k.create_task(name, u8::from(i % 3));
    }

    let Ok(queue) = k.queue_create(8) else {
        println!("queue refused");
        return;
    };
    let Ok(sem) = k.semaphore_create_counting(8, 0) else {
        println!("semaphore refused");
        return;
    };
    // A queue nothing ever sends to, so a receive with a real timeout parks
    // the caller instead of answering it.
    let Ok(starved) = k.queue_create(2) else {
        println!("starved queue refused");
        return;
    };

    let _ = k.start_scheduler();

    for round in 0..ROUNDS {
        // Fill and drain, so both the empty and the full ends of the queue's
        // bookkeeping are walked rather than just the middle.
        for n in 0..8u64 {
            if matches!(k.queue_send(queue, n, 0), Ok(Wait::Ready(()))) {
                sends = sends.wrapping_add(1);
            }
        }
        for _ in 0..8 {
            if matches!(k.queue_receive(queue, 0), Ok(Wait::Ready(_))) {
                receives = receives.wrapping_add(1);
            }
        }

        // The counting semaphore, which is the same queue machinery with no
        // payload.
        for _ in 0..4 {
            if matches!(k.semaphore_give(sem), Ok(Wait::Ready(()))) {
                gives = gives.wrapping_add(1);
            }
        }
        for _ in 0..4 {
            if matches!(k.semaphore_take(sem, 0), Ok(Wait::Ready(()))) {
                takes = takes.wrapping_add(1);
            }
        }

        // ---- BLOCK, which nothing here used to do ----------------------
        //
        // Every queue and semaphore call above passes 0 as its timeout, so
        // the queue is exercised only on the path where it answers
        // immediately. The other path is the one with the machinery in it:
        // `vTaskPlaceOnEventList` puts the caller on an event list sorted by
        // PRIORITY, `prvAddCurrentTaskToDelayedList` puts it on the delayed
        // list sorted by wake time, and the timeout has to unlink BOTH.
        //
        // A receive from a queue nobody fills reaches all of it, every
        // round, deterministically. The semaphore take is the same machinery
        // with no payload and a different waiter list.
        if matches!(k.queue_receive(starved, 3), Ok(Wait::Blocked)) {
            blocks = blocks.wrapping_add(1);
        }
        if matches!(k.semaphore_take(sem, 2), Ok(Wait::Blocked)) {
            blocks = blocks.wrapping_add(1);
        }

        // A tick every round, which is where the delayed lists and the
        // time-slice decision get walked.
        if round % 2 == 0 {
            k.tick_from_isr();
            ticks = ticks.wrapping_add(1);
        }
    }

    let events = k.into_trace().events;
    println!("checksum {events}");
    println!(
        "rounds {ROUNDS} sends {sends} receives {receives} gives {gives} takes {takes} blocks {blocks} ticks {ticks} events {events}"
    );
}

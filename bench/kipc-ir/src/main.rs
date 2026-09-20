//! Instruction counts for the kernel's IPC subsystems, which nothing else counts.
//!
//! khot-ir measures the queues and ksched-ir the scheduler. These three have
//! never been measured at all:
//!
//!   * task notifications -- the lightest thing a task can block on, a slot in
//!     the TCB rather than an object;
//!   * stream and message buffers -- the byte-oriented half of the IPC surface;
//!   * software timers, whose commands go through the timer queue.
//!
//! Same shape as its siblings: a counting trace, a no-op tick hook, the
//! conformance corpus's geometry, a deterministic counter. The operation
//! tallies and the trace event count are the work parity anchors.

use rusty_rtos_core::config::PosixDemoConfig;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::trace::CountTrace;

use rusty_rtos_kernel_core::kernel::{Kernel, NotifyAction};
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_port_core::SimPort;

type IpcKernel = Kernel<
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

const ROUNDS: u32 = 2_000;

fn main() {
    let Ok(mut k) = IpcKernel::new(SimPort::new(), CountTrace::default()) else {
        println!("kernel geometry refused");
        return;
    };

    let names = ["a", "b", "c", "d"];
    let mut tasks = [None; 4];
    for (i, slot) in tasks.iter_mut().enumerate() {
        *slot = k
            .create_task(names.get(i).copied().unwrap_or("z"), 1)
            .ok();
    }
    let _ = k.start_scheduler();

    let stream = k.stream_buffer_create(64, 1).ok();
    let message = k.message_buffer_create(64).ok();
    let timer = k.timer_create("t", 10, true, 0, 0).ok();

    // A stream buffer nothing ever fills, so a receive with a real timeout
    // parks the caller instead of answering it.
    let starved = k.stream_buffer_create(64, 1).ok();

    let mut blocks = 0u64;
    let mut notifies = 0u64;
    let mut takes = 0u64;
    let mut sends = 0u64;
    let mut receives = 0u64;
    let mut commands = 0u64;
    let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let mut sink = [0u8; 16];

    for round in 0..ROUNDS {
        // Notifications, against each task in turn and each action.
        if let Some(Some(task)) = tasks.get(usize::try_from(round).unwrap_or(0) % 4).copied() {
            let action = match round % 4 {
                0 => NotifyAction::SetBits,
                1 => NotifyAction::Increment,
                2 => NotifyAction::Overwrite,
                _ => NotifyAction::NoOverwrite,
            };
            if k.notify(task, 0, u32::from(round as u16), action).is_ok() {
                notifies = notifies.wrapping_add(1);
            }
        }
        if matches!(k.notify_take(0, true, 0), Ok(Wait::Ready(_))) {
            takes = takes.wrapping_add(1);
        }

        // The byte-oriented half: a stream keeps no frame boundary, a message
        // buffer keeps one, and the two take different paths through the same
        // storage.
        if let Some(b) = stream {
            if matches!(k.stream_buffer_send(b, &payload, 0), Ok(Wait::Ready(_))) {
                sends = sends.wrapping_add(1);
            }
            if matches!(k.stream_buffer_receive(b, &mut sink, 0), Ok(Wait::Ready(_))) {
                receives = receives.wrapping_add(1);
            }
        }
        if let Some(b) = message {
            if matches!(k.stream_buffer_send(b, &payload, 0), Ok(Wait::Ready(_))) {
                sends = sends.wrapping_add(1);
            }
            if matches!(k.stream_buffer_receive(b, &mut sink, 0), Ok(Wait::Ready(_))) {
                receives = receives.wrapping_add(1);
            }
        }

        // ---- BLOCK, which nothing here used to do ----------------------
        //
        // Every notification and stream-buffer call above passes 0 as its
        // timeout, so both are exercised only where they answer at once. The
        // blocking path is the one with the machinery: a notification waiter
        // parks on the DELAYED list alone (a task waiting on a notification
        // is on no event list at all, which is its own special case in
        // `eTaskGetState`), while a stream-buffer waiter parks on an event
        // list AND the delayed list, and the timeout unlinks both.
        //
        // Two different shapes, and neither had an instrument.
        if matches!(k.notify_wait(0, 0, 0, 2), Ok(Wait::Blocked)) {
            blocks = blocks.wrapping_add(1);
        }
        if let Some(b) = starved {
            if matches!(k.stream_buffer_receive(b, &mut sink, 3), Ok(Wait::Blocked)) {
                blocks = blocks.wrapping_add(1);
            }
        }

        // Timer commands, which travel through the timer queue.
        if let Some(t) = timer {
            if k.timer_start(t, 0).is_ok() {
                commands = commands.wrapping_add(1);
            }
            if k.timer_stop(t, 0).is_ok() {
                commands = commands.wrapping_add(1);
            }
        }

        k.tick_from_isr();
    }

    let events = k.into_trace().events;
    println!("checksum {events}");
    println!(
        "rounds {ROUNDS} notifies {notifies} takes {takes} sends {sends} receives {receives} blocks {blocks} commands {commands} events {events}"
    );
}

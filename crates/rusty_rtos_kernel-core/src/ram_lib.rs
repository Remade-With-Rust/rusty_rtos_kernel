#![no_std]
//! What a Kairos kernel occupies in RAM, on the target, before any task
//! exists — and how that total moves when each arena grows.
//!
//! # Why a `static` and not a `println!`
//!
//! The number wanted here is `size_of` on **rv32**, and `size_of` on the
//! host is a different number: a host `usize` is eight bytes where the
//! target's is four. Printing it from a host test would be measuring the
//! wrong machine.
//!
//! So each probe is a zero-filled array whose LENGTH is the size being
//! measured. The linker then records that length as the symbol's size, and
//! `llvm-nm -S` reads it straight back out of the object built for the real
//! target. No value decoding, no emulator, and nothing to run.
//!
//! # Why several of them
//!
//! One total is not a decomposition. FreeRTOS takes its TCBs, queues and
//! timers from the heap on demand, so its static cost does not move when a
//! task is added; Kairos declares arenas, so its static cost is the whole
//! budget and moves with every dimension. Those are different floor
//! FUNCTIONS, and a function needs more than one point. Each pair below
//! differs in exactly one dimension, so the difference between them is that
//! dimension's per-unit cost — a slope, not a guess.
//!
//! Note that `ITEMS` and `LISTS` are derived from the other dimensions by
//! `items_for`/`lists_for`, exactly as a real configuration derives them.
//! A task costs its list items too, and charging it for them is the point.

use core::mem::size_of;

use rusty_rtos_core::config::PosixDemoConfig;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::trace::NoTrace;
use rusty_rtos_kernel_core::kernel::Kernel;
use rusty_rtos_kernel_core::{list_slots_for, lists_for};
use rusty_rtos_port_core::sim::SimPort;

/// `PosixDemoConfig::MAX_PRIORITIES`, which sizes the ready lists. Written
/// out rather than read through the trait so it reads beside the C arm's
/// `configMAX_PRIORITIES 5`, which is the same number for the same reason.
const PRIOS: u8 = 5;

/// One geometry's size, in bytes, on the target.
///
/// A macro rather than a generic type alias because stable Rust will not let
/// a generic const parameter feed a const expression:
/// `list_slots_for(TASKS, ..)` is rejected where `list_slots_for(8, 16, 9)`
/// is fine. Substituting literals first
/// sidesteps that, and costs only that each geometry is written out.
///
/// The trace and the tick hook are the no-op ones, so what is measured is
/// the kernel rather than a demo's instrumentation.
macro_rules! ksize {
    ($tasks:literal, $queues:literal, $slots:literal,
     $buffers:literal, $bytes:literal, $timers:literal, $groups:literal) => {
        size_of::<
            Kernel<
                PosixDemoConfig,
                SimPort,
                NoTrace,
                NoTickHook,
                $tasks,
                { list_slots_for($tasks, $timers, lists_for(PRIOS, $queues, $groups)) },
                { lists_for(PRIOS, $queues, $groups) },
                $queues,
                $slots,
                $buffers,
                $bytes,
                $timers,
                $groups,
                { <PosixDemoConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
            >,
        >()
    };
}

// The baseline geometry, and one neighbour per dimension. Each differs from
// BASE in exactly one place, so each difference is that dimension's slope.
const BASE: usize = ksize!(8, 8, 64, 4, 1024, 16, 2);
const TASKS_16: usize = ksize!(16, 8, 64, 4, 1024, 16, 2);
const QUEUES_16: usize = ksize!(8, 16, 64, 4, 1024, 16, 2);
const SLOTS_128: usize = ksize!(8, 8, 128, 4, 1024, 16, 2);
const BUFFERS_8: usize = ksize!(8, 8, 64, 8, 1024, 16, 2);
const BYTES_2048: usize = ksize!(8, 8, 64, 4, 2048, 16, 2);
const TIMERS_32: usize = ksize!(8, 8, 64, 4, 1024, 32, 2);
const GROUPS_4: usize = ksize!(8, 8, 64, 4, 1024, 16, 4);

/// The geometry the conformance corpus actually runs, on every architecture
/// it runs on. Read off the mangled names in the built rv32 firmware:
/// TASKS 24, QUEUES 12, SLOTS 128, BUFFERS 8, BYTES 2048, TIMERS 32,
/// GROUPS 4.
const CORPUS: usize = ksize!(24, 12, 128, 8, 2048, 32, 4);

macro_rules! probe {
    ($sym:ident, $size:expr) => {
        #[no_mangle]
        pub static $sym: [u8; $size] = [0; $size];
    };
}

probe!(KAIROS_RAM_BASE, BASE);
probe!(KAIROS_RAM_TASKS_16, TASKS_16);
probe!(KAIROS_RAM_QUEUES_16, QUEUES_16);
probe!(KAIROS_RAM_SLOTS_128, SLOTS_128);
probe!(KAIROS_RAM_BUFFERS_8, BUFFERS_8);
probe!(KAIROS_RAM_BYTES_2048, BYTES_2048);
probe!(KAIROS_RAM_TIMERS_32, TIMERS_32);
probe!(KAIROS_RAM_GROUPS_4, GROUPS_4);
probe!(KAIROS_RAM_CORPUS, CORPUS);

// ------------------------------------------- what DECLARING buys you --
//
// `BASE` is a hand-picked geometry: 8 tasks, 8 queues, 16 timers, a 1 KiB
// byte arena. The whole point of declaring is that an application that uses
// less pays less, and the static-vs-static ratio against C is measured at a
// geometry nobody's application actually has. These are the geometries a real
// firmware declares, so the row can be read at the size it will be paid at.

/// A blinker: two tasks, one queue, one timer, no stream buffers at all.
const TINY: usize = ksize!(2, 1, 8, 0, 0, 1, 0);
/// A small sensor node: four tasks, two queues, two timers, one event group.
const SMALL: usize = ksize!(4, 2, 16, 0, 0, 2, 1);
/// The same, with a 256-byte stream buffer for a UART.
const SMALL_SB: usize = ksize!(4, 2, 16, 1, 256, 2, 1);

probe!(KAIROS_RAM_TINY, TINY);
probe!(KAIROS_RAM_SMALL, SMALL);
probe!(KAIROS_RAM_SMALL_SB, SMALL_SB);

// ---------------------------------------------------- the decomposition --
//
// One total is not a decomposition, and neither is a set of SLOPES: `ITEMS`
// and `LISTS` are derived from `TASKS`, `QUEUES` and `GROUPS`, so the size
// function is not linear and the per-dimension slopes above do not sum to
// `BASE`. Adding them up and calling the difference a "constant term" gives
// a number that means nothing -- it was tried, and it read 696 bytes of
// nonsense.
//
// These are the actual fields, measured on the target by the kernel itself
// (the arena types are private to `rusty_rtos_kernel-core`, so nothing out
// here can do it). `ACCOUNTED` is their sum; `BASE - ACCOUNTED` is the
// scalar tail plus layout padding, and it is printed rather than assumed.

/// The BASE geometry as a type, so the kernel's own footprint consts can be
/// read off it.
type Base = Kernel<
    PosixDemoConfig,
    SimPort,
    NoTrace,
    NoTickHook,
    8,
    { list_slots_for(8, 16, lists_for(PRIOS, 8, 2)) },
    { lists_for(PRIOS, 8, 2) },
    8,
    64,
    4,
    1024,
    16,
    2,
    { <PosixDemoConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
>;

probe!(KAIROS_FP_TOTAL, Base::FOOTPRINT);
probe!(KAIROS_FP_TCBS, Base::FOOTPRINT_TCBS);
probe!(KAIROS_FP_QUEUES, Base::FOOTPRINT_QUEUES);
probe!(KAIROS_FP_LISTS, Base::FOOTPRINT_LISTS);
probe!(KAIROS_FP_SLOTS, Base::FOOTPRINT_SLOTS);
probe!(KAIROS_FP_BUFFERS, Base::FOOTPRINT_BUFFERS);
probe!(KAIROS_FP_TIMERS, Base::FOOTPRINT_TIMERS);
probe!(KAIROS_FP_GROUPS, Base::FOOTPRINT_GROUPS);
probe!(KAIROS_FP_TIMER_MESSAGES, Base::FOOTPRINT_TIMER_MESSAGES);
probe!(KAIROS_FP_BYTES, Base::FOOTPRINT_BYTES);
probe!(KAIROS_FP_FREE_LISTS, Base::FOOTPRINT_FREE_LISTS);
probe!(KAIROS_FP_PER_TASK_SIDE, Base::FOOTPRINT_PER_TASK_SIDE);
probe!(KAIROS_FP_ACCOUNTED, Base::FOOTPRINT_ACCOUNTED);

// ------------------------------------- the per-TIMER slope, decomposed --
//
// The row reports 120 B a timer against the C's 40, and a total says nothing
// about WHERE. `Timer` itself is only 36 B of fields. These are the same
// geometry as `Base` with TIMERS raised to 32, so each component's slope is
// `(T32 - Base) / 16` and the parts must sum to the 120 the bench measures.
type T32 = Kernel<
    PosixDemoConfig,
    SimPort,
    NoTrace,
    NoTickHook,
    8,
    { list_slots_for(8, 32, lists_for(PRIOS, 8, 2)) },
    { lists_for(PRIOS, 8, 2) },
    8,
    64,
    4,
    1024,
    32,
    2,
    { <PosixDemoConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
>;

probe!(KAIROS_T32_TOTAL, T32::FOOTPRINT);
probe!(KAIROS_T32_TIMERS, T32::FOOTPRINT_TIMERS);
probe!(KAIROS_T32_LISTS, T32::FOOTPRINT_LISTS);
probe!(KAIROS_T32_ACCOUNTED, T32::FOOTPRINT_ACCOUNTED);

/// TIMERS = 17: one more than `Base`, and INSIDE the same power-of-two band
/// of list slots. `Base` needs `8*2 + 16 + 29 = 61` slots and this needs 62;
/// both round to 64. So `T17 - Base` is the marginal cost of one timer with
/// no granularity step in it, and `T32 - Base` is not.
type T17 = Kernel<
    PosixDemoConfig,
    SimPort,
    NoTrace,
    NoTickHook,
    8,
    { list_slots_for(8, 17, lists_for(PRIOS, 8, 2)) },
    { lists_for(PRIOS, 8, 2) },
    8,
    64,
    4,
    1024,
    17,
    2,
    { <PosixDemoConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
>;

probe!(KAIROS_T17_TOTAL, T17::FOOTPRINT);
probe!(KAIROS_T17_TIMERS, T17::FOOTPRINT_TIMERS);
probe!(KAIROS_T17_LISTS, T17::FOOTPRINT_LISTS);

/// A staticlib needs one even though nothing here can panic.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

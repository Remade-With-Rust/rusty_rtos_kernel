#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
//! `rusty_rtos_kernel-core` — the FreeRTOS scheduler remade in safe Rust.
//!
//! This crate is `tasks.c` and `queue.c` as a state machine over indices:
//! the ready lists, the two delayed lists, the pending-ready and suspended
//! lists, the tick, the context switch, and the queue with its two event
//! lists. It owns no CPU (that is [`rusty_rtos_core::port::Port`]), no
//! memory (that is `Heap`), and no stack — which is the point.
//!
//! # Why there is no context switch here
//!
//! A context switch is a stack swap, and a stack swap is `unsafe`. The
//! family's law is that `unsafe` lives only in `rusty_rtos_port-<arch>`
//! (mission plan §2.1), so this crate never touches one. What it does
//! instead is decide *which task should run*: [`Kernel::switch_context`]
//! moves `current` and fires the same `TASK_SWITCHED_OUT` / `_IN` pair the
//! C kernel fires. On silicon the port acts on that decision with its
//! fenced assembly; on the sim the runner acts on it by calling the next
//! task's step function. Both see the same decisions in the same order,
//! which is what the conformance diff checks.
//!
//! # Geometry is const, not heap
//!
//! FreeRTOS allocates TCBs and queue storage from a heap. Kairos puts them
//! in arenas sized at compile time, so the kernel has no allocator at all
//! and a firmware's RAM is a number you can read off the type:
//!
//! ```ignore
//! type K = Kernel<PosixDemoConfig, SimPort, SimTrace,
//!                 16,                        // TASKS
//!                 { list_slots_for(8, 0, LISTS) }, // items + markers, rounded
//!                 { lists_for(7, 8) },       // ready + 4 + two per queue
//!                 8,                         // QUEUES
//!                 64>;                       // shared queue slots
//! ```
//!
//! The four helpers below compute the derived numbers, and
//! [`Kernel::new`] refuses a geometry that does not add up rather than
//! indexing out of bounds later.
//!
//! `forbid(unsafe)`. `no_std` (+ `alloc`). Every arithmetic operation is
//! checked, wrapping or saturating by name; every slice access is a `get`.

pub mod events;
pub mod kernel;
pub mod name;
#[cfg(kani)]
pub mod proofs;
pub mod queue;
pub mod stream;
pub mod system;
pub mod timer;
pub mod typed;

pub use kernel::{Kernel, Stall, StartHandles, TaskState};
// The macro-generated `build` names this bound, so it has to be
// reachable from `$crate` at the call site.
pub use name::{NAME_CAPACITY, Name};
pub use queue::{Blocked, Kind as QueueKind, Position, Ready, Wait};
pub use rusty_rtos_core::hooks::TickHook;

/// Crate version, for manifests and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// List items a kernel needs for `tasks` tasks: a state item and an event
/// item each, exactly the two `ListItem_t`s in a C `TCB_t`.
#[must_use]
pub const fn items_for(tasks: usize, timers: usize) -> usize {
    tasks.saturating_mul(2).saturating_add(timers)
}

/// Slots the kernel's list arena needs.
///
/// `ListsOf`'s first const parameter is the SLOT count, not the item count:
/// each list's end marker is a node in the same array — which is what C
/// FreeRTOS does, `xListEnd` being a `ListItem_t` inside `List_t` — and the
/// total is rounded up to a power of two so every link can be followed with
/// a mask instead of a bounds check.
///
/// Measured on `bench/list-cost`, that shape took the list from **35.84 to
/// 18.80 instructions per operation**, against C `list.c`'s 22.32 at `-O2`
/// -- and, at the 32-bit width every Kairos target actually has, **23.75
/// against 33.42**, which is the ratio that matters.
///
/// [`items_for`] keeps its own meaning — two list items per task plus one
/// per timer — because that is a true number and worth being able to say.
/// This wraps it.
#[must_use]
pub const fn list_slots_for(tasks: usize, timers: usize, lists: usize) -> usize {
    rusty_rtos_core::list::slots_for(items_for(tasks, timers), lists)
}

/// Lists a kernel needs: one ready list per priority, the two delayed
/// lists, the pending-ready list and the suspended list, plus the two
/// event lists (`xTasksWaitingToSend`, `xTasksWaitingToReceive`) of every
/// queue and the one (`xTasksWaitingForBits`) of every event group.
#[must_use]
pub const fn lists_for(max_priorities: u8, queues: usize, groups: usize) -> usize {
    (max_priorities as usize)
        .saturating_add(OVERHEAD_LISTS)
        .saturating_add(queues.saturating_mul(2))
        .saturating_add(groups)
}

/// The number of fixed lists that are not a ready list: the two delayed
/// lists, pending-ready and suspended, and the two timer lists.
pub const OVERHEAD_LISTS: usize = 6;

/// The tasks a kernel creates for itself whatever the application asks
/// for: `prvIdleTask` and the timer daemon. A system that declares `n`
/// tasks needs room for `n + OVERHEAD_TASKS`.
pub const OVERHEAD_TASKS: usize = 2;

/// The names a scenario or a port wants in scope.
pub mod prelude {
    pub use crate::kernel::{Kernel, StartHandles, TaskState};
    pub use crate::name::Name;
    pub use crate::queue::{Blocked, Position, Ready, Wait};
    pub use crate::{items_for, list_slots_for, lists_for};
    pub use rusty_rtos_core::prelude::*;
}

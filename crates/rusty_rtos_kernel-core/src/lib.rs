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
//!                 { items_for(16) },         // two list items per task
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

pub mod kernel;
pub mod name;
pub mod queue;
pub mod stream;

pub use kernel::{Kernel, StartHandles, TaskState};
pub use name::{NAME_CAPACITY, Name};
pub use queue::{Blocked, Kind as QueueKind, Position, Ready, Wait};

/// Crate version, for manifests and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// List items a kernel needs for `tasks` tasks: a state item and an event
/// item each, exactly the two `ListItem_t`s in a C `TCB_t`.
#[must_use]
pub const fn items_for(tasks: usize) -> usize {
    tasks.saturating_mul(2)
}

/// Lists a kernel needs: one ready list per priority, the two delayed
/// lists, the pending-ready list and the suspended list, plus the two
/// event lists (`xTasksWaitingToSend`, `xTasksWaitingToReceive`) of every
/// queue.
#[must_use]
pub const fn lists_for(max_priorities: u8, queues: usize) -> usize {
    (max_priorities as usize)
        .saturating_add(OVERHEAD_LISTS)
        .saturating_add(queues.saturating_mul(2))
}

/// The number of fixed lists that are not a ready list: the two delayed
/// lists, pending-ready and suspended.
pub const OVERHEAD_LISTS: usize = 4;

/// The names a scenario or a port wants in scope.
pub mod prelude {
    pub use crate::kernel::{Kernel, StartHandles, TaskState};
    pub use crate::name::Name;
    pub use crate::queue::{Blocked, Position, Ready, Wait};
    pub use crate::{items_for, lists_for};
    pub use rusty_rtos_core::prelude::*;
}

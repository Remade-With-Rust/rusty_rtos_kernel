#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
//! `rusty_rtos_kernel` — FreeRTOS-Kernel remade in Rust: the fixed-priority preemptive scheduler, task notifications, queues, semaphores, mutexes with priority inheritance, queue sets, software timers, event groups, stream and message buffers — a pure state machine over the Port seam, forbid(unsafe), traced against the C kernel.
//!
//! This is the facade: it re-exports the `no_std` core. Depend on this crate;
//! reach into the sub-crates only when you are building a port or a backend.
//!
//! Part of Kairos (Remade With Rust). Plan: `docs/plans/rusty_rtos_kernel.md`.

pub use rusty_rtos_kernel_core::*;

/// The names a firmware wants in scope.
pub mod prelude {
    pub use rusty_rtos_kernel_core::prelude::*;
}

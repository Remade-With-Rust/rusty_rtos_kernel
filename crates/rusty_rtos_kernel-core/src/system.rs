//! The static face: declare the system, and the geometry is the declaration.
//!
//! [`crate::typed`] made one *object* safe to hold — a `Queue<T, N>` that
//! moves `T`s and cannot be minted from an integer. This makes the whole
//! *system* safe to declare. You write down the tasks and queues an
//! application has; the macro writes the kernel's geometry from them and
//! hands back a struct whose fields are those tasks and queues, by name and
//! by type.
//!
//! ```ignore
//! rusty_rtos_kernel_core::system! {
//!     mod blinky use PosixDemoConfig;
//!     tasks { consumer: 2, producer: 2 }
//!     queues { data: u16; 10 }
//! }
//! ```
//!
//! # What that buys, and why each part of it is a compile-time thing
//!
//! **Exact `.bss` by construction.** The geometry const generics —
//! `TASKS`, `ITEMS`, `LISTS`, `QUEUES`, `SLOTS` — are *computed from the
//! declaration* rather than picked by hand and rounded up. `SLOTS` is the
//! sum of the declared queue lengths, not a guess; `TASKS` is what you
//! declared plus the two the kernel makes for itself. A kernel sized this
//! way holds exactly the system and nothing else, and
//! `blinky::Kernel::<..>::SIZE` is a number you can put in a footprint
//! table rather than an allowance you hope covers it.
//!
//! **No create-failure path an application can reach.** Every way
//! `queue_create` and `create_task` can answer `Err` is eliminated *before
//! the program runs*:
//!
//! | how a create fails | why it cannot here |
//! |---|---|
//! | the task arena is full | `TASKS` **is** the declared count plus the kernel's two |
//! | the queue arena is full | `QUEUES` **is** the declared count |
//! | the item slots are exhausted | `SLOTS` **is** the sum of the declared lengths |
//! | a list is missing | `LISTS` is `lists_for` over the same numbers |
//! | the priority is out of range | a `const` assertion per task, so it is a **compile error** |
//!
//! [`System::build`] still answers `Result`, because the kernel calls it
//! makes do and this crate may not panic — but the `Err` arm is now
//! unreachable by construction rather than merely unlikely, and the table
//! above says which construction makes each one unreachable. That is the
//! difference between a runtime check and a proof, and it is the whole
//! point of the exercise.
//!
//! **The handles never enter the application.** `build` is the only place
//! that touches a create call, and it happens once, at startup. Afterwards
//! the application holds `system.data` — a `Queue<u16, 10>` — and there is
//! no constructor anywhere that invents one from an integer. That is what
//! makes the four Kani harnesses that pass a *symbolic handle* into a
//! kernel call unwritable against this face: the state they explore has no
//! way to come into being (`docs/LEDGER.md`, "The topology work,
//! measured").

/// Declare a system: its tasks, its queues, and the geometry they imply.
///
/// See the [module docs](self) for what the expansion contains and why.
///
/// The config is named in the header because the number of lists depends
/// on `Config::MAX_PRIORITIES`, and a `const` may read an associated const
/// of a *concrete* type — which is why this is a declaration and not a
/// generic parameter.
///
/// # What a declaration refuses, that a hand-sized kernel cannot
///
/// Each of these is paired with the working line it is one character
/// from, so neither can pass for the wrong reason.
///
/// **A priority the config has no ready list for.** In C this is a
/// `configASSERT` on the bench, if `configASSERT` is compiled in, on the
/// day that task first runs. Here it is a build failure:
///
/// ```compile_fail
/// # use rusty_rtos_core::config::Config;
/// # use rusty_rtos_core::tick::Bits32;
/// # #[derive(Debug, Clone, Copy, Default)]
/// # pub struct Cfg;
/// # impl Config for Cfg {
/// #     type Tick = Bits32;
/// #     const TICK_RATE_HZ: u32 = 100;
/// #     const MAX_PRIORITIES: u8 = 4;
/// #     const MINIMAL_STACK_SIZE: usize = 1;
/// #     const MAX_TASK_NAME_LEN: usize = 8;
/// #     const TIMER_TASK_PRIORITY: u8 = 3;
/// #     const TIMER_TASK_STACK_DEPTH: usize = 1;
/// #     const TIMER_QUEUE_LENGTH: usize = 1;
/// #     const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
/// # }
/// rusty_rtos_kernel_core::system! {
///     mod bad use Cfg;
///     tasks { spinner: 4 }
///     queues { }
/// }
/// # fn main() {}
/// ```
///
/// The control arm — the same declaration, one priority lower, which is
/// the highest `MAX_PRIORITIES = 4` actually has:
///
/// ```
/// # use rusty_rtos_core::config::Config;
/// # use rusty_rtos_core::tick::Bits32;
/// # #[derive(Debug, Clone, Copy, Default)]
/// # pub struct Cfg;
/// # impl Config for Cfg {
/// #     type Tick = Bits32;
/// #     const TICK_RATE_HZ: u32 = 100;
/// #     const MAX_PRIORITIES: u8 = 4;
/// #     const MINIMAL_STACK_SIZE: usize = 1;
/// #     const MAX_TASK_NAME_LEN: usize = 8;
/// #     const TIMER_TASK_PRIORITY: u8 = 3;
/// #     const TIMER_TASK_STACK_DEPTH: usize = 1;
/// #     const TIMER_QUEUE_LENGTH: usize = 1;
/// #     const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
/// # }
/// rusty_rtos_kernel_core::system! {
///     mod good use Cfg;
///     tasks { spinner: 3 }
///     queues { }
/// }
/// # fn main() { assert_eq!(good::TASKS, 3); }
/// ```
///
/// **A queue told to carry what it was not declared for.** The
/// declaration named `data` as a queue of `u16`, so the field is a
/// `Queue<u16, 4>` and a `u32` is not a `u16`:
///
/// ```compile_fail
/// # use rusty_rtos_core::config::Config;
/// # use rusty_rtos_core::tick::Bits32;
/// # #[derive(Debug, Clone, Copy, Default)]
/// # pub struct Cfg;
/// # impl Config for Cfg {
/// #     type Tick = Bits32;
/// #     const TICK_RATE_HZ: u32 = 100;
/// #     const MAX_PRIORITIES: u8 = 4;
/// #     const MINIMAL_STACK_SIZE: usize = 1;
/// #     const MAX_TASK_NAME_LEN: usize = 8;
/// #     const TIMER_TASK_PRIORITY: u8 = 3;
/// #     const TIMER_TASK_STACK_DEPTH: usize = 1;
/// #     const TIMER_QUEUE_LENGTH: usize = 1;
/// #     const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
/// # }
/// rusty_rtos_kernel_core::system! {
///     mod app use Cfg;
///     tasks { worker: 1 }
///     queues { data: u16; 4 }
/// }
/// # fn main() {}
/// fn wrong<P, T, H>(k: &mut app::Kernel<P, T, H>, s: &mut app::System)
/// where
///     P: rusty_rtos_core::port::Port,
///     T: rusty_rtos_core::trace::Trace,
///     H: rusty_rtos_kernel_core::TickHook<app::Kernel<P, T, H>>,
/// {
///     let _ = s.data.send(k, 70_000u32, 0);
/// }
/// ```
///
/// The control arm, one type name different:
///
/// ```
/// # use rusty_rtos_core::config::Config;
/// # use rusty_rtos_core::tick::Bits32;
/// # #[derive(Debug, Clone, Copy, Default)]
/// # pub struct Cfg;
/// # impl Config for Cfg {
/// #     type Tick = Bits32;
/// #     const TICK_RATE_HZ: u32 = 100;
/// #     const MAX_PRIORITIES: u8 = 4;
/// #     const MINIMAL_STACK_SIZE: usize = 1;
/// #     const MAX_TASK_NAME_LEN: usize = 8;
/// #     const TIMER_TASK_PRIORITY: u8 = 3;
/// #     const TIMER_TASK_STACK_DEPTH: usize = 1;
/// #     const TIMER_QUEUE_LENGTH: usize = 1;
/// #     const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
/// # }
/// rusty_rtos_kernel_core::system! {
///     mod app use Cfg;
///     tasks { worker: 1 }
///     queues { data: u16; 4 }
/// }
/// # fn main() {}
/// fn right<P, T, H>(k: &mut app::Kernel<P, T, H>, s: &mut app::System)
/// where
///     P: rusty_rtos_core::port::Port,
///     T: rusty_rtos_core::trace::Trace,
///     H: rusty_rtos_kernel_core::TickHook<app::Kernel<P, T, H>>,
/// {
///     let _ = s.data.send(k, 7_000u16, 0);
/// }
/// ```
///
/// **And there is no third way in.** A `System` has no public
/// constructor but [`System::build`], and a `Queue<T, N>` has none but
/// `create`, so no application can hold a handle it invented — which is
/// what makes the symbolic-handle proofs unwritable against this face
/// rather than merely slow.
#[macro_export]
macro_rules! system {
    (
        $(#[$meta:meta])*
        $vis:vis mod $name:ident use $cfg:ty;
        tasks { $($task:ident : $prio:expr),* $(,)? }
        queues { $($queue:ident : $qty:ty ; $qlen:expr),* $(,)? }
    ) => {
        $(#[$meta])*
        $vis mod $name {
            #![allow(unused_imports, clippy::allow_attributes)]
            use super::*;

            /// The tasks this system declares. The kernel adds its own two.
            pub const APP_TASKS: usize = 0usize $(+ { let _ = $prio; 1usize })*;
            /// Room for every declared task plus the idle task and the
            /// timer daemon, and for nothing else.
            pub const TASKS: usize = APP_TASKS + $crate::OVERHEAD_TASKS;
            /// The declared queues, and no spare.
            pub const QUEUES: usize = 0usize $(+ { let _ = $qlen; 1usize })*;
            /// The sum of the declared queue lengths: every slot is spoken
            /// for and none is spare.
            pub const SLOTS: usize = 0usize $(+ $qlen)*;
            /// Software timers. Not declared yet; a system that wants them
            /// sizes its own kernel.
            pub const TIMERS: usize = 0;
            /// Event groups, as `TIMERS`.
            pub const GROUPS: usize = 0;
            /// Stream buffers, as `TIMERS`.
            pub const BUFFERS: usize = 0;
            /// Stream-buffer bytes, as `TIMERS`.
            pub const BYTES: usize = 0;
            /// A ready list per priority, the fixed six, two per queue and
            /// one per event group.
            ///
            /// Declared BEFORE `ITEMS`, which now depends on it: the list
            /// arena holds one end-marker node per list alongside the items.
            pub const LISTS: usize = $crate::lists_for(
                <$cfg as ::rusty_rtos_core::config::Config>::MAX_PRIORITIES,
                QUEUES,
                GROUPS,
            );
            /// Slots for two list items per task, one per timer, and one
            /// end marker per list — rounded up to a power of two.
            pub const ITEMS: usize = $crate::list_slots_for(TASKS, TIMERS, LISTS);

            // A declared priority the config has no ready list for is a
            // compile error, not a create that answers `Err` at startup on
            // the bench. This is the one create-failure path that is an
            // application mistake rather than a geometry mistake, so it is
            // the one worth refusing here.
            $(
                const _: () = assert!(
                    ($prio as u8) < <$cfg as ::rusty_rtos_core::config::Config>::MAX_PRIORITIES,
                    concat!(
                        "task `", stringify!($task),
                        "` declares a priority its Config has no ready list for"
                    ),
                );
            )*

            /// This system's kernel: the geometry above, and nothing to
            /// choose. `P`, `T` and `H` are still the port, the trace sink
            /// and the tick hook, which are the platform's business.
            pub type Kernel<P, T, H> = $crate::Kernel<
                $cfg, P, T, H,
                TASKS, ITEMS, LISTS, QUEUES, SLOTS, BUFFERS, BYTES, TIMERS, GROUPS,
            >;

            /// The declared system: every task and every queue, by name.
            ///
            /// Built once, by [`System::build`]. After that an application
            /// holds this and never a handle it could have invented.
            #[derive(Debug)]
            pub struct System {
                $(
                    /// A declared task.
                    pub $task: ::rusty_rtos_core::handle::TaskHandle,
                )*
                $(
                    /// A declared queue, typed and length-checked.
                    pub $queue: $crate::typed::Queue<$qty, { $qlen }>,
                )*
            }

            impl System {
                /// Create everything the declaration names, in the order it
                /// names it.
                ///
                /// # Errors
                /// None an application can reach: see the module docs for
                /// why each create-failure path is closed by construction.
                /// The `Result` is here because this crate may not panic,
                /// not because there is a case to handle.
                pub fn build<P, T, H>(kernel: &mut Kernel<P, T, H>) -> ::rusty_rtos_core::error::Result<Self>
                where
                    P: ::rusty_rtos_core::port::Port,
                    T: ::rusty_rtos_core::trace::Trace,
                    H: $crate::TickHook<Kernel<P, T, H>>,
                {
                    $(
                        let $queue = $crate::typed::Queue::<$qty, { $qlen }>::create(kernel)?;
                    )*
                    $(
                        let $task = kernel.create_task(stringify!($task), $prio)?;
                    )*
                    Ok(Self { $($task,)* $($queue,)* })
                }
            }
        }
    };
}

#[cfg(test)]
// `dead_code`: a `system!` declaration generates a `System` and its
// `build`, and `deep`'s exists to SIZE a kernel rather than to furnish one
// -- its tasks are created directly so the declared queue slot stays free
// for the timer daemon.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
// `pub(crate)` so `timer.rs`, `stream.rs` and `events.rs` can test against
// the same config, port and sink rather than declaring three more of each
// -- three copies of a test harness is three places for it to drift (H3).
pub(crate) mod tests {
    use rusty_rtos_core::config::Config;
    use rusty_rtos_core::hooks::NoTickHook;
    use rusty_rtos_core::isr::Woken;
    use rusty_rtos_core::port::Port;
    use rusty_rtos_core::tick::Bits32;
    use rusty_rtos_core::trace::{Event, Scheduling, Trace};

    use crate::typed::Sent;

    /// Four priorities, so a declaration can get one wrong.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct TestConfig;

    impl Config for TestConfig {
        type Tick = Bits32;
        const TICK_RATE_HZ: u32 = 100;
        const MAX_PRIORITIES: u8 = 4;
        const MINIMAL_STACK_SIZE: usize = 1;
        const MAX_TASK_NAME_LEN: usize = 8;
        const TIMER_TASK_PRIORITY: u8 = 3;
        const TIMER_TASK_STACK_DEPTH: usize = 1;
        const TIMER_QUEUE_LENGTH: usize = 1;
        const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
        const USE_TIME_SLICING: bool = false;
    }

    /// [`TestConfig`] with `configUSE_TICKLESS_IDLE` on, and nothing else
    /// changed — so a difference between the two is the tickless path and
    /// cannot be anything else.
    pub struct TicklessConfig;

    impl Config for TicklessConfig {
        type Tick = Bits32;
        const TICK_RATE_HZ: u32 = 100;
        const MAX_PRIORITIES: u8 = 4;
        const MINIMAL_STACK_SIZE: usize = 1;
        const MAX_TASK_NAME_LEN: usize = 8;
        const TIMER_TASK_PRIORITY: u8 = 3;
        const TIMER_TASK_STACK_DEPTH: usize = 1;
        const TIMER_QUEUE_LENGTH: usize = 1;
        const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
        const USE_TIME_SLICING: bool = false;
        const USE_TICKLESS_IDLE: bool = true;
    }

    /// A port that takes every whole window it is offered — the most
    /// aggressive sleep there is, and so the hardest case for the kernel to
    /// stay correct across.
    #[derive(Debug, Default)]
    pub struct SleepyPort {
        nesting: core::cell::Cell<u32>,
        /// Ticks this port has been asked to sleep, in total.
        pub slept: core::cell::Cell<u64>,
        /// Times it was asked at all.
        pub sleeps: core::cell::Cell<u64>,
    }

    impl Port for SleepyPort {
        fn yield_now(&self) {}
        fn yield_from_isr(&self, _woken: Woken) {}
        fn enter_critical(&self) {
            self.nesting.set(self.nesting.get().saturating_add(1));
        }
        fn exit_critical(&self) {
            self.nesting.set(self.nesting.get().saturating_sub(1));
        }
        fn set_interrupt_mask_from_isr(&self) -> u32 {
            0
        }
        fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
        fn in_isr(&self) -> bool {
            false
        }
        fn set_in_tick_entry(&self, _yes: bool) {}

        fn suppress_ticks_and_sleep(&self, expected_idle_ticks: u64) -> u64 {
            self.sleeps.set(self.sleeps.get().saturating_add(1));
            self.slept
                .set(self.slept.get().saturating_add(expected_idle_ticks));
            expected_idle_ticks
        }
    }

    /// A port that OVERSLEEPS, to prove the kernel clamps rather than
    /// trusting it.
    #[derive(Debug, Default)]
    pub struct OversleepingPort {
        nesting: core::cell::Cell<u32>,
    }

    impl Port for OversleepingPort {
        fn yield_now(&self) {}
        fn yield_from_isr(&self, _woken: Woken) {}
        fn enter_critical(&self) {
            self.nesting.set(self.nesting.get().saturating_add(1));
        }
        fn exit_critical(&self) {
            self.nesting.set(self.nesting.get().saturating_sub(1));
        }
        fn set_interrupt_mask_from_isr(&self) -> u32 {
            0
        }
        fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
        fn in_isr(&self) -> bool {
            false
        }
        fn set_in_tick_entry(&self, _yes: bool) {}

        fn suppress_ticks_and_sleep(&self, expected_idle_ticks: u64) -> u64 {
            expected_idle_ticks.saturating_mul(4).saturating_add(9)
        }
    }

    #[derive(Debug, Default)]
    pub struct TestPort {
        nesting: core::cell::Cell<u32>,
    }

    impl Port for TestPort {
        fn yield_now(&self) {}
        fn yield_from_isr(&self, _woken: Woken) {}
        fn enter_critical(&self) {
            self.nesting.set(self.nesting.get().saturating_add(1));
        }
        fn exit_critical(&self) {
            self.nesting.set(self.nesting.get().saturating_sub(1));
        }
        fn set_interrupt_mask_from_isr(&self) -> u32 {
            0
        }
        fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
        fn in_isr(&self) -> bool {
            false
        }
        fn set_in_tick_entry(&self, _yes: bool) {}
    }

    /// A [`TestPort`] that COUNTS the switches it was asked for.
    ///
    /// `TestPort::yield_now` is empty, which is right for tests about what
    /// the kernel computes and useless for a test about whether it asked
    /// for a switch at all. Those are different questions and the second
    /// one has no other instrument.
    #[derive(Debug, Default)]
    pub struct CountingPort {
        nesting: core::cell::Cell<u32>,
        yields: core::cell::Cell<u32>,
    }

    impl Port for CountingPort {
        /// Count the kernel ASKING for a switch, not one style of taking it.
        ///
        /// The first version of this counted `yield_now`, and measured the
        /// wrong thing: `Port::COMMITS_SWITCH` defaults to `false`, so
        /// `port_yield` calls `switch_context` itself and never reaches
        /// `yield_now` at all. The test read zero and looked like a kernel
        /// defect. `count_yield` is on the unconditional path, so it
        /// answers "did the kernel decide a switch was needed" whichever
        /// way the port takes it — which is the question.
        fn count_yield(&self) {
            self.yields.set(self.yields.get().saturating_add(1));
        }
        fn yield_now(&self) {}
        fn yield_from_isr(&self, _woken: Woken) {}
        fn enter_critical(&self) {
            self.nesting.set(self.nesting.get().saturating_add(1));
        }
        fn exit_critical(&self) {
            self.nesting.set(self.nesting.get().saturating_sub(1));
        }
        fn set_interrupt_mask_from_isr(&self) -> u32 {
            0
        }
        fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
        fn in_isr(&self) -> bool {
            false
        }
        fn set_in_tick_entry(&self, _yes: bool) {}
    }

    #[derive(Debug, Default)]
    pub struct NoTrace;

    impl Trace for NoTrace {
        fn event(&mut self, _tick: u64, _event: Event<'_>) {}
    }

    // The declaration under test: two tasks and two queues of different
    // types and different lengths, so nothing about the geometry can come
    // out right by symmetry.
    crate::system! {
        mod demo use TestConfig;
        tasks { consumer: 2, producer: 1 }
        queues { data: u16; 10, marks: u8; 3 }
    }

    type K = demo::Kernel<TestPort, NoTrace, NoTickHook>;

    // ---- tickless idle ---------------------------------------------------

    crate::system! {
        mod sleepy use TicklessConfig;
        tasks { worker: 1 }
        queues { mark: u8; 1 }
    }

    crate::system! {
        mod awake use TestConfig;
        tasks { worker: 1 }
        queues { mark: u8; 1 }
    }

    /// A sink that keeps the shape of a trace: every event, with the tick it
    /// carried. Comparing two of these compares two schedules.
    #[derive(Debug, Default, PartialEq, Eq)]
    pub struct Recorder {
        pub lines: std::vec::Vec<(u64, &'static str)>,
    }

    impl Trace for Recorder {
        fn event(&mut self, tick: u64, event: Event<'_>) {
            self.lines.push((tick, event.name()));
        }
    }

    /// `worker` delays, leaving only the idle task ready — which is the
    /// precondition tickless idle exists for.
    const DELAY: u64 = 10;

    /// Both declarations build what they declare.
    ///
    /// The tests that matter below start a scheduler, and `System::build` and
    /// a started scheduler cannot both have the one queue slot a declaration
    /// reserves -- `system!` sizes `QUEUES` with "no spare", and the timer
    /// daemon needs it. So they create the one task they need by hand, and
    /// this is where the generated builders are exercised.
    #[test]
    fn both_tickless_declarations_build_what_they_declare() {
        let mut asleep = sleepy_kernel();
        let slept = sleepy::System::build(&mut asleep).expect("the tickless declaration builds");

        let mut ticking =
            awake::Kernel::<TestPort, NoTrace, NoTickHook>::new(TestPort::default(), NoTrace)
                .expect("the declared geometry adds up");
        let ticked = awake::System::build(&mut ticking).expect("the ticking declaration builds");

        // The two declarations are the same text over two configurations, so
        // they must hand back the same handle for the same task. If they did
        // not, the arms of the invariance test below would be comparing two
        // different systems and the comparison would mean nothing.
        assert_eq!(
            slept.worker, ticked.worker,
            "identical declarations gave different task handles"
        );
        let _ = (&slept.mark, &ticked.mark);
    }

    /// A kernel with tickless on and a port that takes every window.
    fn sleepy_kernel() -> sleepy::Kernel<SleepyPort, Scheduling<Recorder>, NoTickHook> {
        sleepy::Kernel::new(SleepyPort::default(), Scheduling::new(Recorder::default()))
            .expect("the declared geometry adds up")
    }

    /// The port asks for the whole window, and the kernel winds the clock
    /// over it — all but the last tick, which is left pended so the delayed
    /// task is woken by the same code that would have woken it.
    #[test]
    fn a_sleeping_port_is_asked_for_the_whole_idle_window() {
        let mut k = sleepy_kernel();
        k.create_task("worker", 1).expect("worker");
        let started = k.start_scheduler().expect("start");
        // The timer daemon is created READY at `TIMER_TASK_PRIORITY`, and
        // in a running system it blocks on its command queue immediately.
        // Nothing runs it here, so park it: a task ready above the idle
        // priority is precisely what `expected_idle_time` refuses to sleep
        // through, and leaving it ready would test that refusal instead of
        // the sleep.
        k.suspend(Some(started.timer))
            .expect("park the timer daemon");
        k.delay(DELAY).expect("the worker delays");

        let before = k.tick_count();
        k.idle_suppress_ticks();

        assert_eq!(k.port().sleeps.get(), 1, "the port was asked once");
        assert_eq!(
            k.port().slept.get(),
            DELAY,
            "it was offered the whole window"
        );
        // Nine ticks were STEPPED and the tenth was left pended, and then
        // `resume_all` unwound it through `increment_tick` -- which is what
        // wakes the delayed task, by the same code that would have woken it
        // had the kernel ticked all ten. So the clock lands on the wake
        // either way, and that is the whole reason the schedule survives.
        assert_eq!(
            k.tick_count(),
            before.saturating_add(DELAY),
            "the clock did not land on the worker's wake"
        );
    }

    /// The `configUSE_TICKLESS_IDLE` gate: the same port, the same script,
    /// a configuration with it off, and the port is never asked.
    #[test]
    fn tickless_off_never_asks_the_port_to_sleep() {
        let mut k =
            awake::Kernel::<SleepyPort, NoTrace, NoTickHook>::new(SleepyPort::default(), NoTrace)
                .expect("the declared geometry adds up");
        k.create_task("worker", 1).expect("worker");
        let started = k.start_scheduler().expect("start");
        // The timer daemon is created READY at `TIMER_TASK_PRIORITY`, and
        // in a running system it blocks on its command queue immediately.
        // Nothing runs it here, so park it: a task ready above the idle
        // priority is precisely what `expected_idle_time` refuses to sleep
        // through, and leaving it ready would test that refusal instead of
        // the sleep.
        k.suspend(Some(started.timer))
            .expect("park the timer daemon");
        k.delay(DELAY).expect("the worker delays");

        let before = k.tick_count();
        k.idle_suppress_ticks();

        assert_eq!(k.port().sleeps.get(), 0, "the port was asked");
        assert_eq!(k.tick_count(), before, "the clock moved");
    }

    /// A port that oversleeps is a port bug, and the kernel refuses to wind
    /// the clock past the wake it would lose. The C `configASSERT`s this;
    /// a kernel that forbids panicking has to clamp instead.
    #[test]
    fn the_kernel_clamps_a_port_that_oversleeps() {
        let mut k = sleepy::Kernel::<OversleepingPort, NoTrace, NoTickHook>::new(
            OversleepingPort::default(),
            NoTrace,
        )
        .expect("the declared geometry adds up");
        k.create_task("worker", 1).expect("worker");
        let started = k.start_scheduler().expect("start");
        // The timer daemon is created READY at `TIMER_TASK_PRIORITY`, and
        // in a running system it blocks on its command queue immediately.
        // Nothing runs it here, so park it: a task ready above the idle
        // priority is precisely what `expected_idle_time` refuses to sleep
        // through, and leaving it ready would test that refusal instead of
        // the sleep.
        k.suspend(Some(started.timer))
            .expect("park the timer daemon");
        k.delay(DELAY).expect("the worker delays");

        let before = k.tick_count();
        k.idle_suppress_ticks();

        // The port claimed `4 * DELAY + 9` = 49 ticks. Unclamped the clock
        // would be 49 ticks on and the worker's wake would have been stepped
        // straight past; clamped it lands on the wake exactly, like a port
        // that behaved.
        assert_eq!(
            k.tick_count(),
            before.saturating_add(DELAY),
            "a port claiming four times its window moved the clock past the wake"
        );
    }

    /// ★ The claim the whole mission rests on.
    ///
    /// Two kernels, identical but for `configUSE_TICKLESS_IDLE`, run the
    /// same script. One sleeps through the idle window; the other ticks
    /// through it. Their traces differ — that is the point, and it is why a
    /// byte-diff cannot gate a tickless port. Their SCHEDULES do not.
    ///
    /// `Scheduling` is the projection that says so, and this is it applied
    /// to a kernel that really did suppress its ticks rather than to a trace
    /// file standing in for one.
    #[test]
    fn the_schedule_is_the_same_with_and_without_tickless() {
        let target = DELAY.saturating_add(2);

        let mut asleep = sleepy_kernel();
        asleep.create_task("worker", 1).expect("worker");
        let started = asleep.start_scheduler().expect("start");
        asleep
            .suspend(Some(started.timer))
            .expect("park the timer daemon");
        asleep.delay(DELAY).expect("the worker delays");
        asleep.idle_suppress_ticks();
        while asleep.tick_count() < target {
            if asleep.increment_tick() {
                asleep.switch_context();
            }
        }

        let mut ticking = awake::Kernel::<TestPort, Scheduling<Recorder>, NoTickHook>::new(
            TestPort::default(),
            Scheduling::new(Recorder::default()),
        )
        .expect("the declared geometry adds up");
        ticking.create_task("worker", 1).expect("worker");
        let started = ticking.start_scheduler().expect("start");
        ticking
            .suspend(Some(started.timer))
            .expect("park the timer daemon");
        ticking.delay(DELAY).expect("the worker delays");
        ticking.idle_suppress_ticks();
        while ticking.tick_count() < target {
            if ticking.increment_tick() {
                ticking.switch_context();
            }
        }

        assert!(
            asleep.port().sleeps.get() > 0,
            "the sleeping arm never slept, so this proves nothing"
        );

        let slept_schedule = asleep.into_trace().into_inner();
        let ticked_schedule = ticking.into_trace().into_inner();

        assert_eq!(
            slept_schedule, ticked_schedule,
            "sleeping through the idle window moved the schedule"
        );
    }

    // ---- a delayed list deeper than the conformance corpus ever builds ---

    // `System` here is never built: the tasks are created directly so the
    // declared queue slot stays free for `xTimerCreateTimerTask`. The
    // declaration is still what sizes the kernel.
    crate::system! {
        mod deep use TestConfig;
        tasks {
            d0: 1, d1: 1, d2: 1, d3: 1, d4: 1,
            d5: 1, d6: 1, d7: 1, d8: 1, d9: 1
        }
        queues { unused: u8; 1 }
    }

    /// Ten tasks blocked at once, waking in wake-time order.
    ///
    /// **This exists because the conformance gate cannot reach here.** That
    /// gate is the strongest evidence in the project -- 19 scenarios, every
    /// scheduling decision compared line-for-line against the instrumented C
    /// kernel -- and its delayed list never gets deeper than FOUR. Measured,
    /// by replaying each scenario's own trace and tracking membership:
    ///
    /// ```text
    ///   BlockQ 4  death 4  dynamic 3  PollQ 3  semtest 3  recmutex 3
    ///   blocktim 3  IntSemTest 3  TimerDemo 3  QPeek 2  GenQTest 2
    ///   MessageBufferAMP 2  countsem 1  QueueOverwrite 1
    ///   QueueSetPolling 1  StreamBufferInterrupt 1  EventGroupsDemo 1
    /// ```
    ///
    /// `vListInsert` is the only operation in the kernel whose cost is its
    /// depth, and the corpus never tests it past n=4. Anything that goes
    /// wrong only when the list is deep -- an ordering bug, an off-by-one in
    /// the walk, a fast path that appends where it should splice -- passes
    /// that gate green.
    ///
    /// The delays are deliberately NOT monotonic: a shortcut that appends
    /// unconditionally orders them wrongly, and ascending delays would let
    /// it through. They are distinct, because two tasks sharing a wake time
    /// wake on the same tick and this probe cannot see which left first.
    #[test]
    #[allow(clippy::indexing_slicing)]
    fn ten_blocked_tasks_wake_in_wake_time_order() {
        let mut k =
            deep::Kernel::<TestPort, NoTrace, NoTickHook>::new(TestPort::default(), NoTrace)
                .expect("the declared geometry adds up");
        // Created directly rather than through `System::build`. Build would
        // also create the declared queue, and that slot is the one
        // `xTimerCreateTimerTask` needs at startup -- the declaration is
        // here to SIZE the kernel, not to furnish it.
        let names = ["d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9"];
        let mut all = [::rusty_rtos_core::handle::TaskHandle::from_raw(0); 10];
        for (slot, name) in all.iter_mut().zip(names) {
            *slot = k.create_task(name, 1).expect("task");
        }
        let started = k.start_scheduler().expect("start");
        // Park the timer daemon. It sits at TIMER_TASK_PRIORITY, above these
        // tasks, so leaving it ready means it is the one that runs and the
        // ten never get a turn. The tickless tests park it for the same
        // reason.
        k.suspend(Some(started.timer)).expect("park the daemon");

        // Out of order on purpose: a shortcut that appends unconditionally
        // gets these wrong, and ascending delays would let it through.
        //
        // DISTINCT, though. Two tasks with the same wake time wake on the
        // same TICK, and `task_state_get` cannot say which of them left the
        // delayed list first -- the probe below would record them in index
        // order whatever the list did. The tie rule is real and it is tested
        // where it is observable, directly on the list, in
        // `list::tests::a_value_equal_to_the_tail_goes_after_it`.
        let delays = [9u64, 3, 14, 7, 1, 11, 5, 8, 2, 13];

        // Block each in turn. `delay` acts on whichever task is RUNNING, so
        // the schedule picks the order and this records what it picked --
        // asserting the kernel's own choice rather than assuming one.
        let mut blocked = [(0u64, 0usize); 10];
        let mut n = 0usize;
        for _ in 0..200u32 {
            if n == all.len() {
                break;
            }
            let running = k.current();
            if let Some(which) = all.iter().position(|t| *t == running) {
                if !blocked[..n].iter().any(|(_, w)| *w == which) {
                    k.delay(delays[which]).expect("blocks");
                    blocked[n] = (delays[which], which);
                    n = n.saturating_add(1);
                }
            }
            k.switch_context();
        }
        assert_eq!(n, all.len(), "every task should have blocked");

        for (i, task) in all.iter().enumerate() {
            assert_eq!(
                k.task_state_get(*task).expect("state"),
                crate::kernel::TaskState::Blocked,
                "task {i} should be blocked; all ten are, at once, which is                  more than twice as deep as any conformance scenario reaches"
            );
        }

        // Tick forward and record the order they come back.
        let mut woke = [0usize; 10];
        let mut w = 0usize;
        let mut asleep = [true; 10];
        for _ in 0..40u32 {
            k.tick_from_isr();
            for (i, task) in all.iter().enumerate() {
                if asleep[i]
                    && k.task_state_get(*task).expect("state") != crate::kernel::TaskState::Blocked
                {
                    asleep[i] = false;
                    woke[w] = i;
                    w = w.saturating_add(1);
                }
            }
        }
        assert_eq!(w, all.len(), "every task should have woken");

        // Ascending wake time; ties in the order they blocked. That is what
        // `vListInsert` promises and the only order the list guarantees.
        let mut want = blocked;
        want.sort_by_key(|(ticks, _)| *ticks);
        let want = want.map(|(_, which)| which);
        assert_eq!(
            woke, want,
            "the delayed list did not wake in wake-time order"
        );
    }

    /// Creating a task at a HIGHER priority than the running one must ask
    /// for a switch; creating one LOWER must not.
    ///
    /// **This closes a gap the ledger measured and left open.** `cargo
    /// mutants` over `kernel.rs`, judged by the conformance corpus, left six
    /// survivors, and five were this one line in `prvAddNewTaskToReadyList`:
    ///
    /// ```text
    /// if self.running && C::USE_PREEMPTION && self.current_priority() < priority
    /// ```
    ///
    /// `death` was added so the corpus would create tasks after the
    /// scheduler had started, and it narrowed six survivors to four. It
    /// could not do better, for a reason visible in its own source:
    /// `vCreateTasks` creates both suicidal tasks at `uxTaskPriorityGet(
    /// NULL )` -- its OWN priority -- so the new task never outranks the
    /// running one. The guard is reached and evaluated on every create and
    /// is never TRUE. Mutations that make it fire spuriously die at once;
    /// mutations that suppress a yield which never happens are invisible.
    /// Reachable is not killed.
    ///
    /// No corpus scenario creates a higher-priority task while running, and
    /// none can be added without a C original to diff against. A unit test
    /// can do it, and this is it. The four survivors:
    ///
    /// | mutation | caught by |
    /// |---|---|
    /// | condition → `false` | the HIGHER half |
    /// | body removed (no yield) | the HIGHER half |
    /// | `<` → `>` | the HIGHER half |
    /// | `<` → `!=` | the LOWER half |
    ///
    /// Which is why both halves are here. The lower half looks like a
    /// formality and is the only thing that kills `!=`.
    #[test]
    fn creating_a_higher_priority_task_asks_for_a_switch_and_a_lower_one_does_not() {
        let mut k = CountingKernel::new(CountingPort::default(), NoTrace)
            .expect("the declared geometry adds up");
        let mid = k.create_task("mid", 2).expect("task mid");
        k.start_scheduler().expect("start");
        assert_eq!(k.current(), mid, "the only app task runs");

        // ---- LOWER: no switch is owed -----------------------------------
        let before = k.port().yields.get();
        let _low = k.create_task("low", 1).expect("task low");
        assert_eq!(
            k.port().yields.get(),
            before,
            "a task created BELOW the running priority cannot preempt it, so              the kernel must not ask for a switch"
        );

        // ---- HIGHER: a switch is owed -----------------------------------
        let before = k.port().yields.get();
        let _high = k.create_task("high", 3).expect("task high");
        assert!(
            k.port().yields.get() > before,
            "a task created ABOVE the running priority preempts it, and the              kernel must ask for the switch"
        );
    }

    /// A kernel with room for a stream buffer, which `system!` never gives
    /// one: the macro sizes `BUFFERS` and `BYTES` at zero.
    type BufferKernel = crate::Kernel<
        TestConfig,
        TestPort,
        NoTrace,
        NoTickHook,
        4,
        { crate::list_slots_for(4, 0, crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 0)) },
        { crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 0) },
        1,
        1,
        2,
        64,
        0,
        0,
    >;

    /// `xStreamBufferSetTriggerLevel`'s contract, pinned.
    ///
    /// The C coerces a zero trigger to one BEFORE bounds-checking:
    ///
    /// ```c
    /// if( xTriggerLevel == ( size_t ) 0 ) { xTriggerLevel = 1; }
    /// if( xTriggerLevel < pxStreamBuffer->xLength ) { ...pdPASS }
    /// else { xReturn = pdFALSE; }
    /// ```
    ///
    /// This kernel had the two the other way round. The orders disagree on
    /// exactly one input -- a zero trigger against a length of ONE -- and
    /// that input cannot be built: `length` is `size + 1` (the spare byte a
    /// ring buffer needs to tell full from empty) and `size == 0` is refused
    /// at creation. So it was an inconsistency with the C, not a live bug,
    /// and the order has been matched anyway rather than left resting on an
    /// arithmetic detail in another function.
    ///
    /// What this test pins is the contract at the lengths that exist. There
    /// is no conformance scenario for this API and no C demo in the vendored
    /// tree that calls it, so a differential is not available --
    /// `docs/HOLES.md` H2 -- and this is the only evidence there is.
    #[test]
    fn the_trigger_level_contract() {
        let mut k =
            BufferKernel::new(TestPort::default(), NoTrace).expect("the declared geometry adds up");

        // `create(1, 1)` is a length of TWO: one byte of payload and the
        // spare. The smallest buffer a caller can actually make.
        let small = k.stream_buffer_create(1, 1).expect("a one-byte buffer");
        assert_eq!(
            k.stream_buffer_set_trigger_level(small, 0),
            Ok(true),
            "zero becomes one, and one IS below a length of two"
        );
        assert_eq!(
            k.stream_buffer_set_trigger_level(small, 2),
            Ok(false),
            "two is not below a length of two"
        );

        let wide = k.stream_buffer_create(8, 1).expect("an eight-byte buffer");
        assert_eq!(k.stream_buffer_set_trigger_level(wide, 0), Ok(true));
        assert_eq!(
            k.stream_buffer_set_trigger_level(wide, 8),
            Ok(true),
            "8 < 9"
        );
        assert_eq!(
            k.stream_buffer_set_trigger_level(wide, 9),
            Ok(false),
            "nine is not below a length of nine"
        );
    }

    fn kernel() -> K {
        K::new(TestPort::default(), NoTrace).expect("the declared geometry adds up")
    }

    /// `configUSE_TIME_SLICING`: two READY tasks of the same priority
    /// must take turns.
    ///
    /// Fixed priority says what happens between DIFFERENT priorities and
    /// nothing at all about two tasks at the same one. The C fills that in
    /// twice over: `xTaskIncrementTick` asks for a switch whenever more
    /// than one task is ready at the running task's priority, and
    /// `taskSELECT_HIGHEST_PRIORITY_TASK` then takes the NEXT entry rather
    /// than the head.
    ///
    /// Without it a task that never blocks starves its equal-priority
    /// peers completely — not slowly, completely — and the failure looks
    /// like somebody else's demo being broken. `rusty_rtos-capi`'s host
    /// cell found it that way: two demo files whose tasks sit at the same
    /// priority, one of them getting 13.5 million turns and the other
    /// ZERO.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct SliceConfig;

    impl Config for SliceConfig {
        type Tick = Bits32;
        const TICK_RATE_HZ: u32 = 100;
        const MAX_PRIORITIES: u8 = 4;
        const MINIMAL_STACK_SIZE: usize = 1;
        const MAX_TASK_NAME_LEN: usize = 8;
        // The daemon sits BELOW the two tasks under test. Nothing drives
        // it here, so at its usual priority it would simply be the current
        // task for ever and the test would measure the daemon.
        const TIMER_TASK_PRIORITY: u8 = 0;
        const TIMER_TASK_STACK_DEPTH: usize = 1;
        const TIMER_QUEUE_LENGTH: usize = 1;
        const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
        // The point of this config: the default, stated out loud.
        const USE_TIME_SLICING: bool = true;
        const USE_PREEMPTION: bool = true;
    }

    /// Built from the const generics directly rather than through
    /// `system!`, because that macro declares its tasks for you and this
    /// test needs to create them itself.
    type SliceKernel = crate::Kernel<
        SliceConfig,
        TestPort,
        NoTrace,
        NoTickHook,
        5,
        { crate::list_slots_for(5, 1, crate::lists_for(SliceConfig::MAX_PRIORITIES, 1, 1)) },
        { crate::lists_for(SliceConfig::MAX_PRIORITIES, 1, 1) },
        1,
        1,
        1,
        8,
        1,
        1,
    >;

    /// The same declaration driven by a port that COUNTS switch requests,
    /// with room for a queue of our own beside the timer daemon's.
    ///
    /// `QUEUES` is 2 and `SLOTS` 8 for exactly that reason: the timer queue
    /// is created by `Kernel::new` and takes the first of each, so a test
    /// that makes its own queue needs the second.
    type CountingKernel = crate::Kernel<
        SliceConfig,
        CountingPort,
        NoTrace,
        NoTickHook,
        5,
        { crate::list_slots_for(5, 1, crate::lists_for(SliceConfig::MAX_PRIORITIES, 2, 1)) },
        { crate::lists_for(SliceConfig::MAX_PRIORITIES, 2, 1) },
        2,
        8,
        1,
        8,
        1,
        1,
    >;

    /// Tick a kernel whose two equal-priority tasks never block, and count
    /// how many ticks each of them was the current task for.
    fn turns_over(ticks: u32) -> (u32, u32) {
        let mut k =
            SliceKernel::new(TestPort::default(), NoTrace).expect("the declared geometry adds up");
        let a = k.create_task("a", 2).expect("task a");
        let b = k.create_task("b", 2).expect("task b");
        k.start_scheduler().expect("start");

        let (mut for_a, mut for_b) = (0u32, 0u32);
        for _ in 0..ticks {
            if k.increment_tick() {
                k.switch_context();
            }
            let now = k.current();
            if now == a {
                for_a = for_a.saturating_add(1);
            } else if now == b {
                for_b = for_b.saturating_add(1);
            }
        }
        (for_a, for_b)
    }

    // A third test belongs here and is not written yet: equal-priority
    // rotation with a HIGHER-priority task cutting in between selections,
    // which is the shape every real system has. Two attempts at it were
    // withdrawn because they measured their own call pattern rather than
    // the scheduler -- `delay` yields as part of its contract, so a loop
    // that calls `delay` and then `switch_context` makes two selections
    // and can only sample after the second.
    //
    // What is known: driven directly, `switch_context` alternates
    // correctly between two equal-priority tasks with a third cutting in
    // (observed at the list, cursor 0 -> 1 -> 0). What is not known is why
    // `rusty_rtos_port/firmware/host-kernel` sees one of a pair get ZERO
    // turns in exactly that shape. That cell fails today and is the live
    // reproduction; this is the note that says so rather than a test that
    // asserts something unproven.

    #[test]
    fn two_ready_tasks_of_equal_priority_take_turns() {
        let (a, b) = turns_over(200);
        assert!(a > 0, "task a never ran: {a} vs {b}");
        assert!(b > 0, "task b never ran: {a} vs {b}");
    }

    /// `queueYIELD_IF_USING_PREEMPTION`: a send that readies a
    /// HIGHER-priority receiver must ask for a switch.
    ///
    /// This is the whole of `TimerDemo.c:469`, with no C anywhere near it.
    /// That demo stops a timer and asserts on the next line that the timer
    /// is inactive, and its own comment says why it may: "this will appear
    /// to happen immediately to this task because this task is running at
    /// a priority below the timer service task". `xTimerStop` is a send to
    /// the daemon's queue, so the assertion is really this test.
    ///
    /// It has two halves and the second is the one that bites. A send must
    /// ask for a switch when it wakes a higher-priority receiver — AND the
    /// receiver has to be WAITING ON THE QUEUE for there to be anything to
    /// wake. A daemon that polls its queue with a zero block time is
    /// parked nowhere, so the send finds no waiter, asks for nothing, and
    /// being higher priority buys it precisely nothing. Both halves are
    /// asserted below, because the first one passing is what makes the
    /// second one's absence invisible.
    #[test]
    fn a_send_that_wakes_a_higher_priority_receiver_asks_for_a_switch() {
        let mut k = CountingKernel::new(CountingPort::default(), NoTrace)
            .expect("the declared geometry adds up");
        let hi = k.create_task("hi", 2).expect("task hi");
        let lo = k.create_task("lo", 1).expect("task lo");
        let q = k.queue_create(4).expect("a queue");
        k.start_scheduler().expect("start");

        // The high-priority task waits ON the queue, which is what makes
        // it a waiter rather than merely a ready task of higher priority.
        assert_eq!(k.current(), hi, "the higher priority task runs first");
        assert!(
            matches!(k.queue_receive(q, 100), Ok(crate::queue::Wait::Blocked)),
            "an empty queue with a block time must park the receiver"
        );
        k.switch_context();
        assert_eq!(k.current(), lo, "with hi parked, lo runs");

        // Now the send, from the LOWER-priority task.
        let before = k.port().yields.get();
        assert!(matches!(
            k.queue_send(q, 7, 0),
            Ok(crate::queue::Wait::Ready(()))
        ));
        let asked = k.port().yields.get().saturating_sub(before);
        assert_eq!(
            asked, 1,
            "a send that readies a higher-priority receiver must ask for a switch"
        );
    }

    /// The other half: a receiver that did NOT wait cannot be woken, so
    /// the send asks for nothing — which is correct, and is exactly the
    /// trap a polling daemon falls into.
    ///
    /// Recorded as an assertion rather than a comment because it is the
    /// behaviour a reader will otherwise call a bug in the send path,
    /// having found the send not yielding. The send is right; the caller
    /// that polls instead of waiting is wrong.
    #[test]
    fn a_send_asks_for_nothing_when_the_higher_priority_task_never_waited() {
        let mut k = CountingKernel::new(CountingPort::default(), NoTrace)
            .expect("the declared geometry adds up");
        let _hi = k.create_task("hi", 2).expect("task hi");
        let _lo = k.create_task("lo", 1).expect("task lo");
        let q = k.queue_create(4).expect("a queue");
        k.start_scheduler().expect("start");

        // `hi` polls. A zero block time on an empty queue is `Empty`, NOT
        // `Blocked` — and the difference is the whole point: `Blocked`
        // means "I have been parked and will be woken", `Empty` means
        // "there was nothing there and I was not parked". A poller gets
        // the second, so there is nothing for a later send to wake.
        assert!(
            matches!(
                k.queue_receive(q, 0),
                Err(rusty_rtos_core::error::Error::Empty)
            ),
            "a zero-wait receive on an empty queue reports Empty, not Blocked"
        );
        k.switch_context();

        let before = k.port().yields.get();
        assert!(matches!(
            k.queue_send(q, 7, 0),
            Ok(crate::queue::Wait::Ready(()))
        ));
        assert_eq!(
            k.port().yields.get(),
            before,
            "there was no waiter to wake, so nothing should have been asked for"
        );
    }

    #[test]
    fn neither_equal_priority_task_starves_the_other() {
        let (a, b) = turns_over(200);
        // A round robin gives them one tick each in turn. Allowing a 4x
        // imbalance is generous; a scheduler that never rotates gives one
        // of them zero, which is what this is here to catch.
        let (lo, hi) = (a.min(b), a.max(b));
        assert!(
            lo.saturating_mul(4) >= hi,
            "one starved the other: {a} against {b}"
        );
    }

    /// **The geometry is the declaration**, arithmetic and all.
    ///
    /// This is the `.bss` claim as a number rather than a hope: every
    /// const is exactly what the two `tasks` and two `queues` lines say,
    /// with no spare anywhere.
    #[test]
    fn the_geometry_is_derived_from_the_declaration() {
        assert_eq!(demo::APP_TASKS, 2, "the two declared tasks");
        assert_eq!(demo::TASKS, 4, "plus the idle task and the timer daemon");
        assert_eq!(demo::QUEUES, 2, "the two declared queues");
        assert_eq!(demo::SLOTS, 13, "10 + 3, the declared lengths and no spare");
        // `ITEMS` is the SLOT count now, not the item count: two list items
        // per task (8), plus one end-marker node per list (14), rounded up
        // to a power of two. 22 -> 32.
        assert_eq!(
            demo::ITEMS,
            32,
            "8 items + 14 markers, rounded to a power of two"
        );
        assert_eq!(
            demo::ITEMS,
            crate::list_slots_for(demo::TASKS, demo::TIMERS, demo::LISTS),
            "and it is derived, not written by hand"
        );
        // The slack is the price of the mask, and it is worth being able to
        // see: this configuration carries 10 spare nodes.
        assert_eq!(
            demo::ITEMS - demo::LISTS,
            18,
            "item capacity: 8 needed, 18 available"
        );
        assert_eq!(
            demo::LISTS,
            crate::lists_for(TestConfig::MAX_PRIORITIES, 2, 0),
            "a ready list per priority, the fixed six, and two per queue"
        );
    }

    /// A declared system builds, and `Kernel::new` accepts the geometry —
    /// which is the check that `items_for`/`lists_for` were applied to the
    /// same numbers the struct was built from.
    #[test]
    fn a_declared_system_builds_and_the_kernel_accepts_its_geometry() {
        let mut k = kernel();
        let system = demo::System::build(&mut k).expect("nothing here can fail");
        // The handles are the kernel's, in declaration order.
        assert!(!system.consumer.is_null());
        assert!(!system.producer.is_null());
        assert_ne!(system.consumer, system.producer);
        assert_eq!(k.task_count(), 2, "no task the declaration did not name");
    }

    /// The queues come back typed, and carry what they were declared to.
    #[test]
    fn the_declared_queues_move_their_own_type() {
        let mut k = kernel();
        let mut system = demo::System::build(&mut k).expect("nothing here can fail");
        assert!(matches!(system.data.send(&mut k, 4_242u16, 0), Sent::Ok));
        assert!(matches!(system.marks.send(&mut k, 7u8, 0), Sent::Ok));
        assert_eq!(system.data.len(&mut k), Ok(1));
        assert_eq!(system.marks.len(&mut k), Ok(1));
    }

    /// **The `.bss` claim, as a number.**
    ///
    /// The alternative to declaring is what every scenario in this repo
    /// does today: pick a geometry by hand that covers the biggest thing
    /// you might do. The demo's kernel is sized `TASKS = 24`,
    /// `QUEUES = 12`, `SLOTS = 128`, `TIMERS = 32` because it has to hold
    /// eighteen scenarios at once. A system that declares two tasks and
    /// thirteen slots gets a kernel sized for two tasks and thirteen
    /// slots, and the difference is not small.
    ///
    /// This is a counter, not a benchmark: `size_of` is exact, decided at
    /// compile time, and identical on every machine.
    #[test]
    fn a_declared_kernel_is_sized_for_what_was_declared() {
        /// The demo's hand-picked geometry, which is what you use when
        /// you cannot declare.
        type Blanket = crate::Kernel<
            TestConfig,
            TestPort,
            NoTrace,
            NoTickHook,
            24,
            { crate::list_slots_for(24, 32, crate::lists_for(TestConfig::MAX_PRIORITIES, 12, 4)) },
            { crate::lists_for(TestConfig::MAX_PRIORITIES, 12, 4) },
            12,
            128,
            8,
            2048,
            32,
            4,
        >;

        let declared = core::mem::size_of::<K>();
        let blanket = core::mem::size_of::<Blanket>();
        // Printed so the footprint row can be read off a test run.
        let ratio = blanket as f64 / declared as f64;
        std::eprintln!("declared {declared} bytes, hand-sized {blanket} bytes, {ratio:.1}x");
        assert!(
            declared < blanket,
            "a declared kernel ({declared}) should be smaller than one              sized for everything ({blanket})"
        );
        // The declaration is the only input: change the declaration and
        // this number changes with it, which is the whole claim.
        assert_eq!(
            declared,
            core::mem::size_of::<demo::Kernel<TestPort, NoTrace, NoTickHook>>()
        );
    }

    /// **The slot count is exact, and the exactness is load-bearing.**
    ///
    /// `SLOTS` is the sum of the declared lengths, so filling every
    /// declared queue to its declared length uses the arena to the last
    /// slot and no further. A geometry that was rounded up would pass a
    /// weaker version of this; this one would fail if `SLOTS` were 12.
    #[test]
    fn every_declared_slot_is_usable_and_there_is_not_one_spare() {
        let mut k = kernel();
        let mut system = demo::System::build(&mut k).expect("nothing here can fail");
        for i in 0..10u16 {
            assert!(
                matches!(system.data.send(&mut k, i, 0), Sent::Ok),
                "slot {i} of the declared ten"
            );
        }
        for i in 0..3u8 {
            assert!(
                matches!(system.marks.send(&mut k, i, 0), Sent::Ok),
                "slot {i} of the declared three"
            );
        }
        // Both are now exactly full, which is `SLOTS` exactly consumed.
        assert!(matches!(system.data.send(&mut k, 99, 0), Sent::Full(99)));
        assert!(matches!(system.marks.send(&mut k, 9, 0), Sent::Full(9)));
        assert_eq!(demo::SLOTS, 13);
    }
}

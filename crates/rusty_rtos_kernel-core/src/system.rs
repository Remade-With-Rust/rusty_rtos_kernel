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
            /// Two list items per task plus one per timer, exactly the
            /// `ListItem_t`s a C `TCB_t` carries.
            pub const ITEMS: usize = $crate::items_for(TASKS, TIMERS);
            /// A ready list per priority, the fixed six, two per queue and
            /// one per event group.
            pub const LISTS: usize = $crate::lists_for(
                <$cfg as ::rusty_rtos_core::config::Config>::MAX_PRIORITIES,
                QUEUES,
                GROUPS,
            );

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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use rusty_rtos_core::config::Config;
    use rusty_rtos_core::hooks::NoTickHook;
    use rusty_rtos_core::isr::Woken;
    use rusty_rtos_core::port::Port;
    use rusty_rtos_core::tick::Bits32;
    use rusty_rtos_core::trace::{Event, Trace};

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
        { crate::items_for(5, 1) },
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
        { crate::items_for(5, 1) },
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
        assert_eq!(demo::ITEMS, 8, "two list items per task, no timers");
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
            { crate::items_for(24, 32) },
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

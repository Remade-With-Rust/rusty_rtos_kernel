//! Property tests (hardening gate H-28): the kernel's documented invariants,
//! checked after EVERY call of long random scripts.
//!
//! `no_panic.rs` asks whether a call comes back at all; the conformance
//! corpus asks whether a known scenario does exactly what the C does. This
//! asks the question between them: whatever valid sequence of calls a
//! program makes, do the things the kernel promises stay true?
//!
//! | invariant | what it means |
//! |---|---|
//! | ready accounting | the ready lists hold exactly the tasks whose state is Ready or Running |
//! | fixed priority | no Ready task outranks the Running one (one core, preemption on) |
//! | inheritance | a task's priority is never below its base, and equals it when it holds no mutex |
//! | ownership | a mutex's holder is the task the model says took it |
//! | bounds | a counting semaphore never exceeds its maximum |
//! | delays | a task delayed `d` ticks at tick `t` is not runnable before `t + d`, unless resumed or aborted |
//! | reaping | `task_count` is the number of live tasks once the idle task has reaped |
//!
//! A blocking call that parks its task is made AGAIN when that task next
//! runs, which is the kernel's retry protocol (`Wait::Blocked`); the script
//! never hands a parked task a different call.
//!
//! The generator is a seeded xorshift, as in `no_panic.rs`: no dependency,
//! and a failure names the seed and step that reproduce it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "a property test: it asserts by panicking and indexes small tables"
)]

mod hammer;

use core::cell::Cell;

use hammer::{Counting, Entropy, Xorshift};
use rusty_rtos_core::config::{Config, PosixDemoConfig};
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::{Kernel, TaskState, list_slots_for, lists_for};

/// A port that RECORDS a yield, so the test switches where a real port's
/// switch interrupt would: after the call that asked for it. (The hammer's
/// port ignores yields, which is right for a no-panic test and wrong here:
/// with it the first task the scheduler picked would run for ever, and
/// "no Ready task outranks the Running one" would hold vacuously. The first
/// version of this test did exactly that, and its coverage check caught it.)
#[derive(Debug, Default)]
struct YieldPort {
    nesting: Cell<u32>,
    in_isr: Cell<bool>,
    yielded: Cell<bool>,
}

impl Port for YieldPort {
    const COMMITS_SWITCH: bool = true;
    fn yield_now(&self) {
        self.yielded.set(true);
    }
    fn yield_from_isr(&self, woken: Woken) {
        if woken == Woken::YES {
            self.yielded.set(true);
        }
    }
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
        self.in_isr.get()
    }
    fn set_in_tick_entry(&self, yes: bool) {
        self.in_isr.set(yes);
    }
}

const TASKS: usize = 8;
const QUEUES: usize = 8;
const TIMERS: usize = 4;
const GROUPS: usize = 4;

type K = Kernel<
    PosixDemoConfig,
    YieldPort,
    Counting,
    NoTickHook,
    TASKS,
    {
        list_slots_for(
            TASKS,
            TIMERS,
            lists_for(<PosixDemoConfig as Config>::MAX_PRIORITIES, QUEUES, GROUPS),
        )
    },
    { lists_for(<PosixDemoConfig as Config>::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    32,
    4,
    256,
    TIMERS,
    GROUPS,
    { <PosixDemoConfig as Config>::TIMER_QUEUE_LENGTH },
>;

const APP: usize = 5;
const PRIORITIES: u8 = 5;
const SEM_MAX: usize = 3;

/// A call a parked task must make again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parked {
    Take(u64),
    Mutex(u64),
}

#[derive(Debug, Clone, Copy)]
struct Task {
    handle: TaskHandle,
    base: u8,
    holds: u32,
    /// The tick before which a delay forbids this task to run.
    wake_at: Option<u64>,
    parked: Option<Parked>,
}

struct World {
    k: K,
    tasks: [Option<Task>; APP],
    idle: TaskHandle,
    timer: TaskHandle,
    sem: QueueHandle,
    mutex: QueueHandle,
    created: u32,
}

impl World {
    fn new() -> Self {
        let mut k = K::new(YieldPort::default(), Counting::default()).unwrap();
        let sem = k.semaphore_create_counting(SEM_MAX, 1).unwrap();
        let mutex = k.mutex_create().unwrap();
        let first = k.create_task("a0", 1).unwrap();
        let started = k.start_scheduler().unwrap();
        // Nothing here runs the timer daemon's body, which in a firmware
        // blocks on its command queue; at the top priority it would hold the
        // CPU for ever. Parked, as `smp_differential.rs` parks it.
        k.suspend(Some(started.timer)).unwrap();
        if k.port().yielded.replace(false) {
            k.switch_context();
        }
        let mut tasks = [None; APP];
        tasks[0] = Some(Task {
            handle: first,
            base: 1,
            holds: 0,
            wake_at: None,
            parked: None,
        });
        Self {
            k,
            tasks,
            idle: started.idle,
            timer: started.timer,
            sem,
            mutex,
            created: 1,
        }
    }

    fn slot_of(&self, h: TaskHandle) -> Option<usize> {
        self.tasks
            .iter()
            .position(|t| t.is_some_and(|t| t.handle == h))
    }

    /// A tick, as the tick interrupt delivers it.
    fn tick(&mut self) {
        if self.k.increment_tick() {
            self.k.switch_context();
        }
    }

    /// The current task's call, or its parked call made again.
    fn as_current(&mut self, rng: &mut Xorshift) {
        let cur = self.k.current();
        let Some(s) = self.slot_of(cur) else {
            // Idle or the timer daemon is running: let time pass.
            self.tick();
            return;
        };
        let mut t = self.tasks[s].unwrap();
        let call = match t.parked {
            Some(p) => p,
            None => match rng.next_u64() % 5 {
                0 => {
                    let d = 1 + rng.next_u64() % 5;
                    self.k.delay(d).unwrap();
                    t.wake_at = Some(self.k.tick_count() + d);
                    self.tasks[s] = Some(t);
                    return;
                }
                1 => Parked::Take(rng.next_u64() % 4),
                // Never a mutex this task already holds: that is a self-
                // deadlock FreeRTOS permits, and when it times out
                // `vTaskPriorityDisinheritAfterTimeout` lowers the RUNNING
                // task without a yield (on one core, by design) -- which the
                // first run of this test found at seed 1, step 887, and which
                // is the C's documented behaviour rather than a defect.
                2 if t.holds == 0 => Parked::Mutex(rng.next_u64() % 4),
                2 => return,
                3 => {
                    let _ = self.k.semaphore_give(self.sem);
                    return;
                }
                _ => {
                    if t.holds > 0 && self.k.semaphore_give(self.mutex).is_ok() {
                        t.holds -= 1;
                        self.tasks[s] = Some(t);
                    }
                    return;
                }
            },
        };
        let answer = match call {
            Parked::Take(ticks) => self.k.semaphore_take(self.sem, ticks),
            Parked::Mutex(ticks) => self.k.semaphore_take(self.mutex, ticks),
        };
        t.parked = None;
        match answer {
            Ok(Wait::Ready(())) => {
                if matches!(call, Parked::Mutex(_)) {
                    t.holds += 1;
                }
            }
            Ok(Wait::Blocked) => t.parked = Some(call),
            Err(_) => {}
        }
        self.tasks[s] = Some(t);
    }

    /// What the switch interrupt does: take the yield the call asked for.
    fn settle(&mut self) {
        if self.k.port().yielded.replace(false) {
            self.k.switch_context();
        }
    }

    fn step(&mut self, rng: &mut Xorshift) {
        let pick = (rng.next_u64() % APP as u64) as usize;
        let op = rng.next_u64() % 12;
        match op {
            0 => {
                if self.tasks[pick].is_none() {
                    let p = 1 + (rng.next_u64() % (u64::from(PRIORITIES) - 1)) as u8;
                    let name = format!("a{}", self.created);
                    self.created += 1;
                    if let Ok(h) = self.k.create_task(&name, p) {
                        self.tasks[pick] = Some(Task {
                            handle: h,
                            base: p,
                            holds: 0,
                            wake_at: None,
                            parked: None,
                        });
                    }
                }
            }
            1 => {
                // A task that holds the mutex is not deleted: FreeRTOS leaves
                // a deleted holder's mutex held for ever, which is documented
                // behaviour and would make "ownership" untestable after it.
                if let Some(t) = self.tasks[pick].filter(|t| t.holds == 0) {
                    self.k.task_delete(Some(t.handle)).unwrap();
                    self.tasks[pick] = None;
                }
            }
            2 => {
                if let Some(t) = self.tasks[pick] {
                    self.k.suspend(Some(t.handle)).unwrap();
                }
            }
            3 => {
                if let Some(mut t) = self.tasks[pick] {
                    self.k.resume(t.handle).unwrap();
                    t.wake_at = None;
                    self.tasks[pick] = Some(t);
                }
            }
            4 => {
                if let Some(mut t) = self.tasks[pick] {
                    let p = (rng.next_u64() % u64::from(PRIORITIES)) as u8;
                    self.k.set_priority(Some(t.handle), p).unwrap();
                    t.base = p;
                    self.tasks[pick] = Some(t);
                }
            }
            5 => {
                if let Some(mut t) = self.tasks[pick] {
                    if self.k.abort_delay(t.handle) == Ok(true) {
                        t.wake_at = None;
                        self.tasks[pick] = Some(t);
                    }
                }
            }
            6 | 7 => self.tick(),
            8 => {
                if let Ok(woken) = self.k.semaphore_give_from_isr(self.sem) {
                    if woken == Woken::YES {
                        self.k.switch_context();
                    }
                }
            }
            _ => self.as_current(rng),
        }
        self.settle();
        self.k.check_tasks_waiting_termination();
        self.settle();
    }

    fn check(&mut self, seed: u64, step: u32) {
        let at = || format!("seed {seed:#x} step {step}");
        let k = &mut self.k;
        let mut all: Vec<TaskHandle> = vec![self.idle, self.timer];
        all.extend(self.tasks.iter().flatten().map(|t| t.handle));

        // Ready accounting.
        let mut runnable = 0;
        for &h in &all {
            if matches!(
                k.task_state_get(h),
                Ok(TaskState::Ready | TaskState::Running)
            ) {
                runnable += 1;
            }
        }
        let listed: usize = (0..7).map(|p| k.ready_len(p).unwrap_or(0)).sum();
        assert_eq!(listed, runnable, "{}: ready lists vs states", at());

        // Fixed priority.
        let cur = k.current();
        let cur_p = k.task_priority_get(Some(cur)).unwrap();
        for &h in &all {
            if k.task_state_get(h) == Ok(TaskState::Ready) {
                let p = k.task_priority_get(Some(h)).unwrap();
                assert!(
                    p <= cur_p,
                    "{}: a Ready task at {p} outranks the Running one at {cur_p}",
                    at()
                );
            }
        }

        // Inheritance and ownership.
        let holder = k.mutex_holder(self.mutex).unwrap();
        let mut holders = 0;
        for t in self.tasks.iter().flatten() {
            let p = k.task_priority_get(Some(t.handle)).unwrap();
            assert!(p >= t.base, "{}: priority {p} below base {}", at(), t.base);
            if t.holds == 0 {
                assert_eq!(
                    p,
                    t.base,
                    "{}: priority stayed raised with no mutex held",
                    at()
                );
            } else {
                holders += 1;
                assert_eq!(
                    holder,
                    t.handle,
                    "{}: the mutex's holder is not the task that took it",
                    at()
                );
            }
        }
        if holders == 0 {
            assert!(
                holder.is_null(),
                "{}: a mutex nobody took has a holder",
                at()
            );
        }

        // Bounds.
        assert!(
            k.semaphore_count(self.sem).unwrap() <= SEM_MAX,
            "{}: semaphore over its maximum",
            at()
        );

        // Delays.
        let now = k.tick_count();
        for t in self.tasks.iter().flatten() {
            if let Some(w) = t.wake_at {
                if now < w {
                    let s = k.task_state_get(t.handle).unwrap();
                    assert!(
                        !matches!(s, TaskState::Ready | TaskState::Running),
                        "{}: woke at tick {now}, delayed until {w}",
                        at()
                    );
                }
            }
        }

        // Reaping.
        assert_eq!(k.task_count(), all.len(), "{}: task_count", at());
    }
}

#[test]
fn the_documented_invariants_hold_after_every_call() {
    let mut steps_checked = 0_u64;
    let mut blocked = 0_u64;
    let mut inherited = 0_u64;
    for seed in 1..=48_u64 {
        let seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut rng = Xorshift(seed);
        let mut w = World::new();
        for step in 0..3_000 {
            w.step(&mut rng);
            w.check(seed, step);
            steps_checked += 1;
            blocked += w
                .tasks
                .iter()
                .flatten()
                .filter(|t| t.parked.is_some())
                .count() as u64;
            inherited += w
                .tasks
                .iter()
                .flatten()
                .filter(|t| t.holds > 0 && w.k.task_priority_get(Some(t.handle)).unwrap() > t.base)
                .count() as u64;
        }
    }
    // The script must reach the states the invariants are about.
    assert!(steps_checked >= 144_000);
    assert!(
        blocked > 10_000,
        "only {blocked} task-steps parked on a call"
    );
    assert!(
        inherited > 100,
        "only {inherited} task-steps with an inherited priority"
    );
}

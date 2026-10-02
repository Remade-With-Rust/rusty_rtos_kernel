//! SMP slice S2a: the two-core scheduler against the C kernel, step for step.
//!
//! `oracle/smp/driver.c` builds the pinned FreeRTOS-Kernel V11.3.1 with
//! `configNUMBER_OF_CORES = 2` on a fake port, runs a 20,000-step random
//! script of kernel calls -- each from a chosen core -- and writes one line
//! per step: the operation, its result, the yields the kernel asked for, and
//! both cores' current tasks after those yields are taken.
//! `oracle/smp/smp.trace` is that output, committed.
//!
//! This test runs the SAME script, from the same 32-bit xorshift, against
//! Kairos, and must print the SAME lines. A divergence names the first step
//! that differs and prints the neighbourhood.
//!
//! A second script, `smp_block.trace`, lets takes BLOCK. The C driver runs
//! each blocking take in a coroutine that leaves the kernel at the yield, and
//! resumes it -- `cont` -- the next time that task is current on either core.
//! Kairos does the same through its retry protocol: `Wait::Blocked` means
//! "make the same call again when this task next runs", and the kernel
//! carries the wait's own state across.
//!
//! Regenerate both traces (WSL, the oracle fetched by `kairos oracle fetch`):
//! `cd oracle/smp && sh run.sh`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "a differential test: it asserts by panicking, and indexes and counts small tables"
)]

use core::cell::Cell;
use core::fmt::Write as _;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::NoTrace;
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::{Kernel, list_slots_for, lists_for};

const TRACE: &str = include_str!("../../../oracle/smp/smp.trace");
const TRACE_BLOCK: &str = include_str!("../../../oracle/smp/smp_block.trace");

/// `oracle/smp/FreeRTOSConfig.h`, field for field.
struct DiffConfig;
impl Config for DiffConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 5;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 4;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 1;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
    const NUMBER_OF_CORES: u8 = 2;
    const USE_TIME_SLICING: bool = true;
}

/// The fake port's twin: a settable core, and every yield RECORDED.
#[derive(Default)]
struct DiffPort {
    core: Cell<u8>,
    yields: Cell<u32>,
}

impl Port for DiffPort {
    const COMMITS_SWITCH: bool = true;
    fn yield_now(&self) {
        self.yields.set(self.yields.get() | (1 << self.core.get()));
    }
    fn yield_from_isr(&self, woken: Woken) {
        if woken == Woken::YES {
            self.yield_now();
        }
    }
    fn enter_critical(&self) {}
    fn exit_critical(&self) {}
    fn set_interrupt_mask_from_isr(&self) -> u32 {
        0
    }
    fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
    fn in_isr(&self) -> bool {
        false
    }
    fn core_id(&self) -> u8 {
        self.core.get()
    }
    fn set_in_tick_entry(&self, _yes: bool) {}
}

const TASKS: usize = 20;
const QUEUES: usize = 4;

type K = Kernel<
    DiffConfig,
    DiffPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { list_slots_for(TASKS, 1, lists_for(5, QUEUES, 0)) },
    { lists_for(5, QUEUES, 0) },
    QUEUES,
    8,
    0,
    0,
    1,
    0,
    1,
>;

/// `driver.c`'s xorshift, bit for bit.
struct Rng(u32);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
}

const STEPS: u32 = 20_000;
const SLOTS: usize = 10;

struct Driver {
    k: K,
    app: [Option<TaskHandle>; SLOTS],
    /// A take suspended inside the kernel, by slot: its block time.
    pending: [Option<u64>; SLOTS],
    created: u32,
    out: String,
}

impl Driver {
    fn on(&self, core: u8) {
        self.k.port().core.set(core);
    }

    fn name(&self, h: TaskHandle) -> String {
        if h.is_null() {
            return "-".to_owned();
        }
        self.k
            .name_of(h)
            .map(|n| n.as_str().to_owned())
            .unwrap_or_else(|_| "?".to_owned())
    }

    fn is_app(&self, h: TaskHandle) -> bool {
        self.app.contains(&Some(h))
    }

    fn slot_of(&self, h: TaskHandle) -> Option<usize> {
        self.app.iter().position(|a| *a == Some(h))
    }

    /// A take, made or continued, as the C prints it: 1 taken, 0 timed out,
    /// 2 blocked (and now pending).
    fn take_blocking(&mut self, slot: usize, sem: QueueHandle, ticks: u64) -> i64 {
        match self.k.semaphore_take(sem, ticks) {
            Ok(Wait::Ready(())) => {
                self.pending[slot] = None;
                1
            }
            Ok(Wait::Blocked) => {
                self.pending[slot] = Some(ticks);
                2
            }
            Err(_) => {
                self.pending[slot] = None;
                0
            }
        }
    }

    fn switch_core(&mut self, c: u8) {
        self.on(c);
        self.k.switch_context();
    }

    fn line(&mut self, step: u32, core: u8, op: &str, r: i64, mask: u32) {
        for c in 0..2u8 {
            if mask & (1 << c) != 0 {
                self.switch_core(c);
            }
        }
        let c0 = self.name(self.k.current_on(0));
        let c1 = self.name(self.k.current_on(1));
        let _ = writeln!(
            self.out,
            "{step} core={core} {op} r={r} y={mask} c0={c0} c1={c1}"
        );
        // The idle task's reaping, which the C driver never needs to show:
        // a deleted TCB the C leaves on its termination list costs nothing
        // visible, but Kairos's reap slots are finite.
        self.k.check_tasks_waiting_termination();
    }

    /// Yields the step asked for: this port's recorded ones and the kernel's
    /// cross-core requests, as one mask -- the C's `fake_yields`.
    fn take_mask(&mut self) -> u32 {
        let own = self.k.port().yields.replace(0);
        own | u32::from(self.k.take_core_yields())
    }
}

fn run(blocking: bool) -> String {
    let mut rng = Rng(0x2545_f491);
    let mut d = Driver {
        k: K::new(DiffPort::default(), NoTrace).expect("geometry"),
        app: [None; SLOTS],
        pending: [None; SLOTS],
        created: 0,
        out: String::new(),
    };
    let sem = d.k.semaphore_create_binary().expect("semaphore");
    for i in 0..4 {
        let name = format!("t{}", d.created);
        d.created += 1;
        let p = (1 + rng.next() % 3) as u8;
        d.app[i] = Some(d.k.create_task(&name, p).expect("initial task"));
    }
    let started = d.k.start_scheduler().expect("start");
    d.on(0);
    d.k.suspend(Some(started.timer))
        .expect("park the timer daemon");
    let _ = d.take_mask();
    d.switch_core(0);
    d.switch_core(1);
    let _ = d.take_mask();
    d.line(0, 0, "start", 0, 0);

    for step in 1..=STEPS {
        let mut core = (rng.next() % 2) as u8;
        let kind = rng.next() % 100;
        let slot = (rng.next() % SLOTS as u32) as usize;
        let arg = rng.next();
        let mut r: i64 = 0;
        d.on(core);
        let _ = d.take_mask();
        let t = d.app[slot];
        let op;

        // A task with a take suspended inside the kernel runs nothing else
        // until that take returns: if it is current here, the step is the
        // continuation -- the same call again, which is the retry protocol.
        if blocking {
            let cur = d.k.current_on(usize::from(core));
            if let Some(s) = d.slot_of(cur) {
                if let Some(ticks) = d.pending[s] {
                    r = d.take_blocking(s, sem, ticks);
                    let op = format!("cont {}", d.name(cur));
                    let mask = d.take_mask();
                    d.line(step, core, &op, r, mask);
                    continue;
                }
            }
        }

        if kind < 10 {
            if t.is_none() {
                let name = format!("t{}", d.created);
                d.created += 1;
                let p = (arg % 4) as u8;
                match d.k.create_task(&name, p) {
                    Ok(h) => {
                        d.app[slot] = Some(h);
                        r = 1;
                    }
                    Err(_) => r = 0,
                }
                op = format!("create {slot} {name} p{p}");
            } else {
                op = "noop".to_owned();
            }
        } else if kind < 18 {
            if let Some(h) = t {
                op = format!("delete {slot} {}", d.name(h));
                d.app[slot] = None;
                d.pending[slot] = None;
                d.k.task_delete(Some(h)).expect("delete");
            } else {
                op = "noop".to_owned();
            }
        } else if kind < 30 {
            if let Some(h) = t {
                d.k.suspend(Some(h)).expect("suspend");
                op = format!("suspend {}", d.name(h));
            } else {
                op = "noop".to_owned();
            }
        } else if kind < 42 {
            if let Some(h) = t {
                d.k.resume(h).expect("resume");
                op = format!("resume {}", d.name(h));
            } else {
                op = "noop".to_owned();
            }
        } else if kind < 54 {
            if let Some(h) = t {
                let p = (arg % 4) as u8;
                d.k.set_priority(Some(h), p).expect("priority");
                op = format!("prio {} {p}", d.name(h));
            } else {
                op = "noop".to_owned();
            }
        } else if kind < 64 {
            let cur = d.k.current_on(usize::from(core));
            if d.is_app(cur) {
                let ticks = u64::from(1 + arg % 5);
                d.k.delay(ticks).expect("delay");
                op = format!("delay {} {ticks}", d.name(cur));
            } else {
                op = "noop".to_owned();
            }
        } else if kind < 72 {
            r = i64::from(matches!(d.k.semaphore_give(sem), Ok(Wait::Ready(()))));
            op = "give".to_owned();
        } else if kind < 78 {
            match d.k.semaphore_give_from_isr(sem) {
                Ok(woken) => {
                    r = 1;
                    d.k.port().yield_from_isr(woken);
                }
                Err(_) => r = 0,
            }
            op = "give_isr".to_owned();
        } else if kind < 86 {
            let cur = d.k.current_on(usize::from(core));
            let ticks = if blocking && arg % 3 != 0 {
                u64::from(1 + (arg >> 2) % 6)
            } else {
                0
            };
            if let (Some(s), true) = (d.slot_of(cur), ticks > 0) {
                r = d.take_blocking(s, sem, ticks);
                op = format!("takeb {} {ticks}", d.name(cur));
            } else if d.is_app(cur) {
                r = i64::from(matches!(d.k.semaphore_take(sem, 0), Ok(Wait::Ready(()))));
                op = format!("take {}", d.name(cur));
            } else {
                op = "noop".to_owned();
            }
        } else if kind < 96 {
            core = 0;
            d.on(0);
            r = i64::from(d.k.increment_tick());
            if r != 0 {
                d.k.port().yield_now();
            }
            op = "tick".to_owned();
        } else {
            d.k.port().yield_now();
            op = "yield".to_owned();
        }
        let mask = d.take_mask();
        d.line(step, core, &op, r, mask);
    }
    d.out
}

#[test]
fn two_cores_schedule_exactly_as_the_c_kernel_does() {
    compare(&run(false), TRACE);
}

/// The same, with takes that BLOCK and are continued later -- possibly on the
/// other core.
#[test]
fn two_cores_block_and_wake_exactly_as_the_c_kernel_does() {
    let theirs = TRACE_BLOCK.replace("\r\n", "\n");
    // The script must actually block, wake and time out, or this proves
    // nothing beyond the test above.
    for (what, at_least) in [(" takeb ", 200), (" cont ", 200)] {
        let n = theirs.lines().filter(|l| l.contains(what)).count();
        assert!(
            n > at_least,
            "the blocking script has only {n} lines with {what:?}"
        );
    }
    let timeouts = theirs
        .lines()
        .filter(|l| l.contains(" cont ") && l.contains(" r=0 "))
        .count();
    assert!(timeouts > 0, "no blocked take ever timed out");
    compare(&run(true), TRACE_BLOCK);
}

fn compare(ours: &str, theirs: &str) {
    let theirs = theirs.replace("\r\n", "\n");
    let mut ours_lines = ours.lines();
    for (n, want) in theirs.lines().enumerate() {
        let got = ours_lines.next().unwrap_or("<missing>");
        if got != want {
            let from = n.saturating_sub(6);
            let ctx: Vec<&str> = theirs.lines().skip(from).take(n - from).collect();
            panic!(
                "SMP divergence at line {n}\n  C:      {want}\n  Kairos: {got}\n  preceding (C):\n    {}",
                ctx.join("\n    ")
            );
        }
    }
    assert!(ours_lines.next().is_none(), "Kairos printed extra lines");
    assert_eq!(theirs.lines().count(), STEPS as usize + 1);
}

//! The API differential (`docs/plans/api-differential.md`, P1): kernel
//! calls against the C kernel, one step at a time, on one core and on two.
//!
//! `oracle/api/driver.c` builds the pinned FreeRTOS-Kernel V11.3.1 on a fake
//! port -- once with `configNUMBER_OF_CORES 1`, once with `2` -- runs a seeded
//! random script, and writes one line per step:
//!
//! ```text
//! <step> c<core> <op> <args...> | r=<result> y=<yields> cur=<..> T=<..> s=<..>
//! ```
//!
//! Left of the bar is the step, fully specified; this test parses it and
//! makes the same call on Kairos. It never regenerates the script, so the
//! grammar lives in the C alone. Right of the bar is what the C kernel then
//! looked like: the call's result, the yields it asked for (already taken),
//! each core's current task, every app task's state and priority as
//! `eTaskGetState` / `uxTaskPriorityGet` answer them, and the semaphore's
//! count. Kairos must print the same right-hand side after every step, so a
//! wrong decision shows on the step that makes it.
//!
//! A call that BLOCKS is continued -- `cont` -- the next time its task is
//! current on a core: the C resumes a suspended coroutine, Kairos makes the
//! same call again (`Wait::Blocked`'s retry protocol).
//!
//! Regenerate the traces (WSL, the oracle fetched by `kairos oracle fetch`):
//! `cd oracle/api && sh run.sh`.
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
use rusty_rtos_core::handle::{EventGroupHandle, QueueHandle, TaskHandle};
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::NoTrace;
use rusty_rtos_kernel_core::kernel::{NotifyAction, TaskState};
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::{Kernel, list_slots_for, lists_for};

const TRACE_1: &str = include_str!("../../../oracle/api/api1.trace");
const TRACE_2: &str = include_str!("../../../oracle/api/api2.trace");

/// `oracle/api/FreeRTOSConfig.h`, field for field, at one core.
struct OneCore;
impl Config for OneCore {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 5;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 4;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 1;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
    const NUMBER_OF_CORES: u8 = 1;
    const USE_TIME_SLICING: bool = true;
}

/// The same, at two cores.
struct TwoCores;
impl Config for TwoCores {
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
/// The semaphore, three app queues, the mutex, the recursive mutex, the
/// counting semaphore and the timer's command queue, with room.
const QUEUES: usize = 12;
/// Queue item storage: three queues of up to three, and the timer's.
const ITEMS: usize = 16;
const SLOTS: usize = 10;
const QSLOTS: usize = 3;
const GSLOTS: usize = 2;

/// A call that may block: made, and if it blocks made again when its task
/// next runs.
#[derive(Clone, Copy)]
enum Call {
    Take(u64),
    QSend {
        q: usize,
        value: u64,
        ticks: u64,
    },
    QSendFront {
        q: usize,
        value: u64,
        ticks: u64,
    },
    QRecv {
        q: usize,
        ticks: u64,
    },
    QPeek {
        q: usize,
        ticks: u64,
    },
    MTake(u64),
    RTake(u64),
    CTake(u64),
    NTake {
        clear: bool,
        ticks: u64,
    },
    NWait {
        entry: u32,
        exit: u32,
        ticks: u64,
    },
    /// `flags`: bit 0 clear on exit, bit 1 wait for all.
    GWait {
        g: usize,
        mask: u32,
        flags: u32,
        ticks: u64,
    },
    GSync {
        g: usize,
        set: u32,
        mask: u32,
        ticks: u64,
    },
}

/// The C's `eNotifyAction`, by number, as the driver prints it.
fn action(n: u64) -> NotifyAction {
    match n {
        0 => NotifyAction::None,
        1 => NotifyAction::SetBits,
        2 => NotifyAction::Increment,
        3 => NotifyAction::Overwrite,
        _ => NotifyAction::NoOverwrite,
    }
}

/// A wait's result as the C driver prints it: `ok` for a call that passed,
/// -2 for blocked, 0 for full / empty / timed out.
fn waited<T>(r: Result<Wait<T>, rusty_rtos_core::error::Error>, ok: impl Fn(T) -> i64) -> i64 {
    match r {
        Ok(Wait::Ready(v)) => ok(v),
        Ok(Wait::Blocked) => -2,
        Err(_) => 0,
    }
}

fn state_char(s: TaskState) -> char {
    match s {
        TaskState::Running => 'X',
        TaskState::Ready => 'R',
        TaskState::Blocked => 'B',
        TaskState::Suspended => 'S',
        TaskState::Deleted => 'D',
    }
}

/// One replay, for one kernel type. A macro because the kernel's core count
/// is part of its type.
macro_rules! replay {
    ($name:ident, $config:ty) => {
        fn $name(trace: &str, label: &str) -> usize {
            type K = Kernel<
                $config,
                DiffPort,
                NoTrace,
                NoTickHook,
                TASKS,
                { list_slots_for(TASKS, 1, lists_for(5, QUEUES, GSLOTS)) },
                { lists_for(5, QUEUES, GSLOTS) },
                QUEUES,
                ITEMS,
                0,
                0,
                1,
                GSLOTS,
                1,
            >;
            const CORES: u8 = <$config as Config>::NUMBER_OF_CORES;

            struct D {
                k: K,
                app: [Option<TaskHandle>; SLOTS],
                pending: [Option<Call>; SLOTS],
                sem: QueueHandle,
                queues: [Option<QueueHandle>; QSLOTS],
                mutex: QueueHandle,
                rmutex: QueueHandle,
                csem: QueueHandle,
                groups: [Option<EventGroupHandle>; GSLOTS],
            }

            impl D {
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
                fn slot_of(&self, h: TaskHandle) -> Option<usize> {
                    self.app.iter().position(|a| *a == Some(h))
                }
                /// The yields the step asked for, this port's and the
                /// kernel's cross-core requests together: the C's
                /// `fake_yields`.
                fn take_mask(&mut self) -> u32 {
                    let own = self.k.port().yields.replace(0);
                    own | u32::from(self.k.take_core_yields())
                }
                fn switch_core(&mut self, c: u8) {
                    self.on(c);
                    self.k.switch_context();
                }
                /// Make (or make again) a parked call: 1 / 0 as the C's call
                /// returns, -2 for "blocked, now pending".
                fn call(&mut self, slot: usize, call: Call) -> i64 {
                    let q = |i: usize| self.queues[i].expect("a live queue");
                    let r = match call {
                        Call::Take(ticks) => waited(self.k.semaphore_take(self.sem, ticks), |()| 1),
                        Call::QSend { q: i, value, ticks } => {
                            waited(self.k.queue_send(q(i), value, ticks), |()| 1)
                        }
                        Call::QSendFront { q: i, value, ticks } => {
                            waited(self.k.queue_send_to_front(q(i), value, ticks), |()| 1)
                        }
                        Call::QRecv { q: i, ticks } => {
                            waited(self.k.queue_receive(q(i), ticks), |v| v as i64)
                        }
                        Call::QPeek { q: i, ticks } => {
                            waited(self.k.queue_peek(q(i), ticks), |v| v as i64)
                        }
                        Call::MTake(ticks) => waited(self.k.semaphore_take(self.mutex, ticks), |()| 1),
                        Call::RTake(ticks) => {
                            waited(self.k.mutex_take_recursive(self.rmutex, ticks), |()| 1)
                        }
                        Call::CTake(ticks) => waited(self.k.semaphore_take(self.csem, ticks), |()| 1),
                        Call::NTake { clear, ticks } => {
                            waited(self.k.notify_take(0, clear, ticks), i64::from)
                        }
                        // pdTRUE / pdFALSE plus twice the value, as the C
                        // driver folds them into one number.
                        Call::NWait { entry, exit, ticks } => waited(
                            self.k.notify_wait(0, entry, exit, ticks),
                            |(ok, v)| i64::from(ok) + 2 * i64::from(v),
                        ),
                        Call::GWait { g, mask, flags, ticks } => {
                            let group = self.groups[g].expect("a live group");
                            waited(
                                self.k.event_group_wait_bits(
                                    group,
                                    mask,
                                    flags & 1 != 0,
                                    flags & 2 != 0,
                                    ticks,
                                ),
                                i64::from,
                            )
                        }
                        Call::GSync { g, set, mask, ticks } => {
                            let group = self.groups[g].expect("a live group");
                            waited(self.k.event_group_sync(group, set, mask, ticks), i64::from)
                        }
                    };
                    self.pending[slot] = (r == -2).then_some(call);
                    r
                }
                /// Take the step's yields, lowest core first, then observe
                /// from core 0: the right-hand side of the C's line.
                fn observe(&mut self, r: i64, mask: u32) -> String {
                    for c in 0..CORES {
                        if mask & (1 << c) != 0 {
                            self.switch_core(c);
                        }
                    }
                    self.on(0);
                    let mut s = format!("r={r} y={mask} cur=");
                    for c in 0..CORES {
                        let n = self.name(self.k.current_on(usize::from(c)));
                        let _ = write!(s, "{}{n}", if c > 0 { "," } else { "" });
                    }
                    s.push_str(" T=");
                    for i in 0..SLOTS {
                        match self.app[i] {
                            None => s.push('-'),
                            Some(h) => {
                                let st = self.k.task_state_get(h).map_or('?', state_char);
                                let p = self.k.task_priority_get(Some(h)).map_or(9, |p| p);
                                let _ = write!(s, "{st}{p}");
                            }
                        }
                        if i + 1 < SLOTS {
                            s.push('.');
                        }
                    }
                    let count = self.k.semaphore_count(self.sem).unwrap_or(99);
                    let _ = write!(s, " s={count} Q=");
                    for i in 0..QSLOTS {
                        match self.queues[i] {
                            None => s.push('-'),
                            Some(h) => {
                                let n = self.k.queue_messages_waiting(h).unwrap_or(99);
                                let _ = write!(s, "{n}");
                            }
                        }
                        if i + 1 < QSLOTS {
                            s.push('.');
                        }
                    }
                    let m = self.k.mutex_holder(self.mutex).unwrap_or(TaskHandle::NULL);
                    let rm = self.k.mutex_holder(self.rmutex).unwrap_or(TaskHandle::NULL);
                    let c = self.k.semaphore_count(self.csem).unwrap_or(99);
                    let _ = write!(s, " M={} R={} C={c} N=", self.name(m), self.name(rm));
                    for i in 0..SLOTS {
                        match self.app[i] {
                            None => s.push('-'),
                            Some(h) => {
                                let v = self.k.notify_value(Some(h), 0).map_or(-1, i64::from);
                                let _ = write!(s, "{v}");
                            }
                        }
                        if i + 1 < SLOTS {
                            s.push('.');
                        }
                    }
                    s.push_str(" E=");
                    for g in 0..GSLOTS {
                        match self.groups[g] {
                            None => s.push('-'),
                            Some(h) => {
                                let bits = self.k.event_group_bits(h).map_or(-1, i64::from);
                                let _ = write!(s, "{bits}");
                            }
                        }
                        if g + 1 < GSLOTS {
                            s.push('.');
                        }
                    }
                    // The idle task's reaping, which the C driver never needs
                    // to show: a deleted TCB the C leaves on its termination
                    // list costs nothing visible, but Kairos's reap slots are
                    // finite.
                    self.k.check_tasks_waiting_termination();
                    s
                }
            }

            let mut lines = trace.lines();
            let header = lines.next().expect("a header line");
            let init: Vec<u8> = header
                .split_whitespace()
                .find_map(|w| w.strip_prefix("init="))
                .expect("init= in the header")
                .split(',')
                .map(|p| p.parse().unwrap())
                .collect();
            assert!(
                header.contains(&format!("cores={CORES} ")),
                "this trace is not a {CORES}-core one: {header}"
            );

            let mut k = K::new(DiffPort::default(), NoTrace).expect("geometry");
            // In the C's order: semaphore, mutex, recursive mutex, counting.
            let sem = k.semaphore_create_binary().expect("semaphore");
            let mutex = k.mutex_create().expect("mutex");
            let rmutex = k.mutex_create_recursive().expect("recursive mutex");
            let csem = k.semaphore_create_counting(3, 1).expect("counting semaphore");
            let mut d = D {
                k,
                app: [None; SLOTS],
                pending: [None; SLOTS],
                sem,
                queues: [None; QSLOTS],
                mutex,
                rmutex,
                csem,
                groups: [None; GSLOTS],
            };
            for (i, p) in init.iter().enumerate() {
                d.app[i] = Some(d.k.create_task(&format!("t{i}"), *p).expect("initial task"));
            }
            let started = d.k.start_scheduler().expect("start");
            d.on(0);
            d.k.suspend(Some(started.timer)).expect("park the timer daemon");
            let _ = d.take_mask();
            for c in 0..CORES {
                d.switch_core(c);
            }
            let _ = d.take_mask();

            let mut seen: Vec<&str> = Vec::new();
            for line in lines {
                let (step, want) = line.split_once(" | ").expect("a step line");
                let mut w = step.split_whitespace();
                let _n = w.next();
                let core: u8 = w.next().unwrap().trim_start_matches('c').parse().unwrap();
                let op = w.next().unwrap();
                let args: Vec<&str> = w.collect();
                let num = |i: usize| -> u64 { args[i].parse().unwrap() };
                d.on(core);
                let _ = d.take_mask();
                let cur_slot = d.slot_of(d.k.current_on(usize::from(core)));
                let mut r: i64 = 0;
                match op {
                    "start" | "noop" => {}
                    "cont" => {
                        let s = cur_slot.expect("cont: the current task is an app task");
                        let call = d.pending[s].expect("cont: a call is pending");
                        r = d.call(s, call);
                    }
                    "create" => {
                        let slot = num(0) as usize;
                        match d.k.create_task(args[1], num(2) as u8) {
                            Ok(h) => {
                                d.app[slot] = Some(h);
                                r = 1;
                            }
                            Err(_) => r = -1,
                        }
                    }
                    "delete" => {
                        let slot = num(0) as usize;
                        let h = d.app[slot].take().unwrap();
                        d.pending[slot] = None;
                        d.k.task_delete(Some(h)).expect("delete");
                    }
                    "suspend" => d.k.suspend(d.app[num(0) as usize]).expect("suspend"),
                    "resume" => d.k.resume(d.app[num(0) as usize].unwrap()).expect("resume"),
                    "prio" => d
                        .k
                        .set_priority(d.app[num(0) as usize], num(1) as u8)
                        .expect("priority"),
                    "delay" => d.k.delay(num(0)).expect("delay"),
                    "give" => {
                        r = i64::from(matches!(d.k.semaphore_give(d.sem), Ok(Wait::Ready(()))));
                    }
                    "give_isr" => match d.k.semaphore_give_from_isr(d.sem) {
                        Ok(woken) => {
                            r = 1;
                            d.k.port().yield_from_isr(woken);
                        }
                        Err(_) => r = 0,
                    },
                    "take" => {
                        let s = cur_slot.expect("take: the current task is an app task");
                        let ticks = num(0);
                        if ticks == 0 {
                            r = i64::from(matches!(
                                d.k.semaphore_take(d.sem, 0),
                                Ok(Wait::Ready(()))
                            ));
                        } else {
                            r = d.call(s, Call::Take(ticks));
                        }
                    }
                    "qcreate" => match d.k.queue_create(num(1) as usize) {
                        Ok(h) => {
                            d.queues[num(0) as usize] = Some(h);
                            r = 1;
                        }
                        Err(_) => r = -1,
                    },
                    "qdelete" => {
                        let h = d.queues[num(0) as usize].take().unwrap();
                        d.k.queue_delete(h).expect("queue delete");
                    }
                    "qsend" | "qsendf" | "qrecv" | "qpeek" => {
                        let s = cur_slot.expect("a queue call from an app task");
                        let q = num(0) as usize;
                        let call = match op {
                            "qsend" => Call::QSend { q, value: num(1), ticks: num(2) },
                            "qsendf" => Call::QSendFront { q, value: num(1), ticks: num(2) },
                            "qrecv" => Call::QRecv { q, ticks: num(1) },
                            _ => Call::QPeek { q, ticks: num(1) },
                        };
                        r = d.call(s, call);
                    }
                    "qover" => {
                        let h = d.queues[num(0) as usize].unwrap();
                        r = waited(d.k.queue_overwrite(h, num(1)), |()| 1);
                    }
                    "qreset" => {
                        let h = d.queues[num(0) as usize].unwrap();
                        d.k.queue_reset(h).expect("reset");
                        r = 1; // xQueueReset answers pdPASS, always
                    }
                    "qsend_isr" | "qsendf_isr" | "qover_isr" => {
                        let h = d.queues[num(0) as usize].unwrap();
                        let sent = match op {
                            "qsend_isr" => d.k.queue_send_from_isr(h, num(1)),
                            "qsendf_isr" => d.k.queue_send_to_front_from_isr(h, num(1)),
                            _ => d.k.queue_overwrite_from_isr(h, num(1)),
                        };
                        match sent {
                            Ok(woken) => {
                                r = 1;
                                d.k.port().yield_from_isr(woken);
                            }
                            Err(_) => r = 0,
                        }
                    }
                    "qrecv_isr" => {
                        let h = d.queues[num(0) as usize].unwrap();
                        match d.k.queue_receive_from_isr(h) {
                            Ok((v, woken)) => {
                                r = v as i64;
                                d.k.port().yield_from_isr(woken);
                            }
                            Err(_) => r = 0,
                        }
                    }
                    "qpeek_isr" => {
                        let h = d.queues[num(0) as usize].unwrap();
                        r = d.k.queue_peek_from_isr(h).map_or(0, |v| v as i64);
                    }
                    "qfull_isr" => {
                        let h = d.queues[num(0) as usize].unwrap();
                        r = i64::from(d.k.queue_is_full_from_isr(h).unwrap_or(false));
                    }
                    "gcreate" => match d.k.event_group_create() {
                        Ok(h) => {
                            d.groups[num(0) as usize] = Some(h);
                            r = 1;
                        }
                        Err(_) => r = -1,
                    },
                    "gdelete" => {
                        let h = d.groups[num(0) as usize].take().unwrap();
                        d.k.event_group_delete(h).expect("group delete");
                    }
                    "gset" | "gclear" | "gget_isr" => {
                        let h = d.groups[num(0) as usize].unwrap();
                        r = i64::from(match op {
                            "gset" => d.k.event_group_set_bits(h, num(1) as u32).unwrap(),
                            "gclear" => d.k.event_group_clear_bits(h, num(1) as u32).unwrap(),
                            _ => d.k.event_group_bits_from_isr(h).unwrap(),
                        });
                    }
                    "gwait" | "gsync" => {
                        let s = cur_slot.expect("a group wait from an app task");
                        let g = num(0) as usize;
                        let call = if op == "gwait" {
                            Call::GWait {
                                g,
                                mask: num(1) as u32,
                                flags: num(2) as u32,
                                ticks: num(3),
                            }
                        } else {
                            Call::GSync {
                                g,
                                set: num(1) as u32,
                                mask: num(2) as u32,
                                ticks: num(3),
                            }
                        };
                        r = d.call(s, call);
                    }
                    "ntf" | "ntfq" | "ntf_isr" | "ntfq_isr" => {
                        let t = d.app[num(0) as usize].unwrap();
                        let (a, v) = (action(num(1)), num(2) as u32);
                        r = match op {
                            "ntf" => i64::from(d.k.notify(t, 0, v, a).unwrap()),
                            "ntfq" => {
                                let (ok, prev) = d.k.notify_and_query(t, 0, v, a).unwrap();
                                i64::from(ok) + 2 * i64::from(prev)
                            }
                            "ntf_isr" => {
                                let (ok, woken) = d.k.notify_from_isr(t, 0, v, a).unwrap();
                                d.k.port().yield_from_isr(woken);
                                i64::from(ok)
                            }
                            _ => {
                                let (ok, prev, woken) =
                                    d.k.notify_and_query_from_isr(t, 0, v, a).unwrap();
                                d.k.port().yield_from_isr(woken);
                                i64::from(ok) + 2 * i64::from(prev)
                            }
                        };
                    }
                    "ngive_isr" => {
                        // vTaskNotifyGiveFromISR: an increment, from an ISR.
                        let t = d.app[num(0) as usize].unwrap();
                        let (_, woken) = d.k.notify_from_isr(t, 0, 0, NotifyAction::Increment).unwrap();
                        d.k.port().yield_from_isr(woken);
                    }
                    "ntake" | "nwait" => {
                        let s = cur_slot.expect("a notification wait from an app task");
                        let call = if op == "ntake" {
                            Call::NTake { clear: num(0) != 0, ticks: num(1) }
                        } else {
                            Call::NWait { entry: num(0) as u32, exit: num(1) as u32, ticks: num(2) }
                        };
                        r = d.call(s, call);
                    }
                    "nstate_clear" => {
                        let t = d.app[num(0) as usize];
                        r = i64::from(d.k.notify_state_clear(t, 0).unwrap());
                    }
                    "nvalue_clear" => {
                        let t = d.app[num(0) as usize];
                        r = i64::from(d.k.notify_value_clear(t, 0, num(1) as u32).unwrap());
                    }
                    "mtake" | "rtake" | "ctake" => {
                        let s = cur_slot.expect("a take from an app task");
                        let call = match op {
                            "mtake" => Call::MTake(num(0)),
                            "rtake" => Call::RTake(num(0)),
                            _ => Call::CTake(num(0)),
                        };
                        r = d.call(s, call);
                    }
                    "mgive" => r = waited(d.k.semaphore_give(d.mutex), |()| 1),
                    "rgive" => r = i64::from(d.k.mutex_give_recursive(d.rmutex).is_ok()),
                    "cgive" => r = waited(d.k.semaphore_give(d.csem), |()| 1),
                    "cgive_isr" => match d.k.semaphore_give_from_isr(d.csem) {
                        Ok(woken) => {
                            r = 1;
                            d.k.port().yield_from_isr(woken);
                        }
                        Err(_) => r = 0,
                    },
                    "tick" => {
                        r = i64::from(d.k.increment_tick());
                        if r != 0 {
                            d.k.port().yield_now();
                        }
                    }
                    "yield" => d.k.port().yield_now(),
                    other => panic!("an op this replay does not know: {other:?}"),
                }
                let mask = d.take_mask();
                let got = d.observe(r, mask);
                // Compared HERE, step by step: the first divergence is the
                // finding, and anything after it is a consequence -- a later
                // step may not even be makeable on a kernel that has drifted.
                if got != want {
                    let from = seen.len().saturating_sub(4);
                    panic!(
                        "{label}: divergence at step line {}\n  C:      {line}\n  Kairos: {step} | {got}\n  preceding (C):\n    {}",
                        seen.len(),
                        seen[from..].join("\n    ")
                    );
                }
                seen.push(line);
            }
            seen.len()
        }
    };
}

replay!(replay_one, OneCore);
replay!(replay_two, TwoCores);

/// The script must actually exercise what it claims to, or a pass proves
/// little. Floors, per op and per outcome, well under what the pinned
/// scripts reach -- a regenerated script that stopped reaching an arm
/// fails here rather than passing quietly.
fn exercised(trace: &str) {
    let t = trace.replace("\r\n", "\n");
    let op = |l: &str| l.split_whitespace().nth(2).unwrap_or("").to_owned();
    let count = |pred: &dyn Fn(&str) -> bool| t.lines().skip(1).filter(|l| pred(l)).count();
    for (name, floor) in [
        ("create", 50),
        ("delete", 50),
        ("suspend", 100),
        ("resume", 100),
        ("prio", 100),
        ("delay", 100),
        ("give", 100),
        ("give_isr", 100),
        ("take", 100),
        ("qcreate", 100),
        ("qdelete", 100),
        ("qsend", 200),
        ("qsendf", 100),
        ("qrecv", 200),
        ("qpeek", 100),
        ("qover", 50),
        ("qreset", 100),
        ("qsend_isr", 100),
        ("qsendf_isr", 100),
        ("qover_isr", 50),
        ("qrecv_isr", 100),
        ("qpeek_isr", 50),
        ("qfull_isr", 50),
        ("mtake", 150),
        ("mgive", 40),
        ("rtake", 100),
        ("rgive", 100),
        ("ctake", 100),
        ("cgive", 100),
        ("cgive_isr", 100),
        ("ntf", 60),
        ("ntfq", 60),
        ("ntf_isr", 60),
        ("ntfq_isr", 60),
        ("ngive_isr", 60),
        ("ntake", 60),
        ("nwait", 60),
        ("nstate_clear", 60),
        ("nvalue_clear", 60),
        ("gcreate", 40),
        ("gdelete", 40),
        ("gset", 100),
        ("gclear", 50),
        ("gget_isr", 50),
        ("gwait", 100),
        ("gsync", 30),
        ("tick", 500),
        ("cont", 200),
    ] {
        let n = count(&|l| op(l) == name);
        assert!(n >= floor, "only {n} `{name}` steps (floor {floor})");
    }
    let blocked = count(&|l| l.contains(" r=-2 ") && op(l) != "cont");
    let timeouts = count(&|l| op(l) == "cont" && l.contains(" r=0 "));
    let values = count(&|l| {
        let v: i64 = l
            .split(" r=")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|r| r.parse().ok())
            .unwrap_or(0);
        v >= 10
    });
    assert!(blocked >= 300, "only {blocked} calls blocked");
    assert!(timeouts >= 50, "only {timeouts} blocked calls timed out");
    assert!(values >= 500, "only {values} values received");
}

#[test]
fn one_core_answers_every_step_as_the_c_kernel_does() {
    exercised(TRACE_1);
    let n = replay_one(&TRACE_1.replace("\r\n", "\n"), "one core");
    assert_eq!(n, 20_001, "every step replayed");
}

#[test]
fn two_cores_answer_every_step_as_the_c_kernel_does() {
    exercised(TRACE_2);
    let n = replay_two(&TRACE_2.replace("\r\n", "\n"), "two cores");
    assert_eq!(n, 20_001, "every step replayed");
}

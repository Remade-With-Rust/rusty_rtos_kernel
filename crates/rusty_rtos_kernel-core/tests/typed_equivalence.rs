//! The typed face is the C-twinned calls it claims, and nothing else.
//!
//! `typed.rs` says a typed send is one `xQueueSendToBack`, a typed receive
//! one `xQueueReceive`, `Mutex::with` one take and one give -- "the same
//! calls, in the same order, taking the same critical sections". The C
//! differential judges those calls; nothing judges that the wrappers make
//! them. This does (plan `api-differential.md`, P4).
//!
//! Two kernels, built and started identically. Each step drives kernel A
//! through a wrapper, behind a proxy that records every [`Raw`] call and
//! its answer; the step also DECLARES the C-twinned sequence the wrapper
//! claims. Then:
//!
//! 1. the recorded sequence must be the declared one, call for call and
//!    argument for argument;
//! 2. kernel B makes the declared calls directly -- the kernel's own
//!    C-twinned methods, no trait in between -- and every answer, every
//!    trace line (with the port's exit count beside it), every yield and the
//!    running task must come out the same as A's.
//!
//! (1) catches a wrapper that calls the wrong thing or in the wrong order;
//! (2) catches a `Raw` impl that maps a name to the wrong kernel call.
//! `raw_queue_has_room` and `raw_in_isr` are reads the kernel makes of its
//! own state, not C calls, and take no critical section; they are recorded
//! so their place in the order is checked, and B reads them the same way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "an equivalence test: it asserts by panicking"
)]

use core::cell::Cell;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::typed::{Isr, Mutex, Queue, Raw};
use rusty_rtos_kernel_core::{Kernel, list_slots_for, lists_for};

struct Cfg;
impl Config for Cfg {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 5;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 4;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 1;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
}

/// Counts what a port can see: critical sections, yields, and whether the
/// caller is an interrupt.
#[derive(Default)]
struct CountingPort {
    enters: Cell<u32>,
    exits: Cell<u32>,
    yields: Cell<u32>,
    isr: Cell<bool>,
}

impl Port for CountingPort {
    fn yield_now(&self) {
        self.yields.set(self.yields.get() + 1);
    }
    fn yield_from_isr(&self, woken: Woken) {
        if woken == Woken::YES {
            self.yield_now();
        }
    }
    fn enter_critical(&self) {
        self.enters.set(self.enters.get() + 1);
    }
    fn exit_critical(&self) {
        self.exits.set(self.exits.get() + 1);
    }
    fn set_interrupt_mask_from_isr(&self) -> u32 {
        self.enters.set(self.enters.get() + 1);
        0
    }
    fn clear_interrupt_mask_from_isr(&self, _saved: u32) {
        self.exits.set(self.exits.get() + 1);
    }
    fn in_isr(&self) -> bool {
        self.isr.get()
    }
    fn core_id(&self) -> u8 {
        0
    }
    fn set_in_tick_entry(&self, _yes: bool) {}
}

/// Every trace line, with the exit count the kernel noted beside it.
#[derive(Default)]
struct Lines {
    exits: u64,
    lines: Vec<String>,
}

impl Trace for Lines {
    fn note_exits(&mut self, exits: u64) {
        self.exits = exits;
    }
    fn event(&mut self, tick: u64, event: Event<'_>) {
        self.lines
            .push(format!("{tick} exits={} {event:?}", self.exits));
    }
}

const TASKS: usize = 8;
const QUEUES: usize = 6;

type K = Kernel<
    Cfg,
    CountingPort,
    Lines,
    NoTickHook,
    TASKS,
    { list_slots_for(TASKS, 1, lists_for(5, QUEUES, 0)) },
    { lists_for(5, QUEUES, 0) },
    QUEUES,
    16,
    0,
    0,
    1,
    0,
    1,
>;

/// One call across the [`Raw`] boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    /// `xQueueCreate`.
    QueueCreate(usize),
    /// `xQueueSendToBack`.
    Send(QueueHandle, u64, u64),
    /// `xQueueReceive`.
    Receive(QueueHandle, u64),
    /// `uxQueueMessagesWaiting`.
    Waiting(QueueHandle),
    /// `xQueueSendToBackFromISR`.
    SendFromIsr(QueueHandle, u64),
    /// `xSemaphoreCreateMutex`.
    MutexCreate,
    /// `xSemaphoreTake`.
    Take(QueueHandle, u64),
    /// `xSemaphoreGive`.
    Give(QueueHandle),
    /// The kernel reading its own queue: no C call.
    HasRoom(QueueHandle),
    /// `xPortIsInsideInterrupt`: no kernel call.
    InIsr,
}

/// Kernel A behind the recording proxy.
struct Rec<'a> {
    k: &'a mut K,
    log: Vec<(Call, String)>,
}

impl Rec<'_> {
    fn note<R: core::fmt::Debug>(&mut self, call: Call, answer: R) -> R {
        self.flush();
        self.log.push((call, format!("{answer:?}")));
        answer
    }
}

impl Raw for Rec<'_> {
    fn raw_queue_create(&mut self, length: usize) -> Result<QueueHandle> {
        let a = self.k.raw_queue_create(length);
        self.note(Call::QueueCreate(length), a)
    }
    fn raw_queue_send(&mut self, queue: QueueHandle, value: u64, ticks: u64) -> Result<Wait<()>> {
        let a = self.k.raw_queue_send(queue, value, ticks);
        self.note(Call::Send(queue, value, ticks), a)
    }
    fn raw_queue_receive(&mut self, queue: QueueHandle, ticks: u64) -> Result<Wait<u64>> {
        let a = self.k.raw_queue_receive(queue, ticks);
        self.note(Call::Receive(queue, ticks), a)
    }
    fn raw_queue_messages_waiting(&mut self, queue: QueueHandle) -> Result<usize> {
        let a = self.k.raw_queue_messages_waiting(queue);
        self.note(Call::Waiting(queue), a)
    }
    fn raw_queue_has_room(&self, queue: QueueHandle) -> bool {
        // `&self`: the one read with no `&mut`, logged through a cell.
        let a = self.k.raw_queue_has_room(queue);
        ROOM.with(|r| {
            r.borrow_mut()
                .push((Call::HasRoom(queue), format!("{a:?}")))
        });
        a
    }
    fn raw_in_isr(&self) -> bool {
        let a = self.k.raw_in_isr();
        ROOM.with(|r| r.borrow_mut().push((Call::InIsr, format!("{a:?}"))));
        a
    }
    fn raw_queue_send_from_isr(&mut self, queue: QueueHandle, value: u64) -> Result<Woken> {
        let a = self.k.raw_queue_send_from_isr(queue, value);
        self.note(Call::SendFromIsr(queue, value), a)
    }
    fn raw_mutex_create(&mut self) -> Result<QueueHandle> {
        let a = self.k.raw_mutex_create();
        self.note(Call::MutexCreate, a)
    }
    fn raw_mutex_take(&mut self, mutex: QueueHandle, ticks: u64) -> Result<Wait<()>> {
        let a = self.k.raw_mutex_take(mutex, ticks);
        self.note(Call::Take(mutex, ticks), a)
    }
    fn raw_mutex_give(&mut self, mutex: QueueHandle) -> Result<()> {
        let a = self.k.raw_mutex_give(mutex);
        self.note(Call::Give(mutex), a)
    }
}

std::thread_local! {
    /// The `&self` reads, in order, until the next `&mut` call files them.
    static ROOM: core::cell::RefCell<Vec<(Call, String)>> = const { core::cell::RefCell::new(Vec::new()) };
}

impl Rec<'_> {
    /// File the `&self` reads made since the last `&mut` call, so the log
    /// keeps the true order.
    fn flush(&mut self) {
        let reads = ROOM.with(|r| core::mem::take(&mut *r.borrow_mut()));
        self.log.extend(reads);
    }
}

/// Kernel B: the declared call, made directly on the kernel's C-twinned
/// method. Answers are formatted the way the proxy formats A's.
fn direct(k: &mut K, call: Call) -> String {
    match call {
        Call::QueueCreate(n) => format!("{:?}", k.queue_create(n)),
        Call::Send(q, v, t) => format!("{:?}", k.queue_send(q, v, t)),
        Call::Receive(q, t) => format!("{:?}", k.queue_receive(q, t)),
        Call::Waiting(q) => format!("{:?}", k.queue_messages_waiting(q)),
        Call::SendFromIsr(q, v) => format!("{:?}", k.queue_send_from_isr(q, v)),
        Call::MutexCreate => format!("{:?}", k.mutex_create()),
        Call::Take(m, t) => format!("{:?}", k.semaphore_take(m, t)),
        Call::Give(m) => format!("{:?}", k.semaphore_give(m).map(|_| ())),
        Call::HasRoom(q) => format!("{:?}", k.raw_queue_has_room(q)),
        Call::InIsr => format!("{:?}", k.port().in_isr()),
    }
}

/// What the two kernels must agree on after every step.
fn state(k: &K) -> String {
    let p = k.port();
    format!(
        "cur={:?} enters={} exits={} yields={} trace:\n{}",
        k.current(),
        p.enters.get(),
        p.exits.get(),
        p.yields.get(),
        k.trace().lines.join("\n")
    )
}

struct Pair {
    a: K,
    b: K,
    tasks: [TaskHandle; 2],
    steps: usize,
}

impl Pair {
    fn new() -> Self {
        fn started() -> (K, [TaskHandle; 2]) {
            let mut k = K::new(CountingPort::default(), Lines::default()).expect("geometry");
            let hi = k.create_task("hi", 2).expect("hi");
            let lo = k.create_task("lo", 1).expect("lo");
            let started = k.start_scheduler().expect("start");
            // The timer daemon outranks both; park it, as the differential does.
            k.suspend(Some(started.timer)).expect("park the daemon");
            (k, [hi, lo])
        }
        let (a, ta) = started();
        let (b, tb) = started();
        assert_eq!(ta, tb);
        Self {
            a,
            b,
            tasks: ta,
            steps: 0,
        }
    }

    /// Run `wrapper` on A through the proxy, declare what it claims, make
    /// the claim on B, and compare everything.
    fn step<R: core::fmt::Debug>(
        &mut self,
        what: &str,
        claim: &[Call],
        wrapper: impl FnOnce(&mut Rec<'_>) -> R,
    ) -> R {
        self.steps += 1;
        ROOM.with(|r| r.borrow_mut().clear());
        let mut rec = Rec {
            k: &mut self.a,
            log: Vec::new(),
        };
        let answer = wrapper(&mut rec);
        rec.flush();
        let log = rec.log;
        let calls: Vec<Call> = log.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            calls, claim,
            "step {} ({what}): the wrapper's calls are not the ones it claims",
            self.steps
        );
        for (call, a_answer) in &log {
            let b_answer = direct(&mut self.b, *call);
            assert_eq!(
                *a_answer, b_answer,
                "step {} ({what}): {call:?} answered differently through the face",
                self.steps
            );
        }
        let (sa, sb) = (state(&self.a), state(&self.b));
        assert!(
            sa == sb,
            "step {} ({what}): the kernels differ after the wrapper\n--- face:\n{sa}\n--- direct:\n{sb}",
            self.steps
        );
        answer
    }

    /// A call outside the face, made identically on both kernels.
    fn both(&mut self, f: impl Fn(&mut K)) {
        f(&mut self.a);
        f(&mut self.b);
    }
}

#[test]
fn every_typed_wrapper_is_the_c_twinned_sequence_it_claims() {
    use Call::*;
    let mut p = Pair::new();
    let [hi, lo] = p.tasks;
    assert_eq!(p.a.current(), hi);

    let mut q = p.step("Queue::create", &[QueueCreate(2)], |k| {
        Queue::<u32, 2>::create(k).expect("queue")
    });
    let qh = q.handle();
    let mut q2 = p.step("Queue::create, one slot", &[QueueCreate(1)], |k| {
        Queue::<u8, 1>::create(k).expect("queue")
    });
    let q2h = q2.handle();
    let m = p.step("Mutex::new", &[MutexCreate], |k| {
        Mutex::new(k, 0_u32).expect("mutex")
    });
    let mh = m.handle();

    // len / is_empty: one uxQueueMessagesWaiting each.
    let n = p.step("Queue::len", &[Waiting(qh)], |k| q.len(k));
    assert_eq!(n, Ok(0));
    let e = p.step("Queue::is_empty", &[Waiting(qh)], |k| q.is_empty(k));
    assert_eq!(e, Ok(true));

    // send with room: the room read, then one send of the slot index.
    let s = p.step("send, room", &[HasRoom(qh), Send(qh, 0, 0)], |k| {
        q.send(k, 10, 0)
    });
    assert!(s.is_ok());
    let s = p.step("send, room", &[HasRoom(qh), Send(qh, 1, 0)], |k| {
        q.send(k, 11, 0)
    });
    assert!(s.is_ok());
    // send to a full queue: the send is still made (it is what gives the
    // trace its QUEUE_SEND_FAILED), and the value comes back.
    let s = p.step("send, full", &[HasRoom(qh), Send(qh, 0, 0)], |k| {
        q.send(k, 12, 0)
    });
    assert!(!s.is_ok(), "a refused send is not pdPASS");
    assert_eq!(s.into_value(), Some(12));

    let r = p.step("receive, ready", &[Receive(qh, 0)], |k| q.receive(k, 0));
    assert_eq!(r, Ok(Wait::Ready(Some(10))));

    // The ISR capability, refused at task level: one read, nothing else.
    let none = p.step("Isr::with at task level", &[InIsr], |k| {
        Isr::with(k, |isr| q.send_from_isr(isr, 99))
    });
    assert!(none.is_none());

    p.both(|k| k.port().isr.set(true));
    let s = p.step(
        "send_from_isr, room",
        &[InIsr, HasRoom(qh), SendFromIsr(qh, 0)],
        |k| Isr::with(k, |isr| q.send_from_isr(isr, 13)),
    );
    let s = s.expect("in an interrupt");
    assert!(s.is_ok());
    assert_eq!(s.woken(), Woken::NO);
    let s = p.step(
        "send_from_isr, full",
        &[InIsr, HasRoom(qh), SendFromIsr(qh, 0)],
        |k| Isr::with(k, |isr| q.send_from_isr(isr, 14)),
    );
    let s = s.expect("in an interrupt");
    assert!(!s.is_ok());
    assert_eq!(s.woken(), Woken::NO);
    p.both(|k| k.port().isr.set(false));

    // Mutex::with: one take, the closure, one give.
    let v = p.step("Mutex::with", &[Take(mh, 0), Give(mh)], |k| {
        m.with(k, 0, |v| {
            *v += 1;
            *v
        })
    });
    assert_eq!(v, Some(1));
    // A mutex its own holder cannot take again: one take, no closure, no give.
    p.both(|k| {
        let _ = k.semaphore_take(mh, 0).expect("take");
    });
    let v = p.step("Mutex::with, not taken", &[Take(mh, 0)], |k| {
        m.with(k, 0, |v| *v)
    });
    assert_eq!(v, None);
    p.both(|k| {
        let _ = k.semaphore_give(mh).expect("give");
    });

    // A receive that blocks: `hi` waits on the empty one-slot queue and
    // `lo` runs; `lo`'s send wakes `hi`, which runs at once and makes the
    // same receive again, as the retry protocol says.
    let r = p.step("receive, blocks", &[Receive(q2h, 5)], |k| q2.receive(k, 5));
    assert_eq!(r, Ok(Wait::Blocked));
    assert_eq!(p.a.current(), lo);
    let s = p.step("send, wakes", &[HasRoom(q2h), Send(q2h, 0, 0)], |k| {
        q2.send(k, 1, 0)
    });
    assert!(s.is_ok());
    assert_eq!(p.a.current(), hi);
    let r = p.step("receive, retried", &[Receive(q2h, 5)], |k| q2.receive(k, 5));
    assert_eq!(r, Ok(Wait::Ready(Some(1))));

    // A send that blocks on the full queue, retried once `lo` makes room.
    let s = p.step("send, blocks", &[HasRoom(qh), Send(qh, 0, 3)], |k| {
        q.send(k, 15, 3)
    });
    let back = match s {
        rusty_rtos_kernel_core::typed::Sent::Blocked(v) => v,
        other => panic!("expected Blocked, got {other:?}"),
    };
    assert_eq!(p.a.current(), lo);
    let r = p.step("receive, makes room", &[Receive(qh, 0)], |k| {
        q.receive(k, 0)
    });
    assert_eq!(r, Ok(Wait::Ready(Some(11))));
    assert_eq!(p.a.current(), hi);
    let s = p.step("send, retried", &[HasRoom(qh), Send(qh, 1, 3)], |k| {
        q.send(k, back, 3)
    });
    assert!(s.is_ok());

    // A deleted queue: each wrapper still makes its one call, and the
    // kernel's refusal comes through.
    p.both(|k| k.queue_delete(q2h).expect("delete"));
    let n = p.step("len, deleted", &[Waiting(q2h)], |k| q2.len(k));
    assert!(n.is_err());
    let r = p.step("receive, deleted", &[Receive(q2h, 0)], |k| q2.receive(k, 0));
    assert!(r.is_err());
    let s = p.step("send, deleted", &[HasRoom(q2h), Send(q2h, 0, 0)], |k| {
        q2.send(k, 2, 0)
    });
    assert_eq!(s.into_value(), Some(2));

    // Enough happened for the comparison to mean something.
    assert!(p.a.trace().lines.len() > 20, "{}", p.a.trace().lines.len());
    assert!(p.a.port().exits.get() > 20);
}

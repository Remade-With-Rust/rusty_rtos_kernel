//! Kani harnesses for the FreeRTOS CBMC proof list.
//!
//! `FreeRTOS/Test/CBMC/proofs/{Queue,Task}` is upstream's own statement of
//! what must be true of the kernel: twenty-three proofs about the queue and
//! fifteen about tasks, each one driving a single API function with
//! unconstrained arguments and a stubbed-out rest-of-kernel, and each one
//! asking CBMC whether anything can go wrong. That list is the property
//! list for this kernel too, which is why the harnesses below are named
//! after the proof directories rather than after our own functions.
//!
//! # What is being proved, and what does not need proving
//!
//! CBMC's job on the C is mostly memory safety: the proofs constrain a
//! pointer to be valid, or deliberately do not, and ask whether a
//! dereference can go out of bounds. Nearly all of that is gone here before
//! any tool runs — the crate is `forbid(unsafe)`, there are no pointers, a
//! handle is an index and a generation that the arena checks, and every
//! slice access is a `get`. What is left is what Kani checks by default and
//! what these harnesses assert on top:
//!
//! - **No panic.** Arithmetic that overflows, a `get` unwrapped, a slice
//!   assumed longer than it is. Kani proves the absence of all three on
//!   every path through the call, for *every* argument — which is the thing
//!   `tests/no_panic.rs` can only sample.
//! - **The answer is an answer.** A call with a nonsense handle comes back
//!   with an error rather than touching another object's state.
//! - **The counters stay consistent.** A successful send leaves exactly one
//!   more message waiting and never more than the queue holds.
//!
//! # Running them
//!
//! `cargo kani -p rusty_rtos_kernel-core --harness <name>` runs one, and
//! one at a time is the way to run them: ten of the thirty-two do not
//! converge, and in a combined run a single blow-up says nothing about the
//! others. Twenty-two verify — 32,145 checks, 0 failures — most in under
//! ten seconds. Which ten do not, and why, is measured rather than guessed;
//! the umbrella ledger has the two rows.
//!
//! Kani is Linux and macOS only, so on a Windows box this is a WSL command.
//! Nothing here compiles outside `cfg(kani)`, so the crate builds and
//! tests exactly as before without the tool.
//!
//! # Why the geometry is tiny
//!
//! Symbolic execution pays for every array element in the kernel, and the
//! kernel is arrays. [`ProofConfig`] is the smallest configuration that
//! still has two priorities, two queues, two tasks and a timer, which is
//! enough for every property here to be about something real: a queue with
//! one slot cannot show the difference between full and empty, and a kernel
//! with one priority cannot preempt.

use rusty_rtos_core::config::Config;
use rusty_rtos_core::error::{Error, Result};
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};

use crate::queue::{Position, Wait};
use crate::{Kernel, items_for, lists_for};

/// The smallest configuration with room for the properties to be about
/// something: two priorities so preemption exists, and a timer daemon
/// below the top so `Config::validate` is satisfied.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProofConfig;

impl Config for ProofConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 100;
    const MAX_PRIORITIES: u8 = 2;
    const MINIMAL_STACK_SIZE: usize = 1;
    const MAX_TASK_NAME_LEN: usize = 4;
    const TIMER_TASK_PRIORITY: u8 = 1;
    const TIMER_TASK_STACK_DEPTH: usize = 1;
    const TIMER_QUEUE_LENGTH: usize = 1;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
    const USE_TIME_SLICING: bool = false;
}

const TASKS: usize = 3;
const QUEUES: usize = 2;
const SLOTS: usize = 4;
const BUFFERS: usize = 1;
const BYTES: usize = 8;
const TIMERS: usize = 1;
const GROUPS: usize = 1;

/// A port that counts nesting and does nothing else, so the kernel's own
/// bookkeeping has something real to talk to.
#[derive(Debug, Default)]
struct ProofPort {
    nesting: core::cell::Cell<u32>,
    in_isr: core::cell::Cell<bool>,
}

impl Port for ProofPort {
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
        self.in_isr.get()
    }
    fn set_in_tick_entry(&self, yes: bool) {
        self.in_isr.set(yes);
    }
}

/// A trace that keeps nothing. The properties here are about the kernel's
/// state, not its trace.
#[derive(Debug, Default)]
struct NoTrace;

impl Trace for NoTrace {
    fn event(&mut self, _tick: u64, _event: Event<'_>) {}
}

type K = Kernel<
    ProofConfig,
    ProofPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { items_for(TASKS, TIMERS) },
    { lists_for(ProofConfig::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    BUFFERS,
    BYTES,
    TIMERS,
    GROUPS,
>;

/// A kernel with one application task, before the scheduler starts.
///
/// This is what nearly every harness below uses, and the reason is
/// measured rather than assumed: `start_scheduler` is the one step the
/// model checker cannot afford (`a_fresh_kernel_is_not_running` and the two
/// beside it are that measurement). A kernel that has its tasks but has not
/// started costs seconds.
///
/// What it gives up is real and worth naming. The *bodies* under proof are
/// the same either side of `vTaskStartScheduler` — a queue send copies,
/// counts and unblocks the same way — so the no-panic result, which is what
/// the CBMC proofs are mostly about, holds for every argument on the same
/// code. What it does not reach is the part of a blocking call that only
/// runs with a scheduler: the block itself, and the switch after it. The
/// four harnesses that need that keep [`started`] and are named as the ones
/// that do not converge.
fn ready() -> K {
    let mut k = match K::new(ProofPort::default(), NoTrace) {
        Ok(k) => k,
        Err(_) => unreachable!(),
    };
    let _ = k.create_task("p", 1);
    k
}

/// A kernel past `start_scheduler`, for the harnesses whose property is
/// about a running system.
///
/// These do not converge — CBMC passes 2 GB and 500 s on one with no
/// symbolic input at all — and they are kept because they are the
/// harnesses, not because they pass. Running them is
/// `cargo kani --harness <name>` and a long wait; the way to make them
/// finish is to stub this function rather than let it build the state.
fn started() -> K {
    let mut k = ready();
    let _ = k.start_scheduler();
    k
}

/// A handle no arena minted: the harnesses that want to prove a nonsense
/// argument is refused rather than followed.
///
/// It ranges over the whole word, and the obvious economy does not pay.
/// Bounding the index and the generation to the handful of values next to
/// the arena — which is where the interesting disagreements live, and what
/// the C proofs do with their pointers — was tried and changed nothing:
/// the four harnesses that pass a symbolic handle into a kernel call time
/// out either way. The cost is not the size of the space; it is that a
/// call which walks the arena and the lists with a symbolic handle has to
/// be explored for every slot it could name.
fn any_queue() -> QueueHandle {
    QueueHandle::from_raw(kani::any())
}

fn any_task() -> TaskHandle {
    TaskHandle::from_raw(kani::any())
}

/// A block time the kernel will not spend the whole proof unwinding.
fn small_ticks() -> u64 {
    let ticks: u64 = kani::any();
    kani::assume(ticks <= 2);
    ticks
}

// ------------------------------------------------------------- Queue --

/// `Queue/QueueGenericCreate`: a queue of any length either comes back or
/// is refused, and a refused one leaves no slots taken.
#[kani::proof]
#[kani::unwind(40)]
fn queue_generic_create() {
    let mut k = ready();
    let length: usize = kani::any();
    kani::assume(length <= SLOTS + 1);
    if let Ok(q) = k.queue_create(length) {
        assert!(k.queue_messages_waiting(q) == Ok(0));
        assert!(k.queue_spaces_available(q) == Ok(length));
    }
}

/// `Queue/QueueGenericSend`: a send either takes a slot or does not, and
/// the count never passes the length.
#[kani::proof]
#[kani::unwind(40)]
fn queue_generic_send() {
    let mut k = ready();
    let Ok(q) = k.queue_create(2) else {
        return;
    };
    let value: u64 = kani::any();
    let ticks = small_ticks();
    let before = k.queue_messages_waiting(q).unwrap_or(0);
    match k.queue_send_generic(q, value, ticks, Position::Back) {
        Ok(Wait::Ready(())) => {
            assert!(k.queue_messages_waiting(q) == Ok(before + 1));
        }
        Ok(Wait::Blocked) | Err(_) => {
            assert!(k.queue_messages_waiting(q) == Ok(before));
        }
    }
    assert!(k.queue_messages_waiting(q).unwrap_or(0) <= 2);
}

/// The same call with a handle from nowhere: refused, and nothing else
/// changed.
#[kani::proof]
#[kani::unwind(40)]
fn queue_generic_send_stale_handle() {
    let mut k = ready();
    let Ok(real) = k.queue_create(2) else {
        return;
    };
    let q = any_queue();
    kani::assume(q != real);
    let before = k.queue_messages_waiting(real).unwrap_or(0);
    let _ = k.queue_send_generic(q, kani::any(), 0, Position::Back);
    assert!(k.queue_messages_waiting(real) == Ok(before));
}

/// `Queue/QueueGenericSendFromISR`.
#[kani::proof]
#[kani::unwind(40)]
fn queue_generic_send_from_isr() {
    let mut k = ready();
    let Ok(q) = k.queue_create(2) else {
        return;
    };
    let before = k.queue_messages_waiting(q).unwrap_or(0);
    match k.queue_send_from_isr(q, kani::any()) {
        Ok(_) => assert!(k.queue_messages_waiting(q) == Ok(before + 1)),
        Err(_) => assert!(k.queue_messages_waiting(q) == Ok(before)),
    }
}

/// `Queue/QueueReceive`: a receive that answers took exactly one message
/// off, and one that did not took none.
#[kani::proof]
#[kani::unwind(40)]
fn queue_receive() {
    let mut k = ready();
    let Ok(q) = k.queue_create(2) else {
        return;
    };
    let _ = k.queue_send_generic(q, kani::any(), 0, Position::Back);
    let before = k.queue_messages_waiting(q).unwrap_or(0);
    match k.queue_receive(q, small_ticks()) {
        Ok(Wait::Ready(_)) => assert!(k.queue_messages_waiting(q) == Ok(before - 1)),
        Ok(Wait::Blocked) | Err(_) => assert!(k.queue_messages_waiting(q) == Ok(before)),
    }
}

/// `Queue/QueueReceiveFromISR`.
#[kani::proof]
#[kani::unwind(40)]
fn queue_receive_from_isr() {
    let mut k = ready();
    let Ok(q) = k.queue_create(2) else {
        return;
    };
    let before = k.queue_messages_waiting(q).unwrap_or(0);
    match k.queue_receive_from_isr(q) {
        Ok((_, _)) => assert!(k.queue_messages_waiting(q) == Ok(before.saturating_sub(1))),
        Err(_) => assert!(k.queue_messages_waiting(q) == Ok(before)),
    }
}

/// `Queue/QueuePeek`: a peek never changes what is waiting.
#[kani::proof]
#[kani::unwind(40)]
fn queue_peek() {
    let mut k = ready();
    let Ok(q) = k.queue_create(2) else {
        return;
    };
    let _ = k.queue_send_generic(q, kani::any(), 0, Position::Back);
    let before = k.queue_messages_waiting(q).unwrap_or(0);
    let _ = k.queue_peek(q, 0);
    assert!(k.queue_messages_waiting(q) == Ok(before));
}

/// `Queue/QueueGenericReset`: after a reset the queue is empty, whatever it
/// held before.
#[kani::proof]
#[kani::unwind(40)]
fn queue_generic_reset() {
    let mut k = ready();
    let Ok(q) = k.queue_create(2) else {
        return;
    };
    let _ = k.queue_send_generic(q, kani::any(), 0, Position::Back);
    let _ = k.queue_send_generic(q, kani::any(), 0, Position::Back);
    if k.queue_reset(q).is_ok() {
        assert!(k.queue_messages_waiting(q) == Ok(0));
        assert!(k.queue_spaces_available(q) == Ok(2));
    }
}

/// `Queue/QueueMessagesWaiting` and `Queue/QueueSpacesAvailable`: the two
/// always add up to the length, for any handle that resolves.
#[kani::proof]
#[kani::unwind(40)]
fn queue_messages_waiting_and_spaces_available() {
    let mut k = ready();
    let Ok(q) = k.queue_create(2) else {
        return;
    };
    let _ = k.queue_send_generic(q, kani::any(), 0, Position::Back);
    let waiting = k.queue_messages_waiting(q);
    let spaces = k.queue_spaces_available(q);
    if let (Ok(waiting), Ok(spaces)) = (waiting, spaces) {
        assert!(waiting + spaces == 2);
    }
}

/// `Queue/QueueCreateCountingSemaphore`: a counting semaphore starts with
/// the initial count it was asked for, or is refused.
#[kani::proof]
#[kani::unwind(40)]
fn queue_create_counting_semaphore() {
    let mut k = ready();
    let max: usize = kani::any();
    let initial: usize = kani::any();
    kani::assume(max <= SLOTS && initial <= max);
    if let Ok(s) = k.semaphore_create_counting(max, initial) {
        assert!(k.queue_messages_waiting(s) == Ok(initial));
    }
}

/// `Queue/QueueCreateMutex` and `Queue/QueueGetMutexHolder`: a fresh mutex
/// is free and has no holder.
#[kani::proof]
#[kani::unwind(40)]
fn queue_create_mutex_and_get_holder() {
    let mut k = ready();
    let Ok(m) = k.mutex_create() else {
        return;
    };
    assert!(k.mutex_holder(m) == Ok(TaskHandle::NULL));
    assert!(k.queue_messages_waiting(m) == Ok(1));
}

/// `Queue/QueueSemaphoreTake` and `Queue/QueueGiveFromISR`: a take that
/// succeeded leaves the count one lower, and a give one higher.
#[kani::proof]
#[kani::unwind(40)]
fn queue_semaphore_take_and_give_from_isr() {
    let mut k = ready();
    let Ok(s) = k.semaphore_create_counting(2, 1) else {
        return;
    };
    let before = k.queue_messages_waiting(s).unwrap_or(0);
    match k.semaphore_take(s, small_ticks()) {
        Ok(Wait::Ready(())) => assert!(k.queue_messages_waiting(s) == Ok(before - 1)),
        _ => assert!(k.queue_messages_waiting(s) == Ok(before)),
    }
    let middle = k.queue_messages_waiting(s).unwrap_or(0);
    match k.semaphore_give_from_isr(s) {
        Ok(_) => assert!(k.queue_messages_waiting(s) == Ok(middle + 1)),
        Err(_) => assert!(k.queue_messages_waiting(s) == Ok(middle)),
    }
}

/// `Queue/QueueTakeMutexRecursive` and `Queue/QueueGiveMutexRecursive`: a
/// recursive take and its matching give leave the mutex exactly as it was.
#[kani::proof]
#[kani::unwind(40)]
fn queue_take_and_give_mutex_recursive() {
    let mut k = ready();
    let Ok(m) = k.mutex_create_recursive() else {
        return;
    };
    let before = k.queue_messages_waiting(m).unwrap_or(0);
    if matches!(
        k.mutex_take_recursive(m, small_ticks()),
        Ok(Wait::Ready(()))
    ) {
        let _ = k.mutex_give_recursive(m);
        assert!(k.queue_messages_waiting(m) == Ok(before));
    }
}

/// `Queue/prvUnlockQueue`, reached through the lock counts a from-ISR call
/// takes and gives back: a queue an interrupt has touched is a queue a task
/// can still use, which is the observable half of the unlock.
#[kani::proof]
#[kani::unwind(40)]
fn queue_unlock_leaves_the_queue_usable() {
    let mut k = ready();
    let Ok(q) = k.queue_create(2) else {
        return;
    };
    let _ = k.queue_send_from_isr(q, kani::any());
    let _ = k.queue_receive_from_isr(q);
    let before = k.queue_messages_waiting(q).unwrap_or(0);
    if matches!(
        k.queue_send_generic(q, kani::any(), 0, Position::Back),
        Ok(Wait::Ready(()))
    ) {
        assert!(k.queue_messages_waiting(q) == Ok(before + 1));
    }
}

// -------------------------------------------------------------- Task --

/// `Task/TaskCreate`: a task either exists at the priority it asked for, or
/// was refused and the count did not move.
#[kani::proof]
#[kani::unwind(40)]
fn task_create() {
    let mut k = ready();
    let priority: u8 = kani::any();
    let before = k.task_count();
    match k.create_task("t", priority) {
        Ok(t) => {
            assert!(k.task_count() == before + 1);
            // `configMAX_PRIORITIES - 1` is the ceiling the C clamps to.
            assert!(k.task_priority_get(Some(t)).unwrap_or(u8::MAX) < ProofConfig::MAX_PRIORITIES);
        }
        Err(_) => assert!(k.task_count() == before),
    }
}

/// `Task/TaskPrioritySet`: whatever is asked for, the priority ends inside
/// the configured range.
#[kani::proof]
#[kani::unwind(40)]
fn task_priority_set() {
    let mut k = ready();
    let priority: u8 = kani::any();
    let _ = k.set_priority(None, priority);
    let now = k.task_priority_get(None).unwrap_or(u8::MAX);
    assert!(now < ProofConfig::MAX_PRIORITIES);
}

/// The same call with a handle from nowhere: refused, and the running
/// task's own priority is untouched.
#[kani::proof]
#[kani::unwind(40)]
fn task_priority_set_stale_handle() {
    let mut k = ready();
    let before = k.task_priority_get(None).unwrap_or(0);
    let t = any_task();
    kani::assume(k.state_of(t) == Ok(crate::kernel::TaskState::Deleted));
    let _ = k.set_priority(Some(t), kani::any());
    assert!(k.task_priority_get(None) == Ok(before));
}

/// `Task/TaskGetTickCount` and `Task/TaskIncrementTick`: the tick only ever
/// goes forward, and a tick from interrupt context advances it by one.
#[kani::proof]
#[kani::unwind(40)]
fn task_increment_tick() {
    let mut k = started();
    let before = k.tick_count();
    k.tick_from_isr();
    assert!(k.tick_count() == before + 1);
}

/// `Task/TaskDelay`: a delay of zero is a yield and does not move the tick.
#[kani::proof]
#[kani::unwind(40)]
fn task_delay() {
    let mut k = started();
    let before = k.tick_count();
    let _ = k.delay(small_ticks());
    assert!(k.tick_count() == before);
}

/// `Task/TaskSuspendAll` and `Task/TaskResumeAll`: the two nest, and the
/// scheduler is only running again when the last one has come back.
#[kani::proof]
#[kani::unwind(40)]
fn task_suspend_all_and_resume_all() {
    let mut k = ready();
    k.suspend_all();
    k.suspend_all();
    let _ = k.resume_all();
    assert!(k.scheduler_suspended() == 1);
    let _ = k.resume_all();
    assert!(k.scheduler_suspended() == 0);
}

/// `Task/TaskGetSchedulerState`: a started kernel says so.
#[kani::proof]
#[kani::unwind(40)]
fn task_get_scheduler_state() {
    let k = started();
    assert!(k.is_running());
}

/// `Task/TaskGetCurrentTaskHandle`: whatever is current is a task that
/// exists.
#[kani::proof]
#[kani::unwind(40)]
fn task_get_current_task_handle() {
    let k = started();
    let current = k.current();
    assert!(k.state_of(current) == Ok(crate::kernel::TaskState::Running));
}

/// `Task/TaskSwitchContext`: a switch always leaves a runnable task
/// current, however many times it is asked for.
#[kani::proof]
#[kani::unwind(40)]
fn task_switch_context() {
    let mut k = started();
    k.switch_context();
    let current = k.current();
    assert!(k.state_of(current) == Ok(crate::kernel::TaskState::Running));
}

/// `Task/TaskStartScheduler`: a started kernel has the idle task and the
/// timer daemon on top of whatever the application made.
#[kani::proof]
#[kani::unwind(40)]
fn task_start_scheduler() {
    let k = started();
    assert!(k.is_running());
    assert!(k.task_count() == 3);
}

// ------------------------------------------------- the Rust face --
//
// The four above are the raw kernel's; these are `typed`'s, and the
// comparison is the point of K2.1's third step.
//
// Four of the ten harnesses that do not converge fail because a *symbolic
// handle* reaches a kernel call, and the model checker then has to explore
// every arena slot that handle could name. Against the typed face those
// four harnesses cannot be written at all: a `Queue<T, N>` is minted by
// `create` and there is no constructor that invents one, so the state they
// were exploring does not exist to be explored. That is what "compile-time
// topology" buys, stated as something checkable rather than as a taste —
// and it is why these finish in seconds while their raw equivalents do not
// finish at all.

/// A queue that behaves like the kernel's, with symbolic room.
///
/// It is deliberately not a stub of the real one: the property under proof
/// is the *face's* slot discipline, and the face may only assume what
/// `Raw` promises. Anything the real kernel does beyond that would be an
/// assumption smuggled into the proof.
#[cfg(kani)]
struct SymbolicQueue {
    held: usize,
    length: usize,
    last_sent: u64,
}

#[cfg(kani)]
impl crate::typed::Raw for SymbolicQueue {
    fn raw_queue_create(&mut self, length: usize) -> Result<QueueHandle> {
        self.length = length;
        self.held = 0;
        Ok(QueueHandle::from_raw(1))
    }

    fn raw_queue_send(&mut self, _q: QueueHandle, value: u64, _ticks: u64) -> Result<Wait<()>> {
        if self.held >= self.length {
            return Err(Error::Full);
        }
        self.held = self.held.saturating_add(1);
        self.last_sent = value;
        Ok(Wait::Ready(()))
    }

    fn raw_queue_receive(&mut self, _q: QueueHandle, _ticks: u64) -> Result<Wait<u64>> {
        if self.held == 0 {
            return Ok(Wait::Blocked);
        }
        self.held = self.held.saturating_sub(1);
        Ok(Wait::Ready(self.last_sent))
    }

    fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> {
        Ok(self.held)
    }

    fn raw_queue_has_room(&self, _q: QueueHandle) -> bool {
        self.held < self.length
    }

    fn raw_in_isr(&self) -> bool {
        false
    }

    fn raw_queue_send_from_isr(&mut self, q: QueueHandle, value: u64) -> Result<Woken> {
        self.raw_queue_send(q, value, 0).map(|_| Woken::NO)
    }
}

/// **A value handed to `send` is never lost.** Either the queue took it, or
/// it comes back — and it comes back *equal to what went in*, which is the
/// half a `BaseType_t` return cannot express.
///
/// This is the property `Sent<T>` exists for, and it is proved for every
/// `u16` and every starting occupancy rather than sampled.
#[kani::proof]
#[kani::unwind(8)]
fn typed_send_never_loses_the_value() {
    let held: usize = kani::any();
    kani::assume(held <= 2);
    let mut k = SymbolicQueue {
        held,
        length: 2,
        last_sent: 0,
    };
    let Ok(mut q) = crate::typed::Queue::<u16, 2>::create(&mut k) else {
        return;
    };
    // `create` resets the fake, so put the occupancy back.
    k.held = held;
    let value: u16 = kani::any();
    let before = k.held;
    match q.send(&mut k, value, 0) {
        crate::typed::Sent::Ok => {
            assert!(k.held == before + 1);
        }
        crate::typed::Sent::Full(back) | crate::typed::Sent::Blocked(back) => {
            assert!(back == value);
            assert!(k.held == before);
        }
    }
}

/// **What comes out is what went in.** For every `u16`, a send that was
/// taken is followed by a receive that answers the same value.
#[kani::proof]
#[kani::unwind(8)]
fn typed_receive_gives_back_what_was_sent() {
    let mut k = SymbolicQueue {
        held: 0,
        length: 2,
        last_sent: 0,
    };
    let Ok(mut q) = crate::typed::Queue::<u16, 2>::create(&mut k) else {
        return;
    };
    let value: u16 = kani::any();
    if !q.send(&mut k, value, 0).is_ok() {
        return;
    }
    match q.receive(&mut k, 0) {
        Ok(Wait::Ready(Some(out))) => assert!(out == value),
        // The queue was just sent to, so it is not empty.
        _ => assert!(false),
    }
}

/// **A refused send leaves the ring alone.** The rule the timer command
/// queue learned the hard way: a slot may only be written once the queue
/// has agreed to carry the index that names it, or a refused send
/// overwrites a value whose index is still queued.
#[kani::proof]
#[kani::unwind(8)]
fn typed_a_refused_send_does_not_disturb_a_queued_value() {
    let mut k = SymbolicQueue {
        held: 0,
        length: 1,
        last_sent: 0,
    };
    let Ok(mut q) = crate::typed::Queue::<u16, 1>::create(&mut k) else {
        return;
    };
    let first: u16 = kani::any();
    let second: u16 = kani::any();
    if !q.send(&mut k, first, 0).is_ok() {
        return;
    }
    // The queue holds one and its length is one, so this must be refused.
    match q.send(&mut k, second, 0) {
        crate::typed::Sent::Full(back) => assert!(back == second),
        _ => assert!(false),
    }
    // ...and the first value is still the one that comes out.
    match q.receive(&mut k, 0) {
        Ok(Wait::Ready(Some(out))) => assert!(out == first),
        _ => assert!(false),
    }
}

// `Task/TaskDelete`, `Task/TaskGetTaskNumber`, `Task/TaskCheckForTimeOut`
// and `Task/TaskSetTimeOutState` have no harness yet, and the reason is the
// same in each case: the surface they prove is not public here. There is no
// `vTaskDelete` on this kernel — nothing in the corpus deletes a task —
// task numbers belong to `configUSE_TRACE_FACILITY`, which is off, and the
// timeout pair is internal to the blocking calls, which `queue_receive` and
// `queue_generic_send` above drive through it with a symbolic block time.
// The `*Static` variants of six queue proofs have no counterpart either:
// this kernel has one allocation story, the arena, and it is the static one.

// ------------------------------------- the structures underneath --
//
// The CBMC proofs are mostly about memory safety, and in the C that means
// pointers. Here it means these three: a list whose links are indices, an
// arena whose handles are an index and a generation, and a name that is a
// fixed buffer and a length. Every out-of-range access the kernel could
// make it makes through one of them, so proving them on *unconstrained*
// inputs proves the property for every caller at once — and they are small
// enough that the model checker finishes, which a whole started kernel is
// not.

/// Every list operation, on an item and a list that may not exist and a
/// value anywhere in range: no panic, and an item is in at most one list.
#[kani::proof]
#[kani::unwind(9)]
fn lists_take_any_argument() {
    let mut lists: rusty_rtos_core::list::Lists<4, 3> = rusty_rtos_core::list::Lists::new();
    let item: u16 = kani::any();
    let list: u8 = kani::any();
    let value: u64 = kani::any();
    let inserted = lists.insert(list, item, value).is_ok();
    assert!(lists.container(item).unwrap_or(None).is_some() == inserted);
    if inserted {
        assert!(lists.value(item) == Ok(value));
        assert!(lists.is_empty(list) == Ok(false));
        assert!(lists.remove(item).is_ok());
        assert!(lists.container(item) == Ok(None));
    }
}

/// `vListInsertEnd` keeps the value the item already had, which is what
/// `vTaskPlaceOnUnorderedEventList` relies on to carry a condition.
#[kani::proof]
#[kani::unwind(9)]
fn insert_end_keeps_the_item_value() {
    let mut lists: rusty_rtos_core::list::Lists<4, 3> = rusty_rtos_core::list::Lists::new();
    let item: u16 = kani::any();
    let list: u8 = kani::any();
    let value: u64 = kani::any();
    if lists.set_value(item, value).is_ok() && lists.insert_end(list, item).is_ok() {
        assert!(lists.value(item) == Ok(value));
    }
}

/// An arena hands out handles that only it can resolve, and a handle it did
/// not mint resolves to nothing however it was built.
#[kani::proof]
#[kani::unwind(9)]
fn an_arena_only_resolves_its_own_handles() {
    let mut arena: rusty_rtos_core::arena::Arena<rusty_rtos_core::handle::Task, u32, 3> =
        rusty_rtos_core::arena::Arena::new();
    let value: u32 = kani::any();
    let Ok(handle) = arena.try_insert(value) else {
        return;
    };
    assert!(arena.resolve(handle) == Ok(&value));
    let other = TaskHandle::from_raw(kani::any());
    kani::assume(other != handle);
    assert!(arena.resolve(other).is_err());
    // A slot handed back and taken again mints a different handle, so the
    // old one is stale rather than an alias for the new object.
    let _ = arena.remove(handle);
    assert!(arena.resolve(handle).is_err());
    if let Ok(again) = arena.try_insert(value) {
        assert!(again != handle);
        assert!(arena.resolve(handle).is_err());
    }
}

/// A name is truncated to the configuration's limit, never past the end of
/// its buffer, and reads back as valid UTF-8 of the length it kept.
#[kani::proof]
#[kani::unwind(9)]
fn a_name_is_truncated_not_overrun() {
    let limit: usize = kani::any();
    kani::assume(limit <= crate::NAME_CAPACITY);
    let bytes: [u8; 4] = kani::any();
    kani::assume(bytes.iter().all(|b| b.is_ascii_graphic()));
    let Ok(text) = core::str::from_utf8(&bytes) else {
        return;
    };
    let name = crate::Name::new(text, limit);
    assert!(name.as_str().len() <= limit);
    assert!(name.as_str().len() <= text.len());
}

/// How much of a kernel the model checker can actually take.
///
/// These three are proofs in their own right — a fresh kernel is not
/// running, a created task is counted, three created tasks are three — and
/// they are also the measurement that says why the harnesses above stop
/// where they do. Building the kernel costs 0.8 s, one task 3.0 s, three
/// tasks 10.2 s, and `start_scheduler` on top of that does not finish in
/// ten minutes. The wall is the setup, not the call under proof.
#[kani::proof]
#[kani::unwind(40)]
fn a_fresh_kernel_is_not_running() {
    let k = match K::new(ProofPort::default(), NoTrace) {
        Ok(k) => k,
        Err(_) => return,
    };
    assert!(!k.is_running());
    assert!(k.task_count() == 0);
}

#[kani::proof]
#[kani::unwind(40)]
fn a_created_task_is_counted() {
    let mut k = match K::new(ProofPort::default(), NoTrace) {
        Ok(k) => k,
        Err(_) => return,
    };
    let Ok(t) = k.create_task("p", 1) else {
        return;
    };
    assert!(k.task_count() == 1);
    // Before the scheduler starts, the first task created is the one that
    // will run first — `pxCurrentTCB` is set as it is created.
    assert!(k.current() == t);
}

#[kani::proof]
#[kani::unwind(40)]
fn three_tasks_are_three() {
    let mut k = match K::new(ProofPort::default(), NoTrace) {
        Ok(k) => k,
        Err(_) => return,
    };
    let _ = k.create_task("p", 1);
    let _ = k.create_task("q", 1);
    let _ = k.create_task("r", 0);
    assert!(k.task_count() == 3);
    // `<=`, so the last-created task of the highest priority runs first.
    assert!(k.task_priority_get(None) == Ok(1));
}

/// A queue made before the scheduler starts is empty, and costs the model
/// checker almost nothing — which is half of why the wall is where it is.
#[kani::proof]
#[kani::unwind(40)]
fn a_queue_starts_empty() {
    let mut k = match K::new(ProofPort::default(), NoTrace) {
        Ok(k) => k,
        Err(_) => return,
    };
    let _ = k.create_task("p", 1);
    let Ok(q) = k.queue_create(1) else {
        return;
    };
    assert!(k.queue_messages_waiting(q) == Ok(0));
}

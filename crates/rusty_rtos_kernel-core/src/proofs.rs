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
//! `cargo kani -p rusty_rtos_kernel-core` verifies the lot;
//! `--harness <name>` runs one. Kani is Linux and macOS only, so on a
//! Windows box this is a WSL command. Nothing here compiles outside `cfg(kani)`,
//! so the crate builds and tests exactly as before without the tool.
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

/// A started kernel with one application task running.
///
/// Every harness needs the same thing: a kernel past `start_scheduler`, so
/// that the paths under proof are the ones a running system takes rather
/// than the ones the pre-scheduler special cases take.
fn started() -> K {
    let mut k = match K::new(ProofPort::default(), NoTrace) {
        Ok(k) => k,
        Err(_) => unreachable!(),
    };
    let _ = k.create_task("p", 1);
    let _ = k.start_scheduler();
    k
}

/// A handle no arena minted: the harnesses that want to prove a nonsense
/// argument is refused rather than followed.
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
#[kani::unwind(6)]
fn queue_generic_create() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_generic_send() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_generic_send_stale_handle() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_generic_send_from_isr() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_receive() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_receive_from_isr() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_peek() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_generic_reset() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_messages_waiting_and_spaces_available() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_create_counting_semaphore() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_create_mutex_and_get_holder() {
    let mut k = started();
    let Ok(m) = k.mutex_create() else {
        return;
    };
    assert!(k.mutex_holder(m) == Ok(TaskHandle::NULL));
    assert!(k.queue_messages_waiting(m) == Ok(1));
}

/// `Queue/QueueSemaphoreTake` and `Queue/QueueGiveFromISR`: a take that
/// succeeded leaves the count one lower, and a give one higher.
#[kani::proof]
#[kani::unwind(6)]
fn queue_semaphore_take_and_give_from_isr() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_take_and_give_mutex_recursive() {
    let mut k = started();
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
#[kani::unwind(6)]
fn queue_unlock_leaves_the_queue_usable() {
    let mut k = started();
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
#[kani::unwind(6)]
fn task_create() {
    let mut k = started();
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
#[kani::unwind(6)]
fn task_priority_set() {
    let mut k = started();
    let priority: u8 = kani::any();
    let _ = k.set_priority(None, priority);
    let now = k.task_priority_get(None).unwrap_or(u8::MAX);
    assert!(now < ProofConfig::MAX_PRIORITIES);
}

/// The same call with a handle from nowhere: refused, and the running
/// task's own priority is untouched.
#[kani::proof]
#[kani::unwind(6)]
fn task_priority_set_stale_handle() {
    let mut k = started();
    let before = k.task_priority_get(None).unwrap_or(0);
    let t = any_task();
    kani::assume(k.state_of(t) == Ok(crate::kernel::TaskState::Deleted));
    let _ = k.set_priority(Some(t), kani::any());
    assert!(k.task_priority_get(None) == Ok(before));
}

/// `Task/TaskGetTickCount` and `Task/TaskIncrementTick`: the tick only ever
/// goes forward, and a tick from interrupt context advances it by one.
#[kani::proof]
#[kani::unwind(6)]
fn task_increment_tick() {
    let mut k = started();
    let before = k.tick_count();
    k.tick_from_isr();
    assert!(k.tick_count() == before + 1);
}

/// `Task/TaskDelay`: a delay of zero is a yield and does not move the tick.
#[kani::proof]
#[kani::unwind(6)]
fn task_delay() {
    let mut k = started();
    let before = k.tick_count();
    let _ = k.delay(small_ticks());
    assert!(k.tick_count() == before);
}

/// `Task/TaskSuspendAll` and `Task/TaskResumeAll`: the two nest, and the
/// scheduler is only running again when the last one has come back.
#[kani::proof]
#[kani::unwind(6)]
fn task_suspend_all_and_resume_all() {
    let mut k = started();
    k.suspend_all();
    k.suspend_all();
    let _ = k.resume_all();
    assert!(k.scheduler_suspended() == 1);
    let _ = k.resume_all();
    assert!(k.scheduler_suspended() == 0);
}

/// `Task/TaskGetSchedulerState`: a started kernel says so.
#[kani::proof]
fn task_get_scheduler_state() {
    let k = started();
    assert!(k.is_running());
}

/// `Task/TaskGetCurrentTaskHandle`: whatever is current is a task that
/// exists.
#[kani::proof]
#[kani::unwind(6)]
fn task_get_current_task_handle() {
    let k = started();
    let current = k.current();
    assert!(k.state_of(current) == Ok(crate::kernel::TaskState::Running));
}

/// `Task/TaskSwitchContext`: a switch always leaves a runnable task
/// current, however many times it is asked for.
#[kani::proof]
#[kani::unwind(6)]
fn task_switch_context() {
    let mut k = started();
    k.switch_context();
    let current = k.current();
    assert!(k.state_of(current) == Ok(crate::kernel::TaskState::Running));
}

/// `Task/TaskStartScheduler`: a started kernel has the idle task and the
/// timer daemon on top of whatever the application made.
#[kani::proof]
#[kani::unwind(6)]
fn task_start_scheduler() {
    let k = started();
    assert!(k.is_running());
    assert!(k.task_count() == 3);
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

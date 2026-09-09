//! `queue.c`: the queue, and the semaphore and mutex built on it.
//!
//! In FreeRTOS a semaphore *is* a queue with a zero-size item, and a mutex
//! is that plus a holder and priority inheritance. Kairos keeps the same
//! shape, because the trace does: a semaphore give fires `QUEUE_SEND` and a
//! mutex take fires `QUEUE_RECEIVE`, and a conformance diff would notice
//! any tidier arrangement.
//!
//! # A blocking call with no stack to block on
//!
//! `xQueueReceive( q, &v, 100 )` blocks: the C task stops inside the call,
//! and when it is woken it loops back to the top, re-checks the queue, and
//! either returns the item or blocks again. This kernel has no stack to
//! suspend, so the loop is turned inside out — a blocking call returns
//! [`Blocked`] and the caller invokes it again, unchanged, when it next
//! runs:
//!
//! ```ignore
//! // in a task body, at some `pc`:
//! match k.queue_receive(q, 100)? {
//!     Blocked => {}                 // stay at this pc; the kernel parked us
//!     Ready(value) => { ...; self.pc = next; }
//! }
//! ```
//!
//! The task is off the ready list when `Blocked` comes back, so the runner
//! will not step it again until the kernel wakes it — which is exactly when
//! the C thread would have resumed. What the C keeps on its stack across
//! the block (the timeout, the remaining ticks, whether inheritance
//! happened) this kernel keeps in the TCB, in [`Wait`], because that *is*
//! the stack frame.

use rusty_rtos_core::config::Config;
use rusty_rtos_core::error::{Error, Result};
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::port::Port;
use rusty_rtos_core::trace::{Event, Trace};

use crate::kernel::{Kernel, OwedTrace};

/// What a blocking call answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait<T> {
    /// The kernel parked the calling task. Leave the program counter where
    /// it is and make the same call again when the task next runs.
    Blocked,
    /// The call completed.
    Ready(T),
}

pub use Wait::{Blocked, Ready};

/// Where an item goes (`xCopyPosition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// `queueSEND_TO_BACK`.
    Back,
    /// `queueSEND_TO_FRONT`.
    Front,
}

/// What a queue is underneath (`ucQueueType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A queue of values.
    Queue,
    /// A binary or counting semaphore: a queue with a zero-size item, so
    /// only the count matters.
    Semaphore,
    /// A mutex: a one-deep semaphore with a holder and priority
    /// inheritance.
    Mutex,
    /// A recursive mutex: as above, and the holder may take it again.
    RecursiveMutex,
}

impl Kind {
    /// `uxItemSize == 0`: semaphores and mutexes carry a count, not data.
    pub(crate) const fn carries_data(self) -> bool {
        matches!(self, Self::Queue)
    }

    /// The two mutex kinds, which inherit priority.
    pub(crate) const fn is_mutex(self) -> bool {
        matches!(self, Self::Mutex | Self::RecursiveMutex)
    }
}

/// One queue: the C `Queue_t` minus the byte pointers. Storage is a range
/// of the kernel's shared slot pool, and the read and write cursors follow
/// C's `pcReadFrom` / `pcWriteTo` exactly — `pcReadFrom` points at the item
/// last read, which is what makes `queueSEND_TO_FRONT` a single step back.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Queue {
    pub(crate) base: usize,
    /// `uxLength`.
    pub(crate) length: usize,
    /// `uxMessagesWaiting`.
    pub(crate) waiting: usize,
    /// `pcReadFrom`, as an index: the item last read.
    pub(crate) read_from: usize,
    /// `pcWriteTo`, as an index: where the next back-send writes.
    pub(crate) write_to: usize,
    pub(crate) kind: Kind,
    /// `u.xSemaphore.xMutexHolder`.
    pub(crate) holder: TaskHandle,
    /// `u.xSemaphore.uxRecursiveCallCount`.
    pub(crate) recursions: u32,
}

impl Queue {
    pub(crate) const fn new(base: usize, length: usize, kind: Kind) -> Self {
        Self {
            base,
            length,
            waiting: 0,
            // `xQueueGenericReset`: pcReadFrom = pcHead + (length - 1) * itemSize.
            read_from: length.saturating_sub(1),
            write_to: 0,
            kind,
            holder: TaskHandle::NULL,
            recursions: 0,
        }
    }
}

impl<
    C: Config,
    P: Port,
    T: Trace,
    const TASKS: usize,
    const ITEMS: usize,
    const LISTS: usize,
    const QUEUES: usize,
    const SLOTS: usize,
> Kernel<C, P, T, TASKS, ITEMS, LISTS, QUEUES, SLOTS>
{
    // ------------------------------------------------------------ create --

    /// `xQueueCreate`.
    ///
    /// # Errors
    /// [`Error::Full`] when the queue arena or the shared slot pool is
    /// exhausted; [`Error::InvalidArgument`] for a zero length.
    pub fn queue_create(&mut self, length: usize) -> Result<QueueHandle> {
        self.new_queue(length, Kind::Queue)
    }

    /// `xSemaphoreCreateBinary`: a one-deep semaphore, created empty.
    ///
    /// # Errors
    /// As [`Kernel::queue_create`].
    pub fn semaphore_create_binary(&mut self) -> Result<QueueHandle> {
        self.new_queue(1, Kind::Semaphore)
    }

    /// `xSemaphoreCreateCounting`.
    ///
    /// # Errors
    /// As [`Kernel::queue_create`]; [`Error::InvalidArgument`] if the
    /// initial count exceeds the maximum.
    pub fn semaphore_create_counting(&mut self, max: usize, initial: usize) -> Result<QueueHandle> {
        if initial > max {
            return Err(Error::InvalidArgument);
        }
        let handle = self.new_queue(max, Kind::Semaphore)?;
        if let Ok(q) = self.queues.resolve_mut(handle) {
            q.waiting = initial;
        }
        Ok(handle)
    }

    /// `xSemaphoreCreateMutex`: created **available**, unlike a semaphore.
    ///
    /// # Errors
    /// As [`Kernel::queue_create`].
    pub fn mutex_create(&mut self) -> Result<QueueHandle> {
        self.new_mutex(Kind::Mutex)
    }

    /// `xSemaphoreCreateRecursiveMutex`.
    ///
    /// # Errors
    /// As [`Kernel::queue_create`].
    pub fn mutex_create_recursive(&mut self) -> Result<QueueHandle> {
        self.new_mutex(Kind::RecursiveMutex)
    }

    fn new_mutex(&mut self, kind: Kind) -> Result<QueueHandle> {
        let handle = self.new_queue(1, kind)?;
        // `prvInitialiseMutex` gives the mutex once so it starts available,
        // and it does so through `xQueueGenericSend` — which fires
        // `traceQUEUE_SEND`. A mutex therefore has a `QUEUE_SEND` line
        // immediately after its `QUEUE_CREATE`, before any task exists.
        let _ = self.queue_send_generic(handle, 0, 0, Position::Back)?;
        Ok(handle)
    }

    fn new_queue(&mut self, length: usize, kind: Kind) -> Result<QueueHandle> {
        if length == 0 {
            return Err(Error::InvalidArgument);
        }
        // A zero-item-size queue needs no storage; only real queues do.
        let slots = if kind.carries_data() { length } else { 0 };
        let base = self.slots_used;
        let end = base.checked_add(slots).ok_or(Error::Full)?;
        if end > SLOTS {
            return Err(Error::Full);
        }
        let handle = self
            .queues
            .try_insert(Queue::new(base, length, kind))
            .map_err(|_| Error::Full)?;
        self.slots_used = end;
        // `xQueueGenericCreate` takes the queue and its storage from the
        // heap before it initialises anything.
        self.account_for_allocation();
        // `xQueueGenericReset` runs in a critical section before
        // `traceQUEUE_CREATE` fires at the end of `prvInitialiseNewQueue`.
        self.enter_critical();
        self.exit_critical();
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace.event(
            tick,
            Event::QueueCreate {
                queue: handle,
                name: "",
                length,
            },
        );
        Ok(handle)
    }

    // ----------------------------------------------------------- getters --

    /// `uxQueueMessagesWaiting`, which takes a critical section.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn queue_messages_waiting(&mut self, queue: QueueHandle) -> Result<usize> {
        self.enter_critical();
        let n = self.queues.resolve(queue).map(|q| q.waiting);
        self.exit_critical();
        n
    }

    /// `uxSemaphoreGetCount`, which is the same thing.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn semaphore_count(&mut self, semaphore: QueueHandle) -> Result<usize> {
        self.queue_messages_waiting(semaphore)
    }

    /// `xSemaphoreGetMutexHolderFromISR`, which is
    /// `xQueueGetMutexHolderFromISR`: the same answer, read without a
    /// critical section because an ISR is already inside one.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn mutex_holder_from_isr(&mut self, mutex: QueueHandle) -> Result<TaskHandle> {
        self.queues.resolve(mutex).map(|q| q.holder)
    }

    /// `xSemaphoreGetMutexHolder`, which takes a critical section.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn mutex_holder(&mut self, mutex: QueueHandle) -> Result<TaskHandle> {
        self.enter_critical();
        let holder = self.queues.resolve(mutex).map(|q| q.holder);
        self.exit_critical();
        holder
    }

    // -------------------------------------------------------------- send --

    /// `xQueueSend` / `xQueueSendToBack`.
    ///
    /// # Errors
    /// [`Error::Full`] when the block time expires with the queue still
    /// full; [`Error::Gone`] for a stale handle.
    pub fn queue_send(&mut self, queue: QueueHandle, value: u64, ticks: u64) -> Result<Wait<()>> {
        self.queue_send_generic(queue, value, ticks, Position::Back)
    }

    /// `xQueueSendToFront`.
    ///
    /// # Errors
    /// As [`Kernel::queue_send`].
    pub fn queue_send_to_front(
        &mut self,
        queue: QueueHandle,
        value: u64,
        ticks: u64,
    ) -> Result<Wait<()>> {
        self.queue_send_generic(queue, value, ticks, Position::Front)
    }

    /// `xSemaphoreGive`: a send with no data.
    ///
    /// # Errors
    /// [`Error::Full`] when the semaphore is already at its maximum count.
    pub fn semaphore_give(&mut self, semaphore: QueueHandle) -> Result<Wait<()>> {
        self.queue_send_generic(semaphore, 0, 0, Position::Back)
    }

    /// `xQueueGenericSend`, one pass of its `for(;;)`.
    ///
    /// # Errors
    /// [`Error::Full`] on timeout; [`Error::Gone`] for a stale handle.
    pub fn queue_send_generic(
        &mut self,
        queue: QueueHandle,
        value: u64,
        ticks: u64,
        position: Position,
    ) -> Result<Wait<()>> {
        let caller = self.current;
        self.begin_wait(caller, queue, ticks)?;
        self.enter_critical();
        let snapshot = match self.queues.resolve(queue) {
            Ok(q) => *q,
            Err(e) => {
                self.exit_critical();
                return Err(e);
            }
        };
        if snapshot.waiting < snapshot.length {
            self.trace.note_exits(self.port.exits());
            let tick = self.tick;
            self.trace.event(tick, Event::QueueSend { queue, name: "" });
            let yield_required = self.copy_data_to_queue(queue, value, position)?;
            let receivers = Self::queue_receive_list(queue);
            let woke_higher = if self.lists.is_empty(receivers) == Ok(false) {
                self.remove_from_event_list(receivers)?
            } else {
                false
            };
            // queueYIELD_IF_USING_PREEMPTION(), inside the section.
            if woke_higher || (self.lists.is_empty(receivers) != Ok(false) && yield_required) {
                self.port_yield();
            }
            self.exit_critical();
            self.end_wait(caller);
            return Ok(Ready(()));
        }
        if self.remaining_ticks(caller) == 0 {
            self.exit_critical();
            self.trace_failure_or_owe(caller, OwedTrace::SendFailed(queue));
            self.end_wait(caller);
            return Err(Error::Full);
        }
        self.exit_critical();
        self.suspend_all();
        self.lock_queue();
        if self.check_for_timeout(caller) {
            self.unlock_queue(queue)?;
            let _ = self.resume_all();
            self.trace_failure_or_owe(caller, OwedTrace::SendFailed(queue));
            self.end_wait(caller);
            return Err(Error::Full);
        }
        if self.is_queue_full(queue) {
            self.trace.note_exits(self.port.exits());
            let tick = self.tick;
            self.trace
                .event(tick, Event::BlockingOnQueueSend { queue, name: "" });
            let ticks_left = self.remaining_ticks(caller);
            self.place_on_event_list(Self::queue_send_list(queue), ticks_left)?;
            self.unlock_queue(queue)?;
            if !self.resume_all() {
                self.yield_or_owe(caller);
            }
        } else {
            self.unlock_queue(queue)?;
            let _ = self.resume_all();
        }
        Ok(Blocked)
    }

    /// `prvCopyDataToQueue`; `true` when giving a mutex back lowered the
    /// giver's priority and a yield is therefore wanted.
    fn copy_data_to_queue(
        &mut self,
        queue: QueueHandle,
        value: u64,
        position: Position,
    ) -> Result<bool> {
        let snapshot = *self.queues.resolve(queue)?;
        let mut yield_required = false;
        if snapshot.kind.carries_data() {
            let (index, next_read, next_write) = match position {
                Position::Back => (
                    snapshot.write_to,
                    snapshot.read_from,
                    wrap_next(snapshot.write_to, snapshot.length),
                ),
                // Write at the item last read, then step back, so the next
                // read's pre-increment lands on it.
                Position::Front => (
                    snapshot.read_from,
                    wrap_prev(snapshot.read_from, snapshot.length),
                    snapshot.write_to,
                ),
            };
            if let Some(slot) = self.slots.get_mut(snapshot.base.saturating_add(index)) {
                *slot = value;
            }
            let q = self.queues.resolve_mut(queue)?;
            q.read_from = next_read;
            q.write_to = next_write;
        } else if snapshot.kind.is_mutex() {
            // Giving a mutex back: the holder drops any inherited priority.
            yield_required = self.priority_disinherit(snapshot.holder)?;
            self.queues.resolve_mut(queue)?.holder = TaskHandle::NULL;
        }
        let q = self.queues.resolve_mut(queue)?;
        q.waiting = q.waiting.saturating_add(1);
        Ok(yield_required)
    }

    // ----------------------------------------------------------- receive --

    /// `xQueueReceive`.
    ///
    /// # Errors
    /// [`Error::Empty`] when the block time expires with the queue still
    /// empty; [`Error::Gone`] for a stale handle.
    pub fn queue_receive(&mut self, queue: QueueHandle, ticks: u64) -> Result<Wait<u64>> {
        self.queue_take(queue, ticks, false)
    }

    /// `xQueuePeek`: read the head without removing it.
    ///
    /// # Errors
    /// As [`Kernel::queue_receive`].
    pub fn queue_peek(&mut self, queue: QueueHandle, ticks: u64) -> Result<Wait<u64>> {
        self.queue_take(queue, ticks, true)
    }

    /// `xSemaphoreTake`, which is `xQueueSemaphoreTake`: a receive with no
    /// data, and — on a mutex — with priority inheritance while it waits.
    ///
    /// # Errors
    /// [`Error::Empty`] on timeout; [`Error::Gone`] for a stale handle.
    pub fn semaphore_take(&mut self, semaphore: QueueHandle, ticks: u64) -> Result<Wait<()>> {
        match self.queue_take(semaphore, ticks, false)? {
            Blocked => Ok(Blocked),
            Ready(_) => Ok(Ready(())),
        }
    }

    /// The shared body of `xQueueReceive`, `xQueuePeek` and
    /// `xQueueSemaphoreTake` — one pass of the C `for(;;)`.
    fn queue_take(&mut self, queue: QueueHandle, ticks: u64, peek: bool) -> Result<Wait<u64>> {
        let caller = self.current;
        self.begin_wait(caller, queue, ticks)?;
        self.enter_critical();
        let snapshot = match self.queues.resolve(queue) {
            Ok(q) => *q,
            Err(e) => {
                self.exit_critical();
                return Err(e);
            }
        };
        if snapshot.waiting > 0 {
            let value = self.copy_data_from_queue(queue, peek)?;
            self.trace.note_exits(self.port.exits());
            let tick = self.tick;
            // `xQueuePeek` is its own function in the C with its own trace
            // macro; `xQueueSemaphoreTake` shares `xQueueReceive`'s.
            let event = if peek {
                Event::QueuePeek { queue, name: "" }
            } else {
                Event::QueueReceive { queue, name: "" }
            };
            self.trace.event(tick, event);
            if peek {
                // A peek wakes another *receiver*, not a sender: the item
                // is still there.
                let receivers = Self::queue_receive_list(queue);
                if self.lists.is_empty(receivers) == Ok(false)
                    && self.remove_from_event_list(receivers)?
                {
                    self.port_yield();
                }
            } else {
                if snapshot.kind.is_mutex() {
                    self.queues.resolve_mut(queue)?.holder = caller;
                    self.increment_mutexes_held(caller);
                }
                let senders = Self::queue_send_list(queue);
                if self.lists.is_empty(senders) == Ok(false)
                    && self.remove_from_event_list(senders)?
                {
                    self.port_yield();
                }
            }
            self.exit_critical();
            self.end_wait(caller);
            return Ok(Ready(value));
        }
        if self.remaining_ticks(caller) == 0 {
            self.exit_critical();
            // `traceQUEUE_PEEK_FAILED` is not one of the harness's hooks,
            // so a failed peek says nothing on either side.
            if !peek {
                self.trace_failure_or_owe(caller, OwedTrace::ReceiveFailed(queue));
            }
            self.end_wait(caller);
            return Err(Error::Empty);
        }
        self.exit_critical();
        self.suspend_all();
        self.lock_queue();
        if self.check_for_timeout(caller) {
            self.unlock_queue(queue)?;
            let _ = self.resume_all();
            if self.is_queue_empty(queue) {
                if snapshot.kind.is_mutex() && self.wait_inherited(caller) {
                    // `vTaskPriorityDisinheritAfterTimeout`: the holder
                    // keeps only what the still-waiting tasks justify.
                    self.enter_critical();
                    let highest = self.highest_waiting_priority(queue)?;
                    let holder = self.queues.resolve(queue).map(|q| q.holder)?;
                    self.priority_disinherit_after_timeout(holder, highest)?;
                    self.exit_critical();
                }
                if !peek {
                    self.trace_failure_or_owe(caller, OwedTrace::ReceiveFailed(queue));
                }
                self.end_wait(caller);
                return Err(Error::Empty);
            }
            return Ok(Blocked);
        }
        if self.is_queue_empty(queue) {
            let event = if peek {
                Event::BlockingOnQueuePeek { queue, name: "" }
            } else {
                Event::BlockingOnQueueReceive { queue, name: "" }
            };
            self.trace.note_exits(self.port.exits());
            let tick = self.tick;
            self.trace.event(tick, event);
            if snapshot.kind.is_mutex() {
                self.enter_critical();
                let holder = self.queues.resolve(queue).map(|q| q.holder)?;
                let inherited = self.priority_inherit(holder)?;
                self.exit_critical();
                if inherited {
                    self.set_wait_inherited(caller);
                }
            }
            let ticks_left = self.remaining_ticks(caller);
            self.place_on_event_list(Self::queue_receive_list(queue), ticks_left)?;
            self.unlock_queue(queue)?;
            if !self.resume_all() {
                self.yield_or_owe(caller);
            }
        } else {
            self.unlock_queue(queue)?;
            let _ = self.resume_all();
        }
        Ok(Blocked)
    }

    /// `prvCopyDataFromQueue`, with the peek variant putting the cursor
    /// back as `xQueuePeek` does.
    fn copy_data_from_queue(&mut self, queue: QueueHandle, peek: bool) -> Result<u64> {
        let snapshot = *self.queues.resolve(queue)?;
        if !snapshot.kind.carries_data() {
            if !peek {
                let q = self.queues.resolve_mut(queue)?;
                q.waiting = q.waiting.saturating_sub(1);
            }
            return Ok(0);
        }
        let next = wrap_next(snapshot.read_from, snapshot.length);
        let value = self
            .slots
            .get(snapshot.base.saturating_add(next))
            .copied()
            .unwrap_or(0);
        let q = self.queues.resolve_mut(queue)?;
        if peek {
            // `xQueuePeek` saves and restores `pcReadFrom`.
            q.read_from = snapshot.read_from;
        } else {
            q.read_from = next;
            q.waiting = q.waiting.saturating_sub(1);
        }
        Ok(value)
    }

    // ------------------------------------------------------ recursive mutex --

    /// `xSemaphoreTakeRecursive`.
    ///
    /// # Errors
    /// [`Error::Empty`] on timeout; [`Error::Gone`] for a stale handle.
    pub fn mutex_take_recursive(&mut self, mutex: QueueHandle, ticks: u64) -> Result<Wait<()>> {
        let caller = self.current;
        let holder = self.queues.resolve(mutex)?.holder;
        if holder == caller {
            let q = self.queues.resolve_mut(mutex)?;
            q.recursions = q.recursions.saturating_add(1);
            return Ok(Ready(()));
        }
        match self.semaphore_take(mutex, ticks)? {
            Blocked => Ok(Blocked),
            Ready(()) => {
                let q = self.queues.resolve_mut(mutex)?;
                q.recursions = q.recursions.saturating_add(1);
                Ok(Ready(()))
            }
        }
    }

    /// `xSemaphoreGiveRecursive`.
    ///
    /// # Errors
    /// [`Error::NotActive`] when the caller does not hold the mutex.
    pub fn mutex_give_recursive(&mut self, mutex: QueueHandle) -> Result<()> {
        let caller = self.current;
        if self.queues.resolve(mutex)?.holder != caller {
            return Err(Error::NotActive);
        }
        let remaining = {
            let q = self.queues.resolve_mut(mutex)?;
            q.recursions = q.recursions.saturating_sub(1);
            q.recursions
        };
        if remaining == 0 {
            let _ = self.semaphore_give(mutex)?;
        }
        Ok(())
    }

    // -------------------------------------------------------- queue locks --

    /// `prvLockQueue`: one critical section. The lock counters themselves
    /// only matter to an ISR, and the sim has none — but the section is
    /// where sim time passes, so it is here.
    fn lock_queue(&mut self) {
        self.enter_critical();
        self.exit_critical();
    }

    /// `prvUnlockQueue`: two critical sections, one per lock counter. The
    /// loops inside them wake tasks that an ISR queued while the lock was
    /// held, and on the sim there are none.
    fn unlock_queue(&mut self, _queue: QueueHandle) -> Result<()> {
        self.enter_critical();
        self.exit_critical();
        self.enter_critical();
        self.exit_critical();
        Ok(())
    }

    /// `vQueueWaitForMessageRestricted`: what the timer service task does
    /// when it has no timer to run — block on the command queue without
    /// suspending the scheduler.
    ///
    /// `wait_indefinitely` is the C third argument: when set the wait
    /// becomes `portMAX_DELAY` and the task goes to the suspended list
    /// rather than a delayed one. The queue locks around it are C's
    /// `prvLockQueue` and `prvUnlockQueue` — three critical sections, and
    /// therefore three sixteenths of a tick's worth of sim time.
    ///
    /// # Errors
    /// A list error surfaces as itself.
    pub fn wait_for_message_restricted(
        &mut self,
        queue: QueueHandle,
        ticks: u64,
        wait_indefinitely: bool,
    ) -> Result<()> {
        self.lock_queue();
        let empty = self
            .queues
            .resolve(queue)
            .map(|q| q.waiting == 0)
            .unwrap_or(false);
        if empty {
            // vTaskPlaceOnEventListRestricted
            let current = self.current;
            let event = Self::event_item(current);
            self.lists
                .insert_end(Self::queue_receive_list(queue), event)?;
            let ticks = if wait_indefinitely {
                Self::MAX_DELAY
            } else {
                ticks
            };
            let wake_at = self.tick.wrapping_add(ticks) & Self::MAX_DELAY;
            self.trace_task(current, |task, name| Event::TaskDelayUntil {
                task,
                name,
                wake_at,
            });
            self.add_current_task_to_delayed_list(ticks, wait_indefinitely)?;
        }
        self.unlock_queue(queue)?;
        Ok(())
    }

    /// `prvIsQueueEmpty`, critical section and all — it is one of the
    /// four the blocking path spends, and therefore a sixteenth of a tick
    /// every four calls.
    fn is_queue_empty(&mut self, queue: QueueHandle) -> bool {
        self.enter_critical();
        let empty = self
            .queues
            .resolve(queue)
            .map(|q| q.waiting == 0)
            .unwrap_or(true);
        self.exit_critical();
        empty
    }

    /// `prvIsQueueFull`, likewise.
    fn is_queue_full(&mut self, queue: QueueHandle) -> bool {
        self.enter_critical();
        let full = self
            .queues
            .resolve(queue)
            .map(|q| q.waiting >= q.length)
            .unwrap_or(false);
        self.exit_critical();
        full
    }

    /// `prvGetHighestPriorityOfWaitToReceiveList`.
    fn highest_waiting_priority(&self, queue: QueueHandle) -> Result<u8> {
        let list = Self::queue_receive_list(queue);
        match self.lists.head(list)? {
            // The event item's value is `configMAX_PRIORITIES - priority`.
            Some(item) => {
                let value = self.lists.value(item)?;
                let max = u64::from(C::MAX_PRIORITIES);
                Ok(u8::try_from(max.saturating_sub(value)).unwrap_or(0))
            }
            None => Ok(0),
        }
    }
}

/// The next index in a ring of `length`.
const fn wrap_next(index: usize, length: usize) -> usize {
    let next = index.saturating_add(1);
    if next >= length { 0 } else { next }
}

/// The previous index in a ring of `length`.
const fn wrap_prev(index: usize, length: usize) -> usize {
    match index.checked_sub(1) {
        Some(prev) => prev,
        None => length.saturating_sub(1),
    }
}

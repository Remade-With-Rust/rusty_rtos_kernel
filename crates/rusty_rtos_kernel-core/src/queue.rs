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
use rusty_rtos_core::hooks::TickHook;
use rusty_rtos_core::isr::Woken;
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
    /// `queueOVERWRITE`: write to the front of a length-one queue whether
    /// or not it already holds something, and leave the message count
    /// where it was rather than raising it.
    Overwrite,
}

/// What a queue is underneath (`ucQueueType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    // ORDER IS LOAD-BEARING, and only for the two predicates below. The kinds
    // that carry data are first and the two mutex kinds are last, so
    // `carries_data` and `is_mutex` are each a range the compiler can test
    // with one comparison instead of two. Nothing reads a discriminant, so
    // the order carries no other meaning.
    /// A queue of values.
    Queue,
    /// A queue set: a queue whose items are the handles of the queues in
    /// it. `xQueueSelectFromSet` is a receive from this one.
    Set,
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
        matches!(self, Self::Queue | Self::Set)
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
    /// `cTxLock`: [`UNLOCKED`] when no task holds the queue locked;
    /// otherwise how many sends an interrupt made while it was locked, and
    /// therefore how many receivers `prvUnlockQueue` still owes a wake-up.
    pub(crate) tx_lock: i8,
    /// `cRxLock`, the same for receives and the tasks waiting to send.
    pub(crate) rx_lock: i8,
    /// `pxQueueSetContainer`: the set this queue belongs to, if any. An
    /// item arriving here is announced there.
    pub(crate) set_container: QueueHandle,
}

/// `queueUNLOCKED`.
pub(crate) const UNLOCKED: i8 = -1;
/// `queueLOCKED_UNMODIFIED`.
pub(crate) const LOCKED_UNMODIFIED: i8 = 0;

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
            tx_lock: UNLOCKED,
            rx_lock: UNLOCKED,
            set_container: QueueHandle::NULL,
        }
    }
}

impl<
    C: Config,
    P: Port,
    T: Trace,
    H,
    const TASKS: usize,
    const ITEMS: usize,
    const LISTS: usize,
    const QUEUES: usize,
    const SLOTS: usize,
    const BUFFERS: usize,
    const BYTES: usize,
    const TIMERS: usize,
    const GROUPS: usize,
> Kernel<C, P, T, H, TASKS, ITEMS, LISTS, QUEUES, SLOTS, BUFFERS, BYTES, TIMERS, GROUPS>
where
    H: TickHook<Self>,
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

    /// `vQueueDelete`, and `vSemaphoreDelete`, which the C defines as the
    /// same function.
    ///
    /// # It traces nothing and still costs time
    ///
    /// `vQueueDelete` takes no critical section of its own — but it ends in
    /// `vPortFree( pxQueue )`, and every `heap_N.c` wraps free in
    /// `vTaskSuspendAll` / `xTaskResumeAll` exactly as it wraps malloc. So
    /// a delete costs **one outermost critical-section exit**, which under
    /// the sim contract is one unit of time, while emitting no trace line
    /// at all (this harness defines no `traceQUEUE_DELETE`).
    ///
    /// `AbortDelay`'s remake left the deletes out on the reasoning that an
    /// event-less call cannot move the trace. Every event still agreed and
    /// the exit column was one short from the first delete onwards. The
    /// sibling `event_group_delete` already carried this note; the queue
    /// had no delete at all.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn queue_delete(&mut self, queue: QueueHandle) -> Result<()> {
        self.queues.resolve(queue)?;
        let _ = self.queues.remove(queue);
        // `vPortFree( pxQueue )`.
        self.account_for_allocation();
        Ok(())
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

    /// `xQueueReset`, which is `xQueueGenericReset( q, pdFALSE )`: empty
    /// the queue and unlock it, waking one waiting sender if there is one.
    ///
    /// The event lists survive — only a brand-new queue re-initialises
    /// those — so a task blocked on this queue stays blocked, and the one
    /// woken here is woken because the reset made room.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn queue_reset(&mut self, queue: QueueHandle) -> Result<()> {
        self.enter_critical();
        let result = (|| {
            let length = self.queues.resolve(queue)?.length;
            {
                let q = self.queues.resolve_mut(queue)?;
                q.waiting = 0;
                q.write_to = 0;
                q.read_from = length.saturating_sub(1);
                q.rx_lock = UNLOCKED;
                q.tx_lock = UNLOCKED;
            }
            let senders = Self::queue_send_list(queue);
            if self.lists.is_empty(senders) == Ok(false) && self.remove_from_event_list(senders)? {
                // queueYIELD_IF_USING_PREEMPTION(), inside the section.
                self.port_yield();
            }
            Ok(())
        })();
        self.exit_critical();
        result
    }

    /// `uxQueueSpacesAvailable`: how many more items would fit.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn queue_spaces_available(&mut self, queue: QueueHandle) -> Result<usize> {
        self.enter_critical();
        let spaces = self
            .queues
            .resolve(queue)
            .map(|q| q.length.wrapping_sub(q.waiting));
        self.exit_critical();
        spaces
    }

    // --------------------------------------------------- queue sets --
    //
    // A queue set is a queue whose items are the handles of the queues in
    // it. Nothing is *moved* into the set: an item arriving on a member
    // queue puts that queue's handle on the set, and the task that wakes on
    // the set reads the handle and then reads the member queue itself. So a
    // set holds an announcement, not the data — which is why adding a queue
    // that already has items in it is refused, and why the two counts would
    // otherwise drift apart forever.

    /// `xQueueCreateSet`: a set that can hold `length` announcements.
    ///
    /// # Errors
    /// As [`Kernel::queue_create`].
    pub fn queue_create_set(&mut self, length: usize) -> Result<QueueHandle> {
        self.new_queue(length, Kind::Set)
    }

    /// `xQueueAddToSet`.
    ///
    /// `false` is the C's `pdFAIL`: the target is not a set, the queue is
    /// already in one, or it has items waiting that the set never saw.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn queue_add_to_set(&mut self, queue: QueueHandle, set: QueueHandle) -> Result<bool> {
        self.enter_critical();
        let result = (|| {
            if self.queues.resolve(set)?.kind != Kind::Set {
                return Ok(false);
            }
            let member = self.queues.resolve(queue)?;
            if member.set_container != QueueHandle::NULL || member.waiting != 0 {
                return Ok(false);
            }
            self.queues.resolve_mut(queue)?.set_container = set;
            Ok(true)
        })();
        self.exit_critical();
        result
    }

    /// `xQueueRemoveFromSet`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn queue_remove_from_set(&mut self, queue: QueueHandle, set: QueueHandle) -> Result<bool> {
        self.enter_critical();
        let result = (|| {
            let member = self.queues.resolve(queue)?;
            if member.set_container != set || member.waiting != 0 {
                return Ok(false);
            }
            self.queues.resolve_mut(queue)?.set_container = QueueHandle::NULL;
            Ok(true)
        })();
        self.exit_critical();
        result
    }

    /// `xQueueSelectFromSet`: which member queue has something on it.
    ///
    /// The C is one line — `xQueueReceive( xQueueSet, &xReturn, xTicksToWait )`
    /// — so this blocks, traces and costs exactly what a receive does, and
    /// `None` is the C's `NULL`.
    ///
    /// # Errors
    /// As [`Kernel::queue_receive`].
    pub fn queue_select_from_set(
        &mut self,
        set: QueueHandle,
        ticks: u64,
    ) -> Result<Wait<Option<QueueHandle>>> {
        match self.queue_receive(set, ticks) {
            Ok(Blocked) => Ok(Blocked),
            Ok(Ready(raw)) => {
                let handle = QueueHandle::from_raw(u32::try_from(raw).unwrap_or(0));
                Ok(Ready((handle != QueueHandle::NULL).then_some(handle)))
            }
            Err(Error::Empty) => Ok(Ready(None)),
            Err(e) => Err(e),
        }
    }

    /// `prvNotifyQueueSetContainer`: put `queue`'s handle on the set it
    /// belongs to. `true` when that woke a task that outranks the current
    /// one.
    fn notify_queue_set_container(&mut self, queue: QueueHandle) -> Result<bool> {
        let set = self.queues.resolve(queue)?.set_container;
        let container = *self.queues.resolve(set)?;
        if container.waiting >= container.length {
            // The C asserts this cannot happen and does nothing if it does.
            return Ok(false);
        }
        let tx_lock = container.tx_lock;
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        // `traceQUEUE_SET_SEND` is `traceQUEUE_SEND` unless a port says
        // otherwise, and this port does not.
        self.trace.event(
            tick,
            Event::QueueSend {
                queue: set,
                name: "",
            },
        );
        let mut woke =
            self.copy_data_to_queue(set, &container, u64::from(queue.to_raw()), Position::Back)?;
        if tx_lock == UNLOCKED {
            let receivers = Self::queue_receive_list(set);
            if self.lists.is_empty(receivers) == Ok(false)
                && self.remove_from_event_list(receivers)?
            {
                woke = true;
            }
        } else {
            self.increment_tx_lock(set, tx_lock);
        }
        Ok(woke)
    }

    /// `xQueueOverwrite`: write to a length-one queue whether or not it
    /// already holds something.
    ///
    /// The C is `xQueueGenericSend( q, item, 0, queueOVERWRITE )` — a send
    /// with no block time, because a queue that is always writable can
    /// never make the caller wait.
    ///
    /// # Errors
    /// As [`Kernel::queue_send`].
    pub fn queue_overwrite(&mut self, queue: QueueHandle, value: u64) -> Result<Wait<()>> {
        self.queue_send_generic(queue, value, 0, Position::Overwrite)
    }

    // ------------------------------------------------------- from an ISR --
    //
    // The `FromISR` half of the API. Three things make it a different
    // animal from the task half, and all three are visible in a trace:
    //
    //   * It never blocks and never yields. It reports that a higher
    //     priority task woke, by returning a [`Woken`], and the interrupt
    //     decides what to do about it on the way out.
    //   * Its critical section is `portSET_INTERRUPT_MASK_FROM_ISR`, not
    //     `portENTER_CRITICAL`. On the Posix port both are empty, so a call
    //     here costs no sim time at all — which is exactly what the C does
    //     and therefore what a matching trace requires.
    //   * If a task has the queue locked it may not touch the event lists.
    //     It counts what it did in `cTxLock` / `cRxLock` instead, and
    //     `prvUnlockQueue` pays the wake-ups back when the task is done.

    /// `xQueueGenericSendFromISR`.
    ///
    /// # Errors
    /// [`Error::Full`] when the queue is full and this is not an overwrite;
    /// [`Error::Gone`] for a stale handle.
    pub fn queue_send_generic_from_isr(
        &mut self,
        queue: QueueHandle,
        value: u64,
        position: Position,
    ) -> Result<Woken> {
        let mask = self.port.enter_critical_from_isr();
        let result = self.send_from_isr_locked(queue, value, position);
        self.port.exit_critical_from_isr(mask);
        result
    }

    fn send_from_isr_locked(
        &mut self,
        queue: QueueHandle,
        value: u64,
        position: Position,
    ) -> Result<Woken> {
        let snapshot = *self.queues.resolve(queue)?;
        if snapshot.waiting >= snapshot.length && position != Position::Overwrite {
            // `traceQUEUE_SEND_FROM_ISR_FAILED` is not one of the harness's
            // hooks, so a full queue says nothing on either side.
            return Err(Error::Full);
        }
        let tx_lock = snapshot.tx_lock;
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace
            .event(tick, Event::QueueSendFromIsr { queue, name: "" });
        let previously_waiting = snapshot.waiting;
        let _ = self.copy_data_to_queue(queue, &snapshot, value, position)?;
        let mut woken = Woken::NO;
        if tx_lock == UNLOCKED {
            if snapshot.set_container != QueueHandle::NULL {
                let overwrote = position == Position::Overwrite && previously_waiting > 0;
                if !overwrote && self.notify_queue_set_container(queue)? {
                    woken = Woken::YES;
                }
                return Ok(woken);
            }
            let receivers = Self::queue_receive_list(queue);
            if self.lists.is_empty(receivers) == Ok(false)
                && self.remove_from_event_list(receivers)?
            {
                woken = Woken::YES;
            }
        } else {
            self.increment_tx_lock(queue, tx_lock);
        }
        Ok(woken)
    }

    /// `xQueueSendToBackFromISR`.
    ///
    /// # Errors
    /// As [`Kernel::queue_send_generic_from_isr`].
    pub fn queue_send_from_isr(&mut self, queue: QueueHandle, value: u64) -> Result<Woken> {
        self.queue_send_generic_from_isr(queue, value, Position::Back)
    }

    /// `xQueueSendToFrontFromISR`.
    ///
    /// # Errors
    /// As [`Kernel::queue_send_generic_from_isr`].
    pub fn queue_send_to_front_from_isr(
        &mut self,
        queue: QueueHandle,
        value: u64,
    ) -> Result<Woken> {
        self.queue_send_generic_from_isr(queue, value, Position::Front)
    }

    /// `xQueueOverwriteFromISR`. Only for a queue of length one.
    ///
    /// # Errors
    /// As [`Kernel::queue_send_generic_from_isr`].
    pub fn queue_overwrite_from_isr(&mut self, queue: QueueHandle, value: u64) -> Result<Woken> {
        self.queue_send_generic_from_isr(queue, value, Position::Overwrite)
    }

    /// `xSemaphoreGiveFromISR`.
    ///
    /// # Errors
    /// As [`Kernel::queue_send_generic_from_isr`].
    pub fn semaphore_give_from_isr(&mut self, semaphore: QueueHandle) -> Result<Woken> {
        self.queue_send_generic_from_isr(semaphore, 0, Position::Back)
    }

    /// `xQueueReceiveFromISR`: the value, and whether a waiting sender that
    /// this made room for outranks the interrupted task.
    ///
    /// # Errors
    /// [`Error::Empty`] when the queue is empty; [`Error::Gone`] for a
    /// stale handle.
    pub fn queue_receive_from_isr(&mut self, queue: QueueHandle) -> Result<(u64, Woken)> {
        let mask = self.port.enter_critical_from_isr();
        let result = self.receive_from_isr_locked(queue);
        self.port.exit_critical_from_isr(mask);
        result
    }

    fn receive_from_isr_locked(&mut self, queue: QueueHandle) -> Result<(u64, Woken)> {
        let snapshot = *self.queues.resolve(queue)?;
        if snapshot.waiting == 0 {
            return Err(Error::Empty);
        }
        let rx_lock = snapshot.rx_lock;
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace
            .event(tick, Event::QueueReceiveFromIsr { queue, name: "" });
        let value = self.copy_data_from_queue(queue, &snapshot, false)?;
        let mut woken = Woken::NO;
        if rx_lock == UNLOCKED {
            let senders = Self::queue_send_list(queue);
            if self.lists.is_empty(senders) == Ok(false) && self.remove_from_event_list(senders)? {
                woken = Woken::YES;
            }
        } else {
            self.increment_rx_lock(queue, rx_lock);
        }
        Ok((value, woken))
    }

    /// `xQueuePeekFromISR`: the head without removing it.
    ///
    /// It wakes nobody and traces nothing — the C has no hook for it —
    /// which is why it returns a bare value rather than a [`Woken`].
    ///
    /// # Errors
    /// [`Error::Empty`] when the queue is empty; [`Error::Gone`] for a
    /// stale handle.
    pub fn queue_peek_from_isr(&mut self, queue: QueueHandle) -> Result<u64> {
        let mask = self.port.enter_critical_from_isr();
        let result = match self.queues.resolve(queue) {
            Ok(q) if q.waiting > 0 => {
                let snapshot = *q;
                self.copy_data_from_queue(queue, &snapshot, true)
            }
            Ok(_) => Err(Error::Empty),
            Err(e) => Err(e),
        };
        self.port.exit_critical_from_isr(mask);
        result
    }

    /// `prvIncrementQueueTxLock`, capped at the task count as the C caps it.
    fn increment_tx_lock(&mut self, queue: QueueHandle, tx_lock: i8) {
        let tasks = self.task_count();
        if let Ok(q) = self.queues.resolve_mut(queue) {
            if i64::from(tx_lock) < tasks as i64 {
                q.tx_lock = tx_lock.saturating_add(1);
            }
        }
    }

    /// `prvIncrementQueueRxLock`.
    fn increment_rx_lock(&mut self, queue: QueueHandle, rx_lock: i8) {
        let tasks = self.task_count();
        if let Ok(q) = self.queues.resolve_mut(queue) {
            if i64::from(rx_lock) < tasks as i64 {
                q.rx_lock = rx_lock.saturating_add(1);
            }
        }
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
        self.enter_critical();
        let snapshot = match self.queues.resolve(queue) {
            Ok(q) => *q,
            Err(e) => {
                self.exit_critical();
                return Err(e);
            }
        };
        // The wait frame is set where the C sets it: `xQueueReceive` and
        // `xQueueGenericSend` call `vTaskInternalSetTimeOutState` only
        // after finding the queue unusable AND the block time non-zero,
        // guarded by `xEntryTimeSet`. Setting it up here instead meant
        // every *successful* call wrote a six-field frame and then had
        // `end_wait` wipe it on the way out.
        //
        // It also has to be after the resolve, so that a handle naming
        // nothing leaves the caller exactly as it found it -- which is
        // what `configASSERT( pxQueue )` means in the C.
        // `( uxMessagesWaiting < uxLength ) || ( xCopyPosition == queueOVERWRITE )`
        if snapshot.waiting < snapshot.length || position == Position::Overwrite {
            self.trace.note_exits(self.port.exits());
            let tick = self.tick;
            self.trace.event(tick, Event::QueueSend { queue, name: "" });
            let previously_waiting = snapshot.waiting;
            let yield_required = self.copy_data_to_queue(queue, &snapshot, value, position)?;
            if snapshot.set_container != QueueHandle::NULL {
                // A queue in a set announces the arrival there, not here.
                // An overwrite of an item that was already present is not an
                // arrival: the count did not change, so the set is not told.
                let overwrote = position == Position::Overwrite && previously_waiting > 0;
                if !overwrote && self.notify_queue_set_container(queue)? {
                    self.port_yield();
                }
                self.exit_critical();
                self.end_wait(caller);
                return Ok(Ready(()));
            }
            let receivers = Self::queue_receive_list(queue);
            let woke_higher = if self.lists.is_empty(receivers) == Ok(false) {
                self.remove_from_event_list(receivers)?
            } else {
                false
            };
            // queueYIELD_IF_USING_PREEMPTION(), inside the section.
            // `yield_required` first: it is false on every send that is
            // not a mutex give, and it is a local, so on the common path
            // the list is not read a second time at all. Both operands are
            // pure, so the order is free to choose. (The read cannot be
            // hoisted above the removal above it -- removing the last
            // waiter is exactly what changes the answer.)
            if woke_higher || (yield_required && self.lists.is_empty(receivers) != Ok(false)) {
                self.port_yield();
            }
            self.exit_critical();
            self.end_wait(caller);
            return Ok(Ready(()));
        }
        if let Err(e) = self.begin_wait(caller, queue, ticks) {
            self.exit_critical();
            return Err(e);
        }
        if self.remaining_ticks(caller) == 0 {
            self.exit_critical();
            self.trace_failure_or_owe(caller, OwedTrace::SendFailed(queue));
            self.end_wait(caller);
            return Err(Error::Full);
        }
        self.exit_critical();
        self.suspend_all();
        self.lock_queue(queue);
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
    ///
    /// This resolves rather than taking the caller's snapshot, and
    /// [`Kernel::copy_data_from_queue`] does the opposite. That is not an
    /// inconsistency: both forms were measured on both helpers and they
    /// disagree, because the writer's caller does not keep its snapshot
    /// live across the call and the reader's does.
    ///
    /// In line on purpose. `queue_send_generic` is its only caller, and out
    /// of line it paid a call and a frame on all 48,000 sends to move one
    /// value into a slot the caller had already resolved. Its reader twin
    /// was already being inlined by LLVM; this one was over the size
    /// threshold and was not.
    #[inline(always)]
    fn copy_data_to_queue(
        &mut self,
        queue: QueueHandle,
        snapshot: &Queue,
        value: u64,
        position: Position,
    ) -> Result<bool> {
        // The snapshot comes from the caller, which has already resolved this
        // handle -- and nothing between that resolve and this call can reach
        // the arena. Resolving again here made every send pay for the same
        // four checks twice. `copy_data_from_queue` has always taken its
        // snapshot this way.
        let mut yield_required = false;
        // `prvCopyDataToQueue` counts the item in — except on an overwrite
        // of a queue that already held one, where it decrements first so
        // the increment cancels and the count stays where it was.
        //
        // Decided once, up here, so that each arm below can fold the count
        // into the resolve it is already making. Deciding it after the arms
        // is what forced a further resolve for one field.
        let counted = !(position == Position::Overwrite && snapshot.waiting > 0);
        if snapshot.kind.carries_data() {
            let (index, next_read, next_write) = match position {
                Position::Back => (
                    snapshot.write_to,
                    snapshot.read_from,
                    wrap_next(snapshot.write_to, snapshot.length),
                ),
                // Write at the item last read, then step back, so the next
                // read's pre-increment lands on it. An overwrite takes the
                // same path; only the message count differs.
                Position::Front | Position::Overwrite => (
                    snapshot.read_from,
                    wrap_prev(snapshot.read_from, snapshot.length),
                    snapshot.write_to,
                ),
            };
            // `base + index` is below `base + length`, which the geometry
            // put inside `SLOTS`; and the `get_mut` refuses anything it is
            // not, so nothing rests on the arithmetic either way.
            if let Some(slot) = self.slots.get_mut(snapshot.base.wrapping_add(index)) {
                *slot = value;
            }
            let q = self.queues.resolve_mut(queue)?;
            q.read_from = next_read;
            q.write_to = next_write;
            if counted {
                q.waiting = q.waiting.wrapping_add(1);
            }
        } else if snapshot.kind.is_mutex() {
            // Giving a mutex back: the holder drops any inherited
            // priority. The disinherit touches the holder's TCB, not this
            // queue, so the resolve after it is still the first one.
            yield_required = self.priority_disinherit(snapshot.holder)?;
            let q = self.queues.resolve_mut(queue)?;
            q.holder = TaskHandle::NULL;
            if counted {
                q.waiting = q.waiting.wrapping_add(1);
            }
        } else if counted {
            // A counting or binary semaphore: only the count moves.
            let q = self.queues.resolve_mut(queue)?;
            q.waiting = q.waiting.wrapping_add(1);
        }
        Ok(yield_required)
    }

    // ----------------------------------------------------------- receive --

    /// `xQueueReceive`.
    ///
    /// # Errors
    /// [`Error::Empty`] when the block time expires with the queue still
    /// empty; [`Error::Gone`] for a stale handle.
    pub fn queue_receive(&mut self, queue: QueueHandle, ticks: u64) -> Result<Wait<u64>> {
        self.queue_take::<false>(queue, ticks)
    }

    /// `xQueuePeek`: read the head without removing it.
    ///
    /// # Errors
    /// As [`Kernel::queue_receive`].
    pub fn queue_peek(&mut self, queue: QueueHandle, ticks: u64) -> Result<Wait<u64>> {
        self.queue_take::<true>(queue, ticks)
    }

    /// `xSemaphoreTake`, which is `xQueueSemaphoreTake`: a receive with no
    /// data, and — on a mutex — with priority inheritance while it waits.
    ///
    /// # Errors
    /// [`Error::Empty`] on timeout; [`Error::Gone`] for a stale handle.
    pub fn semaphore_take(&mut self, semaphore: QueueHandle, ticks: u64) -> Result<Wait<()>> {
        match self.queue_take::<false>(semaphore, ticks)? {
            Blocked => Ok(Blocked),
            Ready(_) => Ok(Ready(())),
        }
    }

    /// The shared body of `xQueueReceive`, `xQueuePeek` and
    /// `xQueueSemaphoreTake` — one pass of the C `for(;;)`.
    /// `PEEK` is a const because it is one at every call site: a receive
    /// passes `false`, a peek `true`, and a semaphore take `false`. As a
    /// parameter it cost an argument to set up and a branch at each of the
    /// ten places this function and its copy helper consult it.
    /// In line on purpose, into all three of `xQueueReceive`, `xQueuePeek`
    /// and `xSemaphoreTake`.
    ///
    /// Out of line it charged eight instructions of prologue and ten of
    /// epilogue to every take -- 48,000 of them in `khot-ir`, which is 7.6%
    /// of that instrument -- so that three wrappers could each hand it two
    /// arguments and return its result unchanged. LLVM declined on size; the
    /// frame is the reason to overrule that.
    ///
    /// The cost is stated rather than hidden: `kernel-ir` pays 214,804 for
    /// the larger wrappers. Splitting the body from the symbol does not
    /// recover it -- outlining at the semaphore site alone leaves `kernel-ir`
    /// 262,828 up and gives back 427,983 of `khot-ir`'s win, and outlining at
    /// the receive site leaves it 190,238 up and gives back 1,104,183 -- so
    /// the regression is the wrappers growing, not any one call site.
    #[inline(always)]
    fn queue_take<const PEEK: bool>(
        &mut self,
        queue: QueueHandle,
        ticks: u64,
    ) -> Result<Wait<u64>> {
        let caller = self.current;
        self.enter_critical();
        let snapshot = match self.queues.resolve(queue) {
            Ok(q) => *q,
            Err(e) => {
                self.exit_critical();
                return Err(e);
            }
        };
        // The wait frame is set where the C sets it: `xQueueReceive` and
        // `xQueueGenericSend` call `vTaskInternalSetTimeOutState` only
        // after finding the queue unusable AND the block time non-zero,
        // guarded by `xEntryTimeSet`. Setting it up here instead meant
        // every *successful* call wrote a six-field frame and then had
        // `end_wait` wipe it on the way out.
        //
        // It also has to be after the resolve, so that a handle naming
        // nothing leaves the caller exactly as it found it -- which is
        // what `configASSERT( pxQueue )` means in the C.
        if snapshot.waiting > 0 {
            let value = self.copy_data_from_queue(queue, &snapshot, PEEK)?;
            self.trace.note_exits(self.port.exits());
            let tick = self.tick;
            // `xQueuePeek` is its own function in the C with its own trace
            // macro; `xQueueSemaphoreTake` shares `xQueueReceive`'s.
            let event = if PEEK {
                Event::QueuePeek { queue, name: "" }
            } else {
                Event::QueueReceive { queue, name: "" }
            };
            self.trace.event(tick, event);
            if PEEK {
                // A PEEK wakes another *receiver*, not a sender: the item
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
        if let Err(e) = self.begin_wait(caller, queue, ticks) {
            self.exit_critical();
            return Err(e);
        }
        if self.remaining_ticks(caller) == 0 {
            self.exit_critical();
            // `traceQUEUE_PEEK_FAILED` is not one of the harness's hooks,
            // so a failed PEEK says nothing on either side.
            if !PEEK {
                self.trace_failure_or_owe(caller, OwedTrace::ReceiveFailed(queue));
            }
            self.end_wait(caller);
            return Err(Error::Empty);
        }
        self.exit_critical();
        self.suspend_all();
        self.lock_queue(queue);
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
                if !PEEK {
                    self.trace_failure_or_owe(caller, OwedTrace::ReceiveFailed(queue));
                }
                self.end_wait(caller);
                return Err(Error::Empty);
            }
            return Ok(Blocked);
        }
        if self.is_queue_empty(queue) {
            let event = if PEEK {
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
    /// `prvCopyDataFromQueue`, over a snapshot its caller already took.
    ///
    /// Every caller resolves this queue to decide whether there is
    /// anything to read; this used to resolve it again to do the reading.
    /// The snapshot cannot be stale — nothing between the two touches this
    /// queue, and both are inside the same critical section.
    ///
    /// The same change to [`Kernel::copy_data_to_queue`] is **refused**,
    /// and the pair is why this bench exists: threading the snapshot into
    /// the writer is worth -245,389 Ir inside it and +211,717 in
    /// `queue_send_generic`, because a `&Queue` argument forces the
    /// caller's local to be addressable and it stops living in registers.
    /// Here the caller is `queue_take`, which keeps its snapshot alive
    /// across the call anyway, so there is no spill to pay for.
    fn copy_data_from_queue(
        &mut self,
        queue: QueueHandle,
        snapshot: &Queue,
        peek: bool,
    ) -> Result<u64> {
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
            .get(snapshot.base.wrapping_add(next))
            .copied()
            .unwrap_or(0);
        // `xQueuePeek` saves and restores `pcReadFrom` — which is to
        // say it leaves it exactly as it found it. Writing it back stored
        // the value already there, and the resolve that reached it was a
        // whole handle validation for a no-op.
        if !peek {
            let q = self.queues.resolve_mut(queue)?;
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
        // One resolve reads the holder and counts the re-entry.
        // `resolve_mut` runs the same four checks and answers the same
        // errors, so asking twice only asked twice.
        {
            let q = self.queues.resolve_mut(mutex)?;
            if q.holder == caller {
                q.recursions = q.recursions.wrapping_add(1);
                return Ok(Ready(()));
            }
        }
        match self.semaphore_take(mutex, ticks)? {
            Blocked => Ok(Blocked),
            Ready(()) => {
                let q = self.queues.resolve_mut(mutex)?;
                q.recursions = q.recursions.wrapping_add(1);
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
        // One resolve, as in the take above.
        let remaining = {
            let q = self.queues.resolve_mut(mutex)?;
            if q.holder != caller {
                return Err(Error::NotActive);
            }
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
    /// `prvLockQueue`: stop interrupts taking tasks off this queue's event
    /// lists while a task walks them. An interrupt that finds the queue
    /// locked counts what it did instead, and [`Kernel::unlock_queue`] pays
    /// it back.
    fn lock_queue(&mut self, queue: QueueHandle) {
        self.enter_critical();
        if let Ok(q) = self.queues.resolve_mut(queue) {
            if q.rx_lock == UNLOCKED {
                q.rx_lock = LOCKED_UNMODIFIED;
            }
            if q.tx_lock == UNLOCKED {
                q.tx_lock = LOCKED_UNMODIFIED;
            }
        }
        self.exit_critical();
    }

    /// `prvUnlockQueue`: two critical sections, one per lock counter. The
    /// loops inside them wake tasks that an ISR queued while the lock was
    /// held, and on the sim there are none.
    /// `prvUnlockQueue`: two critical sections, one per lock count, each
    /// waking one task per send or receive an interrupt made while the
    /// queue was locked. The yields are *missed* yields — pended, not
    /// taken — because a task is walking the list.
    fn unlock_queue(&mut self, queue: QueueHandle) -> Result<()> {
        self.enter_critical();
        {
            // One resolve for both fields. They were two, and a resolve
            // is a null test, a bounds check, a generation compare and an
            // `Option` unwrap -- all of it repeated to reach the same slot.
            let (mut tx_lock, container) = self
                .queues
                .resolve(queue)
                .map_or((UNLOCKED, QueueHandle::NULL), |q| {
                    (q.tx_lock, q.set_container)
                });
            while tx_lock > LOCKED_UNMODIFIED {
                if container != QueueHandle::NULL {
                    if self.notify_queue_set_container(queue)? {
                        self.missed_yield();
                    }
                    tx_lock = tx_lock.saturating_sub(1);
                    continue;
                }
                let receivers = Self::queue_receive_list(queue);
                if self.lists.is_empty(receivers) == Ok(true) {
                    break;
                }
                if self.remove_from_event_list(receivers)? {
                    self.missed_yield();
                }
                tx_lock = tx_lock.saturating_sub(1);
            }
            if let Ok(q) = self.queues.resolve_mut(queue) {
                q.tx_lock = UNLOCKED;
            }
        }
        self.exit_critical();
        self.enter_critical();
        {
            let mut rx_lock = self
                .queues
                .resolve(queue)
                .map(|q| q.rx_lock)
                .unwrap_or(UNLOCKED);
            while rx_lock > LOCKED_UNMODIFIED {
                let senders = Self::queue_send_list(queue);
                if self.lists.is_empty(senders) == Ok(true) {
                    break;
                }
                if self.remove_from_event_list(senders)? {
                    self.missed_yield();
                }
                rx_lock = rx_lock.saturating_sub(1);
            }
            if let Ok(q) = self.queues.resolve_mut(queue) {
                q.rx_lock = UNLOCKED;
            }
        }
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
        self.lock_queue(queue);
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
    // `wrapping_add`, and it is the SAME function, not an approximation
    // of it: at `usize::MAX` saturating yields `usize::MAX`, which is
    // `>= length`, so the caller gets 0 -- and wrapping yields 0, which is
    // `< length`, so the caller gets 0 as well. Every other index agrees
    // without argument.
    let next = index.wrapping_add(1);
    if next >= length { 0 } else { next }
}

/// The previous index in a ring of `length`.
const fn wrap_prev(index: usize, length: usize) -> usize {
    match index.checked_sub(1) {
        Some(prev) => prev,
        None => length.saturating_sub(1),
    }
}

/// The Rust face's raw surface: see [`crate::typed`].
///
/// Every method here is one of the kernel's own calls under another name.
/// The one that is not — [`Raw::raw_queue_has_room`] — is the kernel
/// reading its own queue rather than a task asking, so it takes no
/// critical section and costs no sim time, which is what lets the typed
/// face be free.
impl<
    C: Config,
    P: Port,
    T: Trace,
    H,
    const TASKS: usize,
    const ITEMS: usize,
    const LISTS: usize,
    const QUEUES: usize,
    const SLOTS: usize,
    const BUFFERS: usize,
    const BYTES: usize,
    const TIMERS: usize,
    const GROUPS: usize,
> crate::typed::Raw
    for Kernel<C, P, T, H, TASKS, ITEMS, LISTS, QUEUES, SLOTS, BUFFERS, BYTES, TIMERS, GROUPS>
where
    H: TickHook<Self>,
{
    fn raw_queue_create(&mut self, length: usize) -> Result<QueueHandle> {
        self.queue_create(length)
    }

    fn raw_queue_send(&mut self, queue: QueueHandle, value: u64, ticks: u64) -> Result<Wait<()>> {
        self.queue_send(queue, value, ticks)
    }

    fn raw_queue_receive(&mut self, queue: QueueHandle, ticks: u64) -> Result<Wait<u64>> {
        self.queue_receive(queue, ticks)
    }

    fn raw_queue_messages_waiting(&mut self, queue: QueueHandle) -> Result<usize> {
        self.queue_messages_waiting(queue)
    }

    fn raw_queue_has_room(&self, queue: QueueHandle) -> bool {
        self.queues
            .resolve(queue)
            .is_ok_and(|q| q.waiting < q.length)
    }

    fn raw_in_isr(&self) -> bool {
        self.port.in_isr()
    }

    fn raw_queue_send_from_isr(&mut self, queue: QueueHandle, value: u64) -> Result<Woken> {
        self.queue_send_from_isr(queue, value)
    }

    fn raw_mutex_create(&mut self) -> Result<QueueHandle> {
        self.mutex_create()
    }

    fn raw_mutex_take(&mut self, mutex: QueueHandle, ticks: u64) -> Result<Wait<()>> {
        self.semaphore_take(mutex, ticks)
    }

    fn raw_mutex_give(&mut self, mutex: QueueHandle) -> Result<()> {
        self.semaphore_give(mutex).map(|_| ())
    }
}

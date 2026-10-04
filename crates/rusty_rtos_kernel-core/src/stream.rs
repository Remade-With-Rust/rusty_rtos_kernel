//! `stream_buffer.c`: a byte ring one task writes and another reads.
//!
//! A stream buffer is the only kernel object here that is not built on a
//! queue and not built on an event list. It is a ring of bytes plus two
//! task handles, and a task waiting on one blocks on a **task
//! notification** rather than on an event list — which is why the
//! notification API had to exist before this file could.
//!
//! Two shapes share the code:
//!
//! * a **stream buffer** passes bytes, and a reader wakes when at least
//!   the trigger level of them have arrived;
//! * a **message buffer** passes whole messages, by writing the length in
//!   front of each one. A read returns one message or nothing.
//!
//! The length prefix is `sizeof( configMESSAGE_BUFFER_LENGTH_TYPE )` wide,
//! which is `size_t` unless a configuration says otherwise — eight bytes on
//! the machine the oracle runs on. That width is in the arithmetic of every
//! message send, so it is a [`Config`] const here rather than a guess.

use core::num::NonZeroU64;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::error::{Error, Result};
use rusty_rtos_core::handle::{QueueHandle, StreamBufferHandle, TaskHandle};
use rusty_rtos_core::hooks::TickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::trace::{Event, Trace};

use crate::kernel::{Kernel, NotifyAction, OwedTrace};
use crate::queue::{Blocked, Ready, Wait};

/// One stream or message buffer.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StreamBuffer {
    /// Where this buffer's bytes start in the kernel's byte arena.
    pub(crate) base: usize,
    /// `xLength`: the ring is one byte longer than the buffer asked for,
    /// because a ring that is full and a ring that is empty would otherwise
    /// look the same.
    pub(crate) length: usize,
    /// `xTriggerLevelBytes`.
    pub(crate) trigger: usize,
    /// `xHead`: where the next byte is written.
    pub(crate) head: usize,
    /// `xTail`: where the next byte is read.
    pub(crate) tail: usize,
    /// `xTaskWaitingToReceive`.
    pub(crate) waiting_to_receive: TaskHandle,
    /// `xTaskWaitingToSend`.
    pub(crate) waiting_to_send: TaskHandle,
    /// `sbFLAGS_IS_MESSAGE_BUFFER`.
    pub(crate) is_message: bool,
    /// `uxNotificationIndex`.
    pub(crate) notify_index: usize,
}

impl StreamBuffer {
    /// `prvBytesInBuffer`.
    ///
    /// The C adds the length before subtracting the tail so the unsigned
    /// arithmetic cannot go below zero, then folds the extra length back
    /// out. These are the C's own operations, and they need no checks for
    /// the same reason the C's do not.
    ///
    /// `head` and `tail` are kept below `length` by the wrap arithmetic that
    /// writes them, and `length` is at least 1. So `length + head` is at
    /// most `2 * length - 1` and cannot overflow; `length + head - tail` is
    /// at least `head + 1` and cannot underflow; and the fold only runs when
    /// `count >= length`, where `count` is at most `2 * length - 1`, so it
    /// lands back inside `0..length`.
    #[inline]
    pub(crate) const fn bytes_in_buffer(&self) -> usize {
        let mut count = self.length.wrapping_add(self.head).wrapping_sub(self.tail);
        if count >= self.length {
            count = count.wrapping_sub(self.length);
        }
        count
    }

    /// `xStreamBufferSpacesAvailable`: one less than the gap, because the
    /// ring keeps a spare byte so full and empty do not look alike.
    ///
    /// Bounded as [`StreamBuffer::bytes_in_buffer`] is, with one more step:
    /// `length + tail - head` is at least 1, because `head` is at most
    /// `length - 1`, so taking the spare byte off it cannot underflow.
    #[inline]
    pub(crate) const fn spaces_available(&self) -> usize {
        let mut space = self
            .length
            .wrapping_add(self.tail)
            .wrapping_sub(self.head)
            .wrapping_sub(1);
        if space >= self.length {
            space = space.wrapping_sub(self.length);
        }
        space
    }

    /// `prvBytesInBufferMeetTriggerLevel`, for the two shapes that are not
    /// a batching buffer.
    pub(crate) const fn meets_trigger(&self, bytes: usize) -> bool {
        bytes >= self.trigger
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
    const TIMER_CMDS: usize,
> Kernel<C, P, T, H, TASKS, ITEMS, LISTS, QUEUES, SLOTS, BUFFERS, BYTES, TIMERS, GROUPS, TIMER_CMDS>
where
    H: TickHook<Self>,
{
    /// How many bytes a message buffer spends on each message's length.
    pub const MESSAGE_LENGTH_BYTES: usize = C::MESSAGE_LENGTH_BYTES;

    /// The key a blocking `xStreamBufferSend` files its `TimeOut_t` under
    /// in the task's wait frame, which is keyed by queue. Index `u32::MAX`
    /// is past `Handle::MAX_INDEX`, so no queue is ever this one, and a task
    /// is inside one blocking call at a time.
    const STREAM_SEND_WAIT: QueueHandle = QueueHandle::from_parts(u32::MAX, 1);

    /// `xStreamBufferCreate`.
    ///
    /// A trigger level of zero becomes one, as the C does: a reader that
    /// woke on nothing would spin.
    ///
    /// # Errors
    /// [`Error::Full`] when the buffer arena or the byte arena is full;
    /// [`Error::InvalidArgument`] for a zero-length buffer.
    pub fn stream_buffer_create(
        &mut self,
        size: usize,
        trigger: usize,
    ) -> Result<StreamBufferHandle> {
        self.new_stream_buffer(size, trigger, false)
    }

    /// `xMessageBufferCreate`.
    ///
    /// # Errors
    /// As [`Kernel::stream_buffer_create`]; a message buffer must also be
    /// longer than one length prefix.
    pub fn message_buffer_create(&mut self, size: usize) -> Result<StreamBufferHandle> {
        if size <= Self::MESSAGE_LENGTH_BYTES {
            return Err(Error::InvalidArgument);
        }
        self.new_stream_buffer(size, 1, true)
    }

    fn new_stream_buffer(
        &mut self,
        size: usize,
        trigger: usize,
        is_message: bool,
    ) -> Result<StreamBufferHandle> {
        let caller = self.cur();
        if size == 0 || trigger > size {
            return Err(Error::InvalidArgument);
        }
        let trigger = if trigger == 0 { 1 } else { trigger };
        // `xBufferSizeBytes++` before the allocation: the spare byte.
        let length = size.saturating_add(1);
        let base = self.take_bytes(length).ok_or(Error::Full)?;
        let handle = self
            .buffers
            .try_insert(StreamBuffer {
                base,
                length,
                trigger,
                head: 0,
                tail: 0,
                waiting_to_receive: TaskHandle::NULL,
                waiting_to_send: TaskHandle::NULL,
                is_message,
                notify_index: 0,
            })
            .map_err(|_| Error::Full)?;
        self.account_for_allocation();
        // The malloc's `xTaskResumeAll` can release a tick and switch us
        // away, and the C's thread then stops there with
        // `traceSTREAM_BUFFER_CREATE` still ahead of it -- so the line
        // belongs to this task's NEXT run, not to whoever took the CPU.
        self.trace_failure_or_owe(
            caller,
            OwedTrace::StreamBufferCreate {
                buffer: handle,
                is_message_buffer: is_message,
            },
        );
        Ok(handle)
    }

    /// Take `length` bytes of the arena: first fit from what deleted
    /// buffers gave back, and only then from the untouched end.
    ///
    /// First fit rather than best fit because the pattern that needs an
    /// allocator at all is create-then-delete of the *same size*, which
    /// first fit serves exactly and without fragmenting.
    fn take_bytes(&mut self, length: usize) -> Option<usize> {
        for i in 0..self.free_count {
            let (base, free) = *self.free_blocks.get(i)?;
            if free < length {
                continue;
            }
            if free == length {
                self.drop_free_block(i);
            } else if let Some(slot) = self.free_blocks.get_mut(i) {
                *slot = (base.saturating_add(length), free.wrapping_sub(length));
            }
            return Some(base);
        }
        let end = self.bytes_used.checked_add(length)?;
        if end > BYTES {
            return None;
        }
        let base = self.bytes_used;
        self.bytes_used = end;
        Some(base)
    }

    /// Give a block back, coalescing with whichever neighbours touch it so
    /// the list cannot grow past one entry per hole.
    fn give_bytes(&mut self, base: usize, length: usize) {
        let mut base = base;
        let mut length = length;
        let mut i = 0;
        while i < self.free_count {
            let Some(&(other_base, other_len)) = self.free_blocks.get(i) else {
                break;
            };
            if other_base.saturating_add(other_len) == base {
                base = other_base;
                length = length.saturating_add(other_len);
                self.drop_free_block(i);
                continue;
            }
            if base.saturating_add(length) == other_base {
                length = length.saturating_add(other_len);
                self.drop_free_block(i);
                continue;
            }
            i = i.wrapping_add(1);
        }
        // A block at the very end goes back to the bump pointer instead of
        // the list, which is what keeps a create/delete loop free.
        if base.saturating_add(length) == self.bytes_used {
            self.bytes_used = base;
            return;
        }
        if let Some(slot) = self.free_blocks.get_mut(self.free_count) {
            *slot = (base, length);
            self.free_count = self.free_count.wrapping_add(1);
        }
    }

    fn drop_free_block(&mut self, index: usize) {
        let last = self.free_count.saturating_sub(1);
        if index < last {
            if let Some(&moved) = self.free_blocks.get(last) {
                if let Some(slot) = self.free_blocks.get_mut(index) {
                    *slot = moved;
                }
            }
        }
        self.free_count = last;
    }

    /// `vStreamBufferDelete`: the handle goes stale and the ring's bytes go
    /// back to the arena.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_delete(&mut self, buffer: StreamBufferHandle) -> Result<()> {
        let b = *self.buffers.resolve(buffer)?;
        let _ = self.buffers.discard(buffer);
        self.give_bytes(b.base, b.length);
        // `vStreamBufferDelete` ends in `vPortFree` (stream_buffer.c:582),
        // and `heap_4` brackets that with `vTaskSuspendAll` /
        // `xTaskResumeAll` -- which takes a critical section, and on the sim
        // a critical-section exit is the clock. The create paid for its
        // `pvPortMalloc` and the delete did not, so every deleted buffer
        // lost one exit. `StreamBufferDemo`'s trigger-level test deletes one
        // per pass and drifted by exactly 1 at its second `vTaskDelay`.
        //
        // The timer service's `Delete` arm had the identical defect.
        self.account_for_allocation();
        Ok(())
    }

    /// `xStreamBufferBytesAvailable`. No critical section, as in the C.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_bytes_available(&self, buffer: StreamBufferHandle) -> Result<usize> {
        Ok(self.buffers.resolve(buffer)?.bytes_in_buffer())
    }

    /// `xStreamBufferSpacesAvailable`. No critical section, as in the C.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_spaces_available(&self, buffer: StreamBufferHandle) -> Result<usize> {
        Ok(self.buffers.resolve(buffer)?.spaces_available())
    }

    /// `xStreamBufferIsEmpty`: head and tail in the same place.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_is_empty(&self, buffer: StreamBufferHandle) -> Result<bool> {
        let b = self.buffers.resolve(buffer)?;
        Ok(b.head == b.tail)
    }

    /// `xStreamBufferIsFull`: no room for another message.
    ///
    /// For a message buffer "another message" means at least one more byte
    /// than its length prefix, so a buffer with exactly a prefix's worth of
    /// room left is already full. A stream buffer compares against zero.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_is_full(&self, buffer: StreamBufferHandle) -> Result<bool> {
        let b = self.buffers.resolve(buffer)?;
        let prefix = if b.is_message {
            Self::MESSAGE_LENGTH_BYTES
        } else {
            0
        };
        Ok(b.spaces_available() <= prefix)
    }

    /// `xStreamBufferSetTriggerLevel`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_set_trigger_level(
        &mut self,
        buffer: StreamBufferHandle,
        trigger: usize,
    ) -> Result<bool> {
        let length = self.buffers.resolve(buffer)?.length;
        // Coerce first, then bounds-check, which is the order the C uses:
        //
        //     if( xTriggerLevel == ( size_t ) 0 ) { xTriggerLevel = 1; }
        //     if( xTriggerLevel < pxStreamBuffer->xLength ) { ...pdPASS }
        //     else { xReturn = pdFALSE; }
        //
        // The two orders disagree on exactly one input: a zero trigger
        // against a length of ONE. Coercing first makes it 1, finds `1 < 1`
        // false, and refuses without storing; bounds-checking first lets 0
        // through and stores the coerced 1.
        //
        // THAT INPUT IS UNREACHABLE HERE, and the ordering is matched anyway
        // rather than left to depend on it. `length` is `size + 1` -- the
        // spare byte a ring buffer needs to tell full from empty, the C's
        // own `xBufferSizeBytes++` -- and `size == 0` is refused at
        // creation, so no buffer has a length of 1 and the orders agree on
        // every input a caller can construct.
        //
        // Written down because the first version of this comment claimed a
        // live divergence. It is not one; it is an inconsistency with the C
        // that one arithmetic detail in a different function is keeping
        // harmless.
        let trigger = if trigger == 0 { 1 } else { trigger };
        if trigger >= length {
            return Ok(false);
        }
        self.buffers.resolve_mut(buffer)?.trigger = trigger;
        Ok(true)
    }

    /// `xStreamBufferReset`: empty it, unless a task is waiting on it.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_reset(&mut self, buffer: StreamBufferHandle) -> Result<bool> {
        self.enter_critical();
        let done = match self.buffers.resolve_mut(buffer) {
            Ok(b) if b.waiting_to_receive.is_null() && b.waiting_to_send.is_null() => {
                b.head = 0;
                b.tail = 0;
                true
            }
            _ => false,
        };
        self.exit_critical();
        Ok(done)
    }

    /// `xStreamBufferNextMessageLengthBytes`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_next_message_length(&self, buffer: StreamBufferHandle) -> Result<usize> {
        let b = *self.buffers.resolve(buffer)?;
        if !b.is_message {
            return Ok(0);
        }
        let available = b.bytes_in_buffer();
        if available <= Self::MESSAGE_LENGTH_BYTES {
            return Ok(0);
        }
        Ok(Self::read_length_prefix_from(&self.bytes, b.base, b.length, b.tail).0)
    }

    // ---------------------------------------------------------- sending --

    /// `xStreamBufferSend`.
    ///
    /// Returns how many bytes went in. `Blocked` means the caller must
    /// make the same call again: the C loops here waiting for space, and a
    /// kernel with no stacks returns between passes.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    /// Sim contract v2: what a kernel call costs when it took no critical
    /// section.
    ///
    /// v1 had two tick sources, the idle hook and every 16th outermost
    /// critical-section exit. Both are kernel-visible points, and between
    /// them they left one hole: a task that is always ready and whose
    /// no-progress path takes no critical section takes NO TIME, never
    /// yields, and stops the clock for every other task.
    ///
    /// `xStreamBufferSend` and `xStreamBufferReceive` are the only calls in
    /// this corpus that can return that way -- the zero-wait path reads the
    /// ring and leaves. Every other object closes the hole by accident:
    /// `uxQueueMessagesWaiting`, `eTaskGetState` and `uxTaskPriorityGet`
    /// all take a section. `StreamBufferDemo`'s `prvNonBlockingReceiverTask`
    /// is the task that found it, and under v1 the whole run froze at
    /// `ticks=1 exits=16` (`docs/HOLES.md`, H9).
    ///
    /// So a blind call costs exactly one empty critical section. Not a new
    /// clock -- the SAME one, so the count, the every-16th rule, the
    /// running-task test and the switch all reuse paths v1 already proved,
    /// and `exits` keeps its meaning: the number of kernel-visible points.
    /// The C port does the identical thing from
    /// `traceRETURN_xStreamBufferSend` / `..._Receive`; see
    /// `vPortKairosApiReturn`.
    ///
    /// A call that BLOCKED is not blind: the C's thread stops inside it,
    /// other tasks run, and the counter has moved by the time the return
    /// hook is reached. Comparing the count is what says so on both sides.
    ///
    /// Gated on `T::EMITS`, for the same reason [`Kernel::note_exits`] is, and
    /// to MATCH the C rather than diverge from it. In the C this bracket is
    /// `traceRETURN_xStreamBufferSend`, which `FreeRTOS.h` defines as **empty**
    /// unless a config wires it. Only `oracle/harness/FreeRTOSConfig.h` does --
    /// the sim. `bench/kernel-ram/c/FreeRTOSConfig.h`, which the rv32 flash arm
    /// compiles against, defines no `traceRETURN_*` at all, so **C pays zero
    /// bytes for it in the arm we measure** while we were paying for it
    /// unconditionally. Same defect class as relaxation being off on our arm
    /// only (scorecard 14).
    ///
    /// Nothing observes the bracket under a silent sink: the exits counter it
    /// samples is read by no one, and the critical section it takes is a closed
    /// window that restores the mask it found. Under an emitting sink -- which
    /// is every `conform` scenario -- the gate is true and the behaviour is
    /// exactly as before, which is why 26/26 is unchanged.
    fn blind_call(&mut self, exits_at_entry: u64) {
        if !T::EMITS {
            return;
        }
        if self.port.exits() != exits_at_entry {
            return;
        }
        self.enter_critical();
        self.exit_critical();
    }

    /// `xStreamBufferSend`, with the contract-v2 bracket around it.
    ///
    /// # Errors
    /// As [`Kernel::stream_buffer_send_inner`].
    pub fn stream_buffer_send(
        &mut self,
        buffer: StreamBufferHandle,
        data: &[u8],
        ticks: u64,
    ) -> Result<Wait<usize>> {
        let entry = if T::EMITS { self.port.exits() } else { 0 };
        let out = self.stream_buffer_send_inner(buffer, data, ticks);
        self.blind_call(entry);
        out
    }

    fn stream_buffer_send_inner(
        &mut self,
        buffer: StreamBufferHandle,
        data: &[u8],
        ticks: u64,
    ) -> Result<Wait<usize>> {
        let caller = self.cur();
        // ONLY the fields this body reads. It was `*self.buffers.resolve(..)`
        // -- the whole nine-field descriptor, about thirty-two bytes on rv32 --
        // to reach three of them. `notify_index` is read at four
        // points spread across the body, each behind an `&mut self` call, so
        // the descriptor had to stay LIVE across all of them and was spilled
        // for it. Two scalars live in registers. Same finding as `queue_take`:
        // the cost was the live range, not the copy.
        let (is_message, length, notify_index) = {
            let b = self.buffers.resolve(buffer)?;
            (b.is_message, b.length, b.notify_index)
        };
        let max_reported = length.saturating_sub(1);
        let mut required = data.len();
        let mut ticks = ticks;
        if is_message {
            required = required.saturating_add(Self::MESSAGE_LENGTH_BYTES);
            if required > max_reported {
                // It will never fit, so do not wait for it to.
                ticks = 0;
            }
        } else if required > max_reported {
            required = max_reported;
        }

        // The do-while's condition, `xTaskCheckForTimeOut(..) == pdFALSE`:
        // `Some(left)` goes round again with what is left of the block time,
        // `None` leaves the loop. Set by whichever arm finished a wait.
        let mut again: Option<u64> = None;
        let mut sampled = self.take_stream_resume(caller);
        if let Some(space) = sampled {
            // Resuming after the exit that sampled the space. The same
            // point as in `stream_buffer_receive`: the C tests `xSpace <
            // xRequiredSpace` below the exit, against the sample taken
            // inside it, so a task preempted there still blocks.
            if space < required {
                // What is left of the block time: the frame is this call's,
                // so `begin_wait` answers it without touching it.
                let left = self
                    .begin_wait(caller, Self::STREAM_SEND_WAIT, ticks)
                    .unwrap_or(0);
                match self.notify_wait(notify_index, 0, 0, left)? {
                    Blocked => {
                        self.park_stream_sample(caller, space);
                        return Ok(Blocked);
                    }
                    Ready(_) => {
                        self.buffers.resolve_mut(buffer)?.waiting_to_send = TaskHandle::NULL;
                        again = self.check_for_timeout(caller).map(NonZeroU64::get);
                    }
                }
            }
        } else if self.notify_wait_pending(caller) {
            match self.notify_wait(notify_index, 0, 0, ticks)? {
                Blocked => return Ok(Blocked),
                Ready(_) => {}
            }
            // Spelled out at both resume arms and in the loop rather than
            // factored into a helper the way the receive side's
            // `after_stream_wait` is. Factoring it measured **+28 B** on
            // 2026-09-25, identical with and without `#[inline(never)]` -- so
            // LLVM outlines it either way, and one shared body plus three
            // calls costs more than three copies that each fold against their
            // own arm.
            self.buffers.resolve_mut(buffer)?.waiting_to_send = TaskHandle::NULL;
            sampled = Some(self.stream_sample(caller));
            again = self.check_for_timeout(caller).map(NonZeroU64::get);
        } else if ticks > 0 {
            // `vTaskSetTimeOutState( &xTimeOut )` (stream_buffer.c:878),
            // which the C runs before it looks at the buffer at all.
            //
            // It is the PUBLIC entry point, so it takes a critical section
            // of its own -- unlike the queue calls, which reach the same
            // state through `vTaskInternalSetTimeOutState` from inside one.
            // One exit per blocking send, and `StreamBufferDemo` makes the
            // first of them 350 ticks in.
            //
            // ... and only once per call: a task preempted at this very
            // exit resumes BELOW it, so re-entry must not buy it again.
            if !self.take_stream_timed(caller) {
                self.enter_critical();
                // A fresh `TimeOut_t` every call, as the C's is a local:
                // a frame an erroring call left behind must not be reused.
                self.end_wait(caller);
                let _ = self.begin_wait(caller, Self::STREAM_SEND_WAIT, ticks);
                self.exit_critical();
                if self.cur() != caller {
                    self.set_stream_timed(caller);
                    return Ok(Blocked);
                }
            }
            again = Some(ticks);
        }

        // `do { ... } while( xTaskCheckForTimeOut( &xTimeOut, &xTicksToWait )
        // == pdFALSE )`. A wake is not a verdict: the receiver notifies on
        // every read, and if that freed too little the C samples again and
        // blocks again for what is left. One pass and a partial write was
        // what the API differential caught (one core, step 9106).
        while let Some(left) = again {
            self.enter_critical();
            let space = self.buffers.resolve(buffer)?.spaces_available();
            let must_block = space < required;
            if must_block {
                let _ = self.notify_state_clear(None, notify_index);
                self.buffers.resolve_mut(buffer)?.waiting_to_send = caller;
            }
            self.exit_critical();
            sampled = Some(space);
            // That exit can release a tick, and a tick can switch this task
            // away. The C's thread stops inside the exit and everything
            // below it runs when the task is resumed — with the sample it
            // already took, and without paying for the section twice.
            if self.cur() != caller {
                self.set_stream_resume(caller, space);
                return Ok(Blocked);
            }
            if !must_block {
                break;
            }
            // `traceBLOCKING_ON_STREAM_BUFFER_SEND` is not one of the
            // harness's hooks, so blocking says nothing on either side.
            match self.notify_wait(notify_index, 0, 0, left)? {
                Blocked => {
                    self.park_stream_sample(caller, space);
                    return Ok(Blocked);
                }
                Ready(_) => {
                    self.buffers.resolve_mut(buffer)?.waiting_to_send = TaskHandle::NULL;
                    again = self.check_for_timeout(caller).map(NonZeroU64::get);
                }
            }
        }
        self.end_wait(caller);

        // `if( xSpace == 0 ) xSpace = xStreamBufferSpacesAvailable(..)`: the
        // last sample stands, stale or not, unless it was zero -- which a
        // send that never sampled (no block time) always is.
        let space = match sampled.take() {
            Some(space) if space != 0 => space,
            _ => self.buffers.resolve(buffer)?.spaces_available(),
        };
        let written = self.write_message(buffer, data, space, required)?;
        if written > 0 {
            let tick = self.tick;
            self.note_exits();
            self.trace.event(
                tick,
                Event::StreamBufferSend {
                    buffer,
                    bytes: written,
                },
            );
            // One resolve, not two: nothing moves between the byte count and
            // the trigger test, and both read the same buffer. The block ends
            // the borrow before the completion call needs `self` mutably.
            let trigger = {
                let b = self.buffers.resolve(buffer)?;
                b.meets_trigger(b.bytes_in_buffer())
            };
            if trigger {
                self.send_completed(buffer)?;
            }
        }
        Ok(Ready(written))
    }

    /// `xStreamBufferSendFromISR`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_send_from_isr(
        &mut self,
        buffer: StreamBufferHandle,
        data: &[u8],
    ) -> Result<(usize, Woken)> {
        let snapshot = *self.buffers.resolve(buffer)?;
        let max_reported = snapshot.length.saturating_sub(1);
        let mut required = data.len();
        if snapshot.is_message {
            required = required.saturating_add(Self::MESSAGE_LENGTH_BYTES);
        } else if required > max_reported {
            required = max_reported;
        }
        let space = snapshot.spaces_available();
        let written = self.write_message(buffer, data, space, required)?;
        let mut woken = Woken::NO;
        if written > 0 {
            // `traceSTREAM_BUFFER_SEND_FROM_ISR` is a different macro from
            // `traceSTREAM_BUFFER_SEND`, and the harness hooks only the
            // latter — so an interrupt's send says nothing on either side.
            // One resolve, not two: nothing moves between the byte count and
            // the trigger test, and both read the same buffer. The block ends
            // the borrow before the completion call needs `self` mutably.
            let trigger = {
                let b = self.buffers.resolve(buffer)?;
                b.meets_trigger(b.bytes_in_buffer())
            };
            if trigger {
                woken = self.send_completed_from_isr(buffer)?;
            }
        }
        Ok((written, woken))
    }

    // -------------------------------------------------------- receiving --

    /// What `xStreamBufferReceive` runs once its notify wait returns.
    ///
    /// `xTaskNotifyWaitIndexed`'s own trailing critical section can release
    /// a tick and switch the caller away, and the C's thread then stops
    /// THERE -- with `prvReadMessageFromBuffer` and
    /// `traceSTREAM_BUFFER_RECEIVE` still ahead of it. So the bytes move,
    /// and the line appears, on this task's next run and not inside
    /// whoever took the CPU. `StreamBufferDemo`'s echo clients showed it
    /// four times in a 2,000-tick run, each a receive traced six exits
    /// early.
    ///
    /// `None` means "stop here"; the caller returns `Blocked` and the body
    /// makes the same call again, which then takes the resume path.
    // In line at all THREE callers, which are all inside
    // `stream_buffer_receive_inner`.
    //
    // This said "A3: ONE caller in the linked kernel, so this pays a prologue
    // and epilogue for a single call" until 2026-09-25. The caller count was
    // simply wrong -- there are three -- so the A3 argument did not apply, and
    // with three callers `#[inline]` DUPLICATES the body rather than moving it,
    // which is the opposite of what the comment claimed.
    //
    // The decision survives the correction, measured: `#[inline(never)]` costs
    // **+100 B** (receive 732 -> 686, but a 146 B symbol appears). Each inlined
    // copy folds to roughly fifteen bytes against the surrounding control flow,
    // while a standalone body pays a frame and two real handle resolves. Right
    // answer, wrong reason, now with a number.
    #[inline(always)]
    fn after_stream_wait(
        &mut self,
        caller: TaskHandle,
        buffer: StreamBufferHandle,
    ) -> Result<Option<usize>> {
        self.buffers.resolve_mut(buffer)?.waiting_to_receive = TaskHandle::NULL;
        let available = self.buffers.resolve(buffer)?.bytes_in_buffer();
        if self.cur() != caller {
            // Merging these two into one accessor that resolves the TCB once
            // measured **0 B** on 2026-09-25: `&mut self` is `noalias`, so LLVM
            // had already shared the resolve across both setters. `list.rs`'s
            // fourth pass says exactly this, and it applies to the TCB flag
            // accessors too. Merging adjacent accessors only pays when an
            // `&mut self` call stands BETWEEN them.
            self.set_stream_waited(caller);
            self.set_stream_resume(caller, available);
            return Ok(None);
        }
        Ok(Some(available))
    }

    /// `xStreamBufferReceive`: bytes into `out`, and how many arrived.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    /// `xStreamBufferReceive`, with the contract-v2 bracket around it.
    /// See [`Kernel::blind_call`].
    ///
    /// # Errors
    /// As [`Kernel::stream_buffer_receive_inner`].
    pub fn stream_buffer_receive(
        &mut self,
        buffer: StreamBufferHandle,
        out: &mut [u8],
        ticks: u64,
    ) -> Result<Wait<usize>> {
        let entry = if T::EMITS { self.port.exits() } else { 0 };
        let received = self.stream_buffer_receive_inner(buffer, out, ticks);
        self.blind_call(entry);
        received
    }

    fn stream_buffer_receive_inner(
        &mut self,
        buffer: StreamBufferHandle,
        out: &mut [u8],
        ticks: u64,
    ) -> Result<Wait<usize>> {
        let caller = self.cur();
        // ONLY the fields this body reads. It was `*self.buffers.resolve(..)`
        // -- the whole nine-field descriptor, about thirty-two bytes on rv32 --
        // to reach two of them. `notify_index` is read at four
        // points spread across the body, each behind an `&mut self` call, so
        // the descriptor had to stay LIVE across all of them and was spilled
        // for it. Two scalars live in registers. Same finding as `queue_take`:
        // the cost was the live range, not the copy.
        let (is_message, notify_index) = {
            let b = self.buffers.resolve(buffer)?;
            (b.is_message, b.notify_index)
        };
        let prefix = if is_message {
            Self::MESSAGE_LENGTH_BYTES
        } else {
            0
        };
        let mut available;
        if let Some(local) = self.take_stream_resume(caller) {
            // Resuming after the exit that sampled the buffer.
            //
            // The C's thread stops inside `taskEXIT_CRITICAL`, and when it
            // runs again it still evaluates the `if( xBytesAvailable <=
            // xBytesToStoreMessageLength )` BELOW that exit -- against the
            // sample it took inside the section. So a task preempted there
            // goes on to block exactly like one that was not.
            //
            // Falling straight through to the read instead returns 0 from a
            // call that was told to wait 350 ticks, which is what
            // `StreamBufferDemo`'s lower-priority echo server caught: the C
            // blocked at tick 1 and the sim created its client instead.
            available = local;
            if !self.take_stream_waited(caller) && available <= prefix {
                match self.notify_wait(notify_index, 0, 0, ticks)? {
                    Blocked => return Ok(Blocked),
                    Ready(_) => match self.after_stream_wait(caller, buffer)? {
                        Some(now) => available = now,
                        None => return Ok(Blocked),
                    },
                }
            }
        } else if self.notify_wait_pending(caller) {
            // Resuming: the C's wait returns here, so its second half runs
            // whatever the buffer now holds.
            match self.notify_wait(notify_index, 0, 0, ticks)? {
                Blocked => return Ok(Blocked),
                Ready(_) => {}
            }
            match self.after_stream_wait(caller, buffer)? {
                Some(now) => available = now,
                None => return Ok(Blocked),
            }
        } else if ticks > 0 {
            self.enter_critical();
            available = self.buffers.resolve(buffer)?.bytes_in_buffer();
            let must_block = available <= prefix;
            if must_block {
                let _ = self.notify_state_clear(None, notify_index);
                self.buffers.resolve_mut(buffer)?.waiting_to_receive = caller;
            }
            self.exit_critical();
            // That exit can release a tick, and a tick can switch this task
            // away. The C's thread stops inside the exit and everything
            // below it runs when the task is resumed — with the sample it
            // already took, and without paying for the section twice.
            if self.cur() != caller {
                self.set_stream_resume(caller, available);
                return Ok(Blocked);
            }
            if must_block {
                match self.notify_wait(notify_index, 0, 0, ticks)? {
                    Blocked => return Ok(Blocked),
                    Ready(_) => match self.after_stream_wait(caller, buffer)? {
                        Some(now) => available = now,
                        None => return Ok(Blocked),
                    },
                }
            }
        } else {
            available = self.buffers.resolve(buffer)?.bytes_in_buffer();
        }

        let mut received = 0;
        if available > prefix {
            received = self.read_message(buffer, out, available)?;
            if received != 0 {
                let tick = self.tick;
                self.note_exits();
                self.trace.event(
                    tick,
                    Event::StreamBufferReceive {
                        buffer,
                        bytes: received,
                    },
                );
                self.receive_completed(buffer)?;
            }
        }
        Ok(Ready(received))
    }

    /// `xStreamBufferReceiveFromISR`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_receive_from_isr(
        &mut self,
        buffer: StreamBufferHandle,
        out: &mut [u8],
    ) -> Result<(usize, Woken)> {
        let snapshot = *self.buffers.resolve(buffer)?;
        let prefix = if snapshot.is_message {
            Self::MESSAGE_LENGTH_BYTES
        } else {
            0
        };
        let available = snapshot.bytes_in_buffer();
        let mut received = 0;
        let mut woken = Woken::NO;
        if available > prefix {
            received = self.read_message(buffer, out, available)?;
            if received != 0 {
                // `traceSTREAM_BUFFER_RECEIVE_FROM_ISR`, likewise unhooked.
                woken = self.receive_completed_from_isr(buffer)?;
            }
        }
        Ok((received, woken))
    }

    // --------------------------------------------------- the completions --

    /// `sbSEND_COMPLETED`: tell a waiting reader, with the scheduler
    /// suspended so the notify cannot switch away mid-update.
    ///
    /// The application may replace it — that is what the macro is for, and
    /// `MessageBufferAMP` is the demo that does — so the hook gets first
    /// refusal and `true` means it handled the whole thing.
    // No `inline`: one was tried and cost 323,801 instructions on
    // `StreamBufferDemo`. The body is small but it is reached from inside
    // `stream_buffer_send`, which is already large.
    // A3: ONE caller in the linked kernel, so this pays a prologue and epilogue
    // for a single call. Inlining moves the body rather than duplicating it.
    #[inline(always)]
    fn send_completed(&mut self, buffer: StreamBufferHandle) -> Result<()> {
        if H::send_completed(self, buffer) {
            return Ok(());
        }
        self.suspend_all();
        let waiting = self.buffers.resolve(buffer)?.waiting_to_receive;
        if !waiting.is_null() {
            let index = self.buffers.resolve(buffer)?.notify_index;
            let _ = self.notify(waiting, index, 0, NotifyAction::None)?;
            self.buffers.resolve_mut(buffer)?.waiting_to_receive = TaskHandle::NULL;
        }
        let _ = self.resume_all();
        Ok(())
    }

    /// `xStreamBufferSendCompletedFromISR` / `sbSEND_COMPLETED_FROM_ISR`.
    ///
    /// Public because an application that replaced [`TickHook::send_completed`]
    /// has to be able to do the notify itself, later and from interrupt
    /// context — which is exactly what `MessageBufferAMP` does once its
    /// stand-in interrupt has read the handle back off the control buffer.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn send_completed_from_isr(&mut self, buffer: StreamBufferHandle) -> Result<Woken> {
        let waiting = self.buffers.resolve(buffer)?.waiting_to_receive;
        if waiting.is_null() {
            return Ok(Woken::NO);
        }
        let index = self.buffers.resolve(buffer)?.notify_index;
        let (_, woken) = self.notify_from_isr(waiting, index, 0, NotifyAction::None)?;
        self.buffers.resolve_mut(buffer)?.waiting_to_receive = TaskHandle::NULL;
        Ok(woken)
    }

    /// `xStreamBufferSendCompletedFromISR` with the C's ANSWER: whether a task
    /// was waiting to receive (and so was notified), and whether waking it
    /// wants a switch.
    ///
    /// [`Kernel::send_completed_from_isr`] does the same work but answers
    /// only the second, which is all a `sbSEND_COMPLETED` replacement needs;
    /// the C function's own return value had no Rust spelling until the API
    /// differential (P1.6) asked for it.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_send_completed_from_isr(
        &mut self,
        buffer: StreamBufferHandle,
    ) -> Result<(bool, Woken)> {
        let waiting = !self.buffers.resolve(buffer)?.waiting_to_receive.is_null();
        let woken = self.send_completed_from_isr(buffer)?;
        Ok((waiting, woken))
    }

    /// `sbRECEIVE_COMPLETED`.
    fn receive_completed(&mut self, buffer: StreamBufferHandle) -> Result<()> {
        self.suspend_all();
        let waiting = self.buffers.resolve(buffer)?.waiting_to_send;
        if !waiting.is_null() {
            let index = self.buffers.resolve(buffer)?.notify_index;
            let _ = self.notify(waiting, index, 0, NotifyAction::None)?;
            self.buffers.resolve_mut(buffer)?.waiting_to_send = TaskHandle::NULL;
        }
        let _ = self.resume_all();
        Ok(())
    }

    /// `sbRECEIVE_COMPLETED_FROM_ISR`.
    fn receive_completed_from_isr(&mut self, buffer: StreamBufferHandle) -> Result<Woken> {
        let waiting = self.buffers.resolve(buffer)?.waiting_to_send;
        if waiting.is_null() {
            return Ok(Woken::NO);
        }
        let index = self.buffers.resolve(buffer)?.notify_index;
        let (_, woken) = self.notify_from_isr(waiting, index, 0, NotifyAction::None)?;
        self.buffers.resolve_mut(buffer)?.waiting_to_send = TaskHandle::NULL;
        Ok(woken)
    }

    // ------------------------------------------------ the ring itself --

    /// `prvWriteMessageToBuffer`.
    fn write_message(
        &mut self,
        buffer: StreamBufferHandle,
        data: &[u8],
        space: usize,
        required: usize,
    ) -> Result<usize> {
        // ONE resolve, and the descriptor is written through in place.
        //
        // This used to copy the whole nine-field descriptor out
        // (`*self.buffers.resolve(..)`) and then resolve a SECOND time to store
        // the new head, because `write_bytes` wanted `&mut self` and so nothing
        // borrowed from `self` could be alive across it. Destructuring `self`
        // into disjoint field borrows removes both the copy and the second
        // resolve: `buffers` and `bytes` are different fields, so the borrow
        // checker allows what C does with one pointer.
        let Self { buffers, bytes, .. } = self;
        let b = buffers.resolve_mut(buffer)?;
        let (base, ring, is_message) = (b.base, b.length, b.is_message);
        let mut next_head = b.head;
        let mut length = data.len();
        let mut space = space;
        if is_message {
            if space >= required {
                // `write_length_prefix`, folded in: it was a one-caller wrapper
                // whose whole body was this.
                // `length.to_le_bytes()`, not `(length as u64).to_le_bytes()`:
                // the widening wrote eight bytes on a target whose `size_t` is
                // four, and the top half was never sent.
                next_head = if Self::MESSAGE_LENGTH_BYTES <= core::mem::size_of::<usize>() {
                    let raw = length.to_le_bytes();
                    let prefix = raw.get(..Self::MESSAGE_LENGTH_BYTES).unwrap_or(&raw);
                    Self::write_bytes_into(bytes, base, ring, prefix, next_head)
                } else {
                    // A length type wider than `size_t` -- a configuration
                    // the C allows -- is written zero-extended, every byte
                    // of it. Writing only `usize`'s bytes here, while the
                    // space arithmetic counted the whole prefix, left the
                    // next message misread on a 32-bit target (plan P5, the
                    // API differential's `-m32` twin). The branch is on
                    // constants, so a prefix that fits `usize` pays nothing.
                    let raw = (length as u64).to_le_bytes();
                    let prefix = raw.get(..Self::MESSAGE_LENGTH_BYTES).unwrap_or(&raw);
                    Self::write_bytes_into(bytes, base, ring, prefix, next_head)
                };
            } else {
                length = 0;
            }
            space = space.saturating_sub(Self::MESSAGE_LENGTH_BYTES);
        }
        let length = length.min(space);
        if length != 0 {
            let chunk = data.get(..length).unwrap_or(data);
            b.head = Self::write_bytes_into(bytes, base, ring, chunk, next_head);
        }
        Ok(length)
    }

    /// `prvReadMessageFromBuffer`.
    fn read_message(
        &mut self,
        buffer: StreamBufferHandle,
        out: &mut [u8],
        available: usize,
    ) -> Result<usize> {
        // ONE resolve, and the descriptor is written through in place -- the
        // mirror of `write_message`. Two resolves and a nine-field copy were
        // needed only while the helpers took `&self`/`&mut self`; giving them
        // the arena as a SLICE lets `buffers` and `bytes` be borrowed disjointly.
        let Self { buffers, bytes, .. } = self;
        let b = buffers.resolve_mut(buffer)?;
        let (base, ring, is_message) = (b.base, b.length, b.is_message);
        let mut next_tail = b.tail;
        let mut available = available;
        let next_length;
        if is_message {
            let (message_length, tail) =
                Self::read_length_prefix_from(bytes, base, ring, next_tail);
            next_tail = tail;
            available = available.saturating_sub(Self::MESSAGE_LENGTH_BYTES);
            next_length = if message_length > out.len() {
                // The C returns nothing rather than half a message.
                0
            } else {
                message_length
            };
        } else {
            next_length = out.len();
        }
        let count = next_length.min(available);
        if count != 0 {
            b.tail = match out.get_mut(..count) {
                Some(slot) => Self::read_bytes_from(bytes, base, ring, slot, next_tail),
                None => next_tail,
            };
        }
        Ok(count)
    }

    /// `prvWriteBytesToBuffer`: the ring wrap, in two `memcpy`s.
    ///
    /// The index arithmetic here is `wrapping_*`, because the geometry has
    /// already bounded every term: `head` is below `length`, `base + length`
    /// is at most `BYTES`, `first` is `min(len, upto)` so `len - first`
    /// cannot go below zero, and `head + first` is at most `length` -- which
    /// the fold on the line after turns back into 0, exactly as `wrap_next`
    /// does. Each index then passes through a `get_mut` that refuses
    /// anything the ring does not own.
    /// Takes the byte arena as a SLICE rather than `&mut self`, so a caller can
    /// hold `&mut` to a descriptor in `self.buffers` at the same time. It asked
    /// for `&mut self` until 2026-09-25, and that is why `write_message` had to
    /// copy the whole descriptor out before calling it: under `forbid(unsafe)`
    /// a `&StreamBuffer` borrowed from `self` cannot coexist with `&mut self`.
    /// Destructuring `self` into disjoint field borrows is the safe form of what
    /// C gets for free by holding one pointer.
    fn write_bytes_into(
        bytes: &mut [u8],
        base: usize,
        ring: usize,
        data: &[u8],
        head: usize,
    ) -> usize {
        // The two copies, and the two the C makes. A payload longer than the
        // ring is refused before this, so that case cannot arrive.
        if data.len() <= ring {
            let upto = ring.wrapping_sub(head);
            let first = data.len().min(upto);
            let from = base.wrapping_add(head);
            if let (Some(dst), Some(src)) = (
                bytes.get_mut(from..from.wrapping_add(first)),
                data.get(..first),
            ) {
                dst.copy_from_slice(src);
            }
            let rest = data.len().wrapping_sub(first);
            if rest > 0 {
                if let (Some(dst), Some(src)) = (
                    bytes.get_mut(base..base.wrapping_add(rest)),
                    data.get(first..),
                ) {
                    dst.copy_from_slice(src);
                }
                return rest;
            }
            let next = head.wrapping_add(first);
            return if next >= ring { 0 } else { next };
        }
        head
    }

    /// `prvReadBytesFromBuffer`.
    ///
    /// Bounded as [`Kernel::write_bytes`] is, and for the same reasons.
    /// The mirror of [`Self::write_bytes_into`], and takes the arena the same
    /// way -- as a slice, so a caller can hold `&mut` to a descriptor in
    /// `self.buffers` across it. A request longer than the ring cannot arrive:
    /// `read_message` clamps `count` to `available`, which the ring's spare byte
    /// holds below `ring`.
    ///
    /// It asked for `&mut self` until 2026-09-25 while only ever READING the
    /// arena, and that single word is why `read_length_prefix` carried its own
    /// copy of this whole ring walk: with `&self` it could not call this.
    ///
    /// In line at both callers: out of line it became a shared symbol and cost
    /// +12 B, because each caller freezes a different length.
    #[inline(always)]
    fn read_bytes_from(
        bytes: &[u8],
        base: usize,
        ring: usize,
        out: &mut [u8],
        tail: usize,
    ) -> usize {
        if out.len() <= ring {
            let upto = ring.wrapping_sub(tail);
            let first = out.len().min(upto);
            let from = base.wrapping_add(tail);
            if let (Some(dst), Some(src)) = (
                out.get_mut(..first),
                bytes.get(from..from.wrapping_add(first)),
            ) {
                dst.copy_from_slice(src);
            }
            let rest = out.len().wrapping_sub(first);
            if rest > 0 {
                if let (Some(dst), Some(src)) = (
                    out.get_mut(first..),
                    bytes.get(base..base.wrapping_add(rest)),
                ) {
                    dst.copy_from_slice(src);
                }
                return rest;
            }
            let next = tail.wrapping_add(first);
            return if next >= ring { 0 } else { next };
        }
        tail
    }

    /// The message length back out, and where the message starts.
    /// Bounded as [`Kernel::read_bytes`] is, and converted for the same
    /// reasons: `tail` is below `length`, `base + length` is at most
    /// `BYTES`, `first` is `min(want, upto)`, and every index passes through
    /// a `get` that refuses anything the ring does not own.
    /// The prefix is converted at the TARGET's width, not always through `u64`.
    ///
    /// This used `[0_u8; 8]` and `u64::from_le_bytes(..) as usize`. On rv32 that
    /// reads eight bytes as two words and throws the top one away, because
    /// `configMESSAGE_BUFFER_LENGTH_TYPE` is `size_t` and `size_t` is FOUR bytes
    /// there. `usize` is the width the C's type actually has on whichever target
    /// this is, so it is the right one to build.
    fn read_length_prefix_from(
        bytes: &[u8],
        base: usize,
        ring: usize,
        tail: usize,
    ) -> (usize, usize) {
        if Self::MESSAGE_LENGTH_BYTES > core::mem::size_of::<usize>() {
            // The wide prefix `write_message` writes zero-extended: read all
            // of it, so the ring advances past it, and keep the value -- a
            // length this kernel wrote, so it fits `usize`.
            let mut wide = [0_u8; 8];
            let next = match wide.get_mut(..Self::MESSAGE_LENGTH_BYTES) {
                Some(slot) => Self::read_bytes_from(bytes, base, ring, slot, tail),
                None => tail,
            };
            return (
                usize::try_from(u64::from_le_bytes(wide)).unwrap_or(usize::MAX),
                next,
            );
        }
        let mut raw = [0_u8; core::mem::size_of::<usize>()];
        // Delegated. This held its OWN two-copy ring walk plus a byte-at-a-time
        // fallback -- a third hand-rolled version of the same walk -- and the
        // only reason was that `read_bytes` asked for `&mut self` when it never
        // needed it.
        // A prefix wider than the target's `usize` cannot be represented, and
        // on the oracle host the two are equal, so this clamp is the identity
        // wherever the arms are matched.
        let want = Self::MESSAGE_LENGTH_BYTES.min(raw.len());
        let next = match raw.get_mut(..want) {
            Some(slot) => Self::read_bytes_from(bytes, base, ring, slot, tail),
            None => tail,
        };
        (usize::from_le_bytes(raw), next)
    }
}

// ================================================================ tests ==

/// `stream.rs` had no unit test at all, and six of its APIs are also the
/// ones no conformance scenario reaches (`docs/HOLES.md`, H2 and H3). For
/// those, the only evidence available is a semantic audit against the C
/// source plus tests that pin what it says.
///
/// So every test here quotes the `stream_buffer.c` line it is pinning, and
/// pins **the C's contract** rather than this kernel's behaviour. A test
/// written the other way round proves only that the code still does what it
/// did, which is worth having and is not evidence of conformance.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use rusty_rtos_core::config::Config;
    use rusty_rtos_core::hooks::NoTickHook;

    use crate::queue::Wait;
    use crate::system::tests::{NoTrace, TestConfig, TestPort};

    /// A kernel with buffers, which `system!` never declares: the macro
    /// sizes `BUFFERS` and `BYTES` at zero. Four tasks is the idle task,
    /// the timer daemon and one of our own, with room to spare.
    type K = crate::Kernel<
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
        { <TestConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
    >;

    fn kernel() -> K {
        K::new(TestPort::default(), NoTrace).expect("the declared geometry adds up")
    }

    /// The header a message buffer writes in front of every message.
    const HDR: usize = K::MESSAGE_LENGTH_BYTES;

    /// `xStreamBufferIsEmpty`:
    ///
    /// ```c
    /// if( pxStreamBuffer->xHead == xTail ) { xReturn = pdTRUE; }
    /// ```
    ///
    /// Head-equals-tail, which is emptiness and NOT "nothing readable" --
    /// the two differ on a message buffer holding only a header, which the
    /// zero-length-message test below pins.
    #[test]
    fn is_empty_is_head_equals_tail() {
        let mut k = kernel();
        let b = k.stream_buffer_create(8, 1).expect("a buffer");
        assert_eq!(k.stream_buffer_is_empty(b), Ok(true));

        k.stream_buffer_send(b, b"abc", 0).expect("send");
        assert_eq!(k.stream_buffer_is_empty(b), Ok(false));

        let mut out = [0_u8; 8];
        k.stream_buffer_receive(b, &mut out, 0).expect("receive");
        assert_eq!(
            k.stream_buffer_is_empty(b),
            Ok(true),
            "drained is empty again, wherever head and tail have walked to"
        );
    }

    /// `xStreamBufferIsFull` on a STREAM buffer:
    ///
    /// ```c
    /// xBytesToStoreMessageLength = 0;   /* not a message buffer */
    /// if( xStreamBufferSpacesAvailable( xStreamBuffer ) <= xBytesToStoreMessageLength )
    /// ```
    ///
    /// so full is exactly "no spaces left".
    #[test]
    fn is_full_on_a_stream_buffer_means_no_spaces_at_all() {
        let mut k = kernel();
        let b = k.stream_buffer_create(8, 1).expect("a buffer");
        assert_eq!(k.stream_buffer_spaces_available(b), Ok(8));
        assert_eq!(k.stream_buffer_is_full(b), Ok(false));

        k.stream_buffer_send(b, b"1234567", 0).expect("send seven");
        assert_eq!(k.stream_buffer_spaces_available(b), Ok(1));
        assert_eq!(
            k.stream_buffer_is_full(b),
            Ok(false),
            "one free byte is not full on a STREAM buffer"
        );

        k.stream_buffer_send(b, b"8", 0).expect("send the last");
        assert_eq!(k.stream_buffer_spaces_available(b), Ok(0));
        assert_eq!(k.stream_buffer_is_full(b), Ok(true));
    }

    /// `xStreamBufferIsFull` on a MESSAGE buffer, which is the one that
    /// surprises:
    ///
    /// ```c
    /// if( ( pxStreamBuffer->ucFlags & sbFLAGS_IS_MESSAGE_BUFFER ) != 0 )
    ///     xBytesToStoreMessageLength = sbBYTES_TO_STORE_MESSAGE_LENGTH;
    /// if( xStreamBufferSpacesAvailable( xStreamBuffer ) <= xBytesToStoreMessageLength )
    /// ```
    ///
    /// `<=`, against the size of the LENGTH HEADER rather than against
    /// zero. So a message buffer reports FULL while it still has free
    /// bytes, because those bytes could not hold even a zero-length
    /// message's header.
    ///
    /// A caller that polls `is_full` and expects `spaces_available() == 0`
    /// is wrong by up to a header, and only on message buffers.
    #[test]
    fn is_full_on_a_message_buffer_leaves_a_header_of_bytes_free() {
        let mut k = kernel();
        let b = k.message_buffer_create(8).expect("a message buffer");
        assert_eq!(k.stream_buffer_spaces_available(b), Ok(8));
        assert_eq!(k.stream_buffer_is_full(b), Ok(false));

        // One three-byte message costs the header plus the payload.
        k.stream_buffer_send(b, b"abc", 0).expect("send");
        let spaces = k.stream_buffer_spaces_available(b).expect("spaces");
        assert_eq!(spaces, 8 - (HDR + 3));
        assert!(spaces > 0, "there ARE free bytes");
        assert!(spaces <= HDR, "but not enough for another header");
        assert_eq!(
            k.stream_buffer_is_full(b),
            Ok(true),
            "full with {spaces} byte(s) free: the C compares against the header, not zero"
        );
    }

    /// `xStreamBufferNextMessageLengthBytes` on a stream buffer:
    ///
    /// ```c
    /// if( ( pxStreamBuffer->ucFlags & sbFLAGS_IS_MESSAGE_BUFFER ) != 0 ) { ... }
    /// else { xReturn = 0; }
    /// ```
    ///
    /// Zero for a stream buffer NO MATTER WHAT IT HOLDS. That is not "no
    /// message waiting", it is "the question does not apply" -- and the
    /// return type cannot tell a caller which of the two it got.
    #[test]
    fn next_message_length_is_zero_on_a_stream_buffer_however_full() {
        let mut k = kernel();
        let b = k.stream_buffer_create(8, 1).expect("a buffer");
        assert_eq!(k.stream_buffer_next_message_length(b), Ok(0));

        k.stream_buffer_send(b, b"abcde", 0).expect("send");
        assert_eq!(k.stream_buffer_bytes_available(b), Ok(5));
        assert_eq!(
            k.stream_buffer_next_message_length(b),
            Ok(0),
            "five bytes waiting and still zero, because it is not a message buffer"
        );
    }

    /// The same call on a message buffer PEEKS:
    ///
    /// ```c
    /// ( void ) prvReadBytesFromBuffer( pxStreamBuffer, ..., pxStreamBuffer->xTail );
    /// ```
    ///
    /// It reads at the tail without advancing it, so asking twice answers
    /// twice and the message is still there to be received.
    #[test]
    fn next_message_length_peeks_and_does_not_consume() {
        let mut k = kernel();
        let b = k.message_buffer_create(16).expect("a message buffer");
        k.stream_buffer_send(b, b"hello", 0).expect("send");

        assert_eq!(k.stream_buffer_next_message_length(b), Ok(5));
        assert_eq!(
            k.stream_buffer_next_message_length(b),
            Ok(5),
            "asking did not consume it"
        );

        let mut out = [0_u8; 16];
        assert_eq!(k.stream_buffer_receive(b, &mut out, 0), Ok(Wait::Ready(5)));
        assert_eq!(&out[..5], b"hello");
        assert_eq!(k.stream_buffer_next_message_length(b), Ok(0), "drained");
    }

    /// A zero-length message is a NO-OP that is indistinguishable from a
    /// send that failed -- which is not visible from the call site, and is
    /// the sort of thing only a source audit finds.
    ///
    /// `prvWriteMessageToBuffer`:
    ///
    /// ```c
    /// if( message buffer ) {
    ///     xMessageLength = ( configMESSAGE_BUFFER_LENGTH_TYPE ) xDataLengthBytes;  /* 0 */
    ///     if( xSpace >= xRequiredSpace ) {
    ///         xNextHead = prvWriteBytesToBuffer( pxStreamBuffer, &xMessageLength,
    ///                                            sbBYTES_TO_STORE_MESSAGE_LENGTH, xNextHead );
    ///     }
    /// }
    /// if( xDataLengthBytes != ( size_t ) 0 ) {
    ///     pxStreamBuffer->xHead = prvWriteBytesToBuffer( ..., xNextHead );
    /// }
    /// return xDataLengthBytes;
    /// ```
    ///
    /// The header IS written into the storage array -- but the new position
    /// goes into `xNextHead`, a LOCAL. `pxStreamBuffer->xHead` is only
    /// committed inside the `xDataLengthBytes != 0` branch, which a
    /// zero-length message does not take.
    ///
    /// So the bytes are in the array and the buffer does not know it: the
    /// head never moves, the buffer stays empty, the call returns 0, and
    /// the next send writes over them. A caller cannot tell this apart from
    /// a send that was refused for want of space.
    ///
    /// Pinned because the earlier version of this test asserted the
    /// opposite -- that the header would show up in `bytes_available` --
    /// and the kernel was right.
    #[test]
    fn a_zero_length_message_is_a_no_op_that_looks_like_a_failed_send() {
        let mut k = kernel();
        let b = k.message_buffer_create(16).expect("a message buffer");
        assert_eq!(k.stream_buffer_is_empty(b), Ok(true));

        assert_eq!(
            k.stream_buffer_send(b, b"", 0),
            Ok(Wait::Ready(0)),
            "zero bytes written, which is also what a refused send answers"
        );
        assert_eq!(
            k.stream_buffer_bytes_available(b),
            Ok(0),
            "the header went into the array and the head was never committed"
        );
        assert_eq!(k.stream_buffer_is_empty(b), Ok(true));
        assert_eq!(k.stream_buffer_next_message_length(b), Ok(0));

        // And a real message lands in the same place, over the top of it.
        k.stream_buffer_send(b, b"ok", 0).expect("a real message");
        assert_eq!(k.stream_buffer_next_message_length(b), Ok(2));
        let mut out = [0_u8; 16];
        assert_eq!(k.stream_buffer_receive(b, &mut out, 0), Ok(Wait::Ready(2)));
        assert_eq!(&out[..2], b"ok");
        assert_eq!(k.stream_buffer_is_empty(b), Ok(true));
    }

    /// `xStreamBufferReset`:
    ///
    /// ```c
    /// if( ( pxStreamBuffer->xTaskWaitingToReceive == NULL ) &&
    ///     ( pxStreamBuffer->xTaskWaitingToSend == NULL ) )
    /// {
    ///     prvInitialiseNewStreamBuffer( ..., pxStreamBuffer->xTriggerLevelBytes, ... );
    ///     xReturn = pdPASS;
    /// }
    /// ```
    ///
    /// The buffer's own trigger level is passed straight back in, so a
    /// reset empties the buffer and KEEPS the trigger. It is not a return
    /// to the creation default, which is what the name suggests.
    #[test]
    fn reset_empties_the_buffer_and_keeps_the_trigger_level() {
        let mut k = kernel();
        let b = k.stream_buffer_create(8, 1).expect("a buffer");
        assert_eq!(k.stream_buffer_set_trigger_level(b, 5), Ok(true));
        k.stream_buffer_send(b, b"abcd", 0).expect("send");
        assert_eq!(k.stream_buffer_bytes_available(b), Ok(4));

        assert_eq!(k.stream_buffer_reset(b), Ok(true));
        assert_eq!(k.stream_buffer_is_empty(b), Ok(true));
        assert_eq!(k.stream_buffer_bytes_available(b), Ok(0));
        assert_eq!(
            k.stream_buffer_spaces_available(b),
            Ok(8),
            "the whole usable length is back"
        );
    }

    /// The other half of that guard: a reset must REFUSE while a task is
    /// blocked on the buffer, because reinitialising would drop the handle
    /// that task is waiting on.
    #[test]
    fn reset_refuses_while_a_task_is_waiting_to_receive() {
        let mut k = kernel();
        let b = k.stream_buffer_create(8, 1).expect("a buffer");
        k.create_task("rx", 1).expect("a task");
        let started = k.start_scheduler().expect("start");
        k.suspend(Some(started.timer)).expect("park the daemon");

        // Run until our task is the one on the CPU, then block it.
        let mut out = [0_u8; 8];
        let mut blocked = false;
        for _ in 0..50_u32 {
            let running = k.name_of(k.current()).expect("a running task has a name");
            if running.as_str() == "rx" {
                assert_eq!(
                    k.stream_buffer_receive(b, &mut out, 20),
                    Ok(Wait::Blocked),
                    "an empty buffer with a timeout blocks"
                );
                blocked = true;
                break;
            }
            k.switch_context();
        }
        assert!(blocked, "never got the task onto the CPU");

        assert_eq!(
            k.stream_buffer_reset(b),
            Ok(false),
            "a waiting receiver refuses the reset"
        );
    }

    /// `xStreamBufferReceiveFromISR` never blocks: it takes a whole
    /// message, or nothing.
    #[test]
    fn receive_from_isr_takes_a_whole_message_or_nothing() {
        let mut k = kernel();
        let b = k.message_buffer_create(16).expect("a message buffer");
        let mut out = [0_u8; 16];

        let (n, _) = k.stream_buffer_receive_from_isr(b, &mut out).expect("isr");
        assert_eq!(n, 0, "nothing waiting, and it did not block");

        k.stream_buffer_send(b, b"xy", 0).expect("send");
        let (n, _) = k.stream_buffer_receive_from_isr(b, &mut out).expect("isr");
        assert_eq!(n, 2);
        assert_eq!(&out[..2], b"xy");
        assert_eq!(k.stream_buffer_is_empty(b), Ok(true));
    }

    /// `bytes_available` and `spaces_available` partition the USABLE
    /// length, which is one less than the ring's own -- the spare byte that
    /// tells full from empty, the C's `xBufferSizeBytes++` at creation.
    #[test]
    fn bytes_and_spaces_partition_the_usable_length() {
        let mut k = kernel();
        let b = k.stream_buffer_create(8, 1).expect("a buffer");
        for n in 0..=8_usize {
            let bytes = k.stream_buffer_bytes_available(b).expect("bytes");
            let spaces = k.stream_buffer_spaces_available(b).expect("spaces");
            assert_eq!(
                bytes.saturating_add(spaces),
                8,
                "at {n} byte(s) in: {bytes} + {spaces} should be the usable 8"
            );
            if n < 8 {
                k.stream_buffer_send(b, b"z", 0).expect("send one");
            }
        }
    }
    /// A STREAM buffer takes a partial write; a MESSAGE buffer is
    /// all-or-nothing. `prvWriteMessageToBuffer`:
    ///
    /// ```c
    /// if( message buffer ) {
    ///     if( xSpace >= xRequiredSpace ) { ...write the header... }
    ///     else { xDataLengthBytes = 0; }          /* nothing at all */
    /// } else {
    ///     xDataLengthBytes = configMIN( xDataLengthBytes, xSpace );   /* as much as fits */
    /// }
    /// ```
    ///
    /// So the same call with the same arguments loses the tail on one kind
    /// and loses everything on the other, and the return value is the only
    /// way to tell which happened.
    #[test]
    fn a_stream_takes_a_partial_write_and_a_message_is_all_or_nothing() {
        let mut k = kernel();
        let s = k.stream_buffer_create(8, 1).expect("a stream buffer");
        k.stream_buffer_send(s, b"123456", 0).expect("six");
        assert_eq!(
            k.stream_buffer_send(s, b"789", 0),
            Ok(Wait::Ready(2)),
            "two of the three fit, and two is what it took"
        );
        assert_eq!(k.stream_buffer_spaces_available(s), Ok(0));

        let mut k = kernel();
        let m = k.message_buffer_create(8).expect("a message buffer");
        // One message of two costs the header plus two: six of the eight.
        k.stream_buffer_send(m, b"xy", 0)
            .expect("the first message");
        assert_eq!(
            k.stream_buffer_send(m, b"zzz", 0),
            Ok(Wait::Ready(0)),
            "the header plus three does not fit, so NOTHING was written"
        );
        assert_eq!(
            k.stream_buffer_next_message_length(m),
            Ok(2),
            "and the message already there is untouched"
        );
    }

    /// `xStreamBufferSendFromISR` writes what fits and never blocks -- the
    /// same body as the task-side send with the block time forced to zero.
    #[test]
    fn send_from_isr_writes_what_fits_and_never_blocks() {
        let mut k = kernel();
        let b = k.stream_buffer_create(8, 1).expect("a buffer");

        let (n, _) = k.stream_buffer_send_from_isr(b, b"abc").expect("isr");
        assert_eq!(n, 3);
        assert_eq!(k.stream_buffer_bytes_available(b), Ok(3));

        // Past the end: it takes the five that fit rather than waiting.
        let (n, _) = k.stream_buffer_send_from_isr(b, b"defghijkl").expect("isr");
        assert_eq!(n, 5, "five of the nine fit");
        assert_eq!(k.stream_buffer_is_full(b), Ok(true));

        let (n, _) = k.stream_buffer_send_from_isr(b, b"m").expect("isr");
        assert_eq!(n, 0, "full, and it still did not block");
    }

    /// A round trip through the two ISR halves, which is what an
    /// interrupt-driven driver actually does.
    #[test]
    fn a_message_survives_a_round_trip_through_both_isr_halves() {
        let mut k = kernel();
        let b = k.message_buffer_create(16).expect("a message buffer");
        let (n, _) = k.stream_buffer_send_from_isr(b, b"ping").expect("isr");
        assert_eq!(n, 4);

        let mut out = [0_u8; 16];
        let (n, _) = k.stream_buffer_receive_from_isr(b, &mut out).expect("isr");
        assert_eq!(n, 4);
        assert_eq!(&out[..4], b"ping");
        assert_eq!(k.stream_buffer_is_empty(b), Ok(true));
    }

    /// Four buffers over a 64-byte arena: room to fill it exactly, punch a
    /// hole and refill it -- which the geometry above (two buffers) cannot.
    type K4 = crate::Kernel<
        TestConfig,
        TestPort,
        NoTrace,
        NoTickHook,
        4,
        { crate::list_slots_for(4, 0, crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 0)) },
        { crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 0) },
        1,
        1,
        4,
        64,
        0,
        0,
        { <TestConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
    >;

    fn arena() -> K4 {
        K4::new(TestPort::default(), NoTrace).expect("the declared geometry adds up")
    }

    /// The byte arena is Kairos storage -- the C mallocs -- so no compared
    /// run judges it, and no run filled it: its exact-fill, exact-fit and
    /// split boundaries all survived plan P5's mutants. A buffer of `n`
    /// takes `n + 1` bytes (the spare byte), so two of 31 fill 64 exactly.
    #[test]
    fn the_byte_arena_fills_exactly_and_a_hole_refits_exactly() {
        use rusty_rtos_core::error::Error;
        let mut k = arena();
        let a = k.stream_buffer_create(31, 1).expect("the first half");
        let _b = k
            .stream_buffer_create(31, 1)
            .expect("the second half fills the arena to its last byte");
        assert_eq!(
            k.stream_buffer_create(1, 1),
            Err(Error::Full),
            "and nothing more fits"
        );

        k.stream_buffer_delete(a).expect("delete");
        k.stream_buffer_create(31, 1)
            .expect("the same size refits the hole exactly");
        assert_eq!(k.stream_buffer_create(1, 1), Err(Error::Full));
    }

    /// A smaller buffer splits a hole and the remainder stays usable; two
    /// freed neighbours coalesce, whichever of them is freed first.
    #[test]
    fn a_split_hole_keeps_its_remainder_and_freed_neighbours_coalesce() {
        use rusty_rtos_core::error::Error;
        let mut k = arena();
        let a = k.stream_buffer_create(31, 1).expect("a");
        let _b = k.stream_buffer_create(31, 1).expect("b: full");
        k.stream_buffer_delete(a).expect("delete a");

        // The lower neighbour freed first, then the upper.
        let c = k.stream_buffer_create(15, 1).expect("half the hole");
        let d = k.stream_buffer_create(15, 1).expect("the remainder of it");
        assert_eq!(k.stream_buffer_create(1, 1), Err(Error::Full));
        k.stream_buffer_delete(c).expect("delete c");
        k.stream_buffer_delete(d).expect("delete d");
        let e = k
            .stream_buffer_create(31, 1)
            .expect("the halves coalesced, lower freed first");

        // The upper neighbour freed first, then the lower.
        k.stream_buffer_delete(e).expect("delete e");
        let c = k.stream_buffer_create(15, 1).expect("c again");
        let d = k.stream_buffer_create(15, 1).expect("d again");
        k.stream_buffer_delete(d).expect("delete d");
        k.stream_buffer_delete(c).expect("delete c");
        k.stream_buffer_create(31, 1)
            .expect("the halves coalesced, upper freed first");
    }

    /// `xStreamBufferCreate` refuses a zero size, and a trigger above the
    /// size, each on its own (plan P5: `||` -> `&&` survived).
    #[test]
    fn a_zero_size_or_a_trigger_above_the_size_is_refused() {
        use rusty_rtos_core::error::Error;
        let mut k = kernel();
        assert_eq!(k.stream_buffer_create(0, 0), Err(Error::InvalidArgument));
        assert_eq!(k.stream_buffer_create(4, 5), Err(Error::InvalidArgument));
        assert!(k.stream_buffer_create(4, 4).is_ok());
    }

    /// [`TestConfig`] with an eight-byte message length: a
    /// `configMESSAGE_BUFFER_LENGTH_TYPE` wider than a 32-bit target's
    /// `size_t`, which the C allows.
    struct WidePrefixConfig;
    impl Config for WidePrefixConfig {
        type Tick = rusty_rtos_core::tick::Bits32;
        const TICK_RATE_HZ: u32 = 100;
        const MAX_PRIORITIES: u8 = 4;
        const MINIMAL_STACK_SIZE: usize = 1;
        const MAX_TASK_NAME_LEN: usize = 8;
        const TIMER_TASK_PRIORITY: u8 = 3;
        const TIMER_TASK_STACK_DEPTH: usize = 1;
        const TIMER_QUEUE_LENGTH: usize = 1;
        const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
        const MESSAGE_LENGTH_BYTES: usize = 8;
    }

    /// A message survives a round trip under a prefix wider than `usize`.
    /// On a 32-bit target this was broken (plan P5, found by the API
    /// differential's `-m32` twin): the send wrote four bytes of prefix and
    /// counted eight, so the receive took the message's first byte and
    /// nothing more. Run it at the targets' width with
    /// `cargo test --target i686-pc-windows-msvc`; on a 64-bit host the
    /// prefix fits `usize` and this is the ordinary path.
    #[test]
    fn a_message_round_trips_under_a_prefix_wider_than_usize() {
        type KW = crate::Kernel<
            WidePrefixConfig,
            TestPort,
            NoTrace,
            NoTickHook,
            4,
            { crate::list_slots_for(4, 0, crate::lists_for(4, 1, 0)) },
            { crate::lists_for(4, 1, 0) },
            1,
            1,
            2,
            64,
            0,
            0,
            1,
        >;
        let mut k = KW::new(TestPort::default(), NoTrace).expect("the declared geometry adds up");
        let b = k.message_buffer_create(32).expect("a message buffer");
        k.stream_buffer_send(b, b"hello", 0).expect("send");
        assert_eq!(
            k.stream_buffer_bytes_available(b),
            Ok(13),
            "eight of prefix, five of message"
        );
        assert_eq!(k.stream_buffer_next_message_length(b), Ok(5));
        let mut out = [0_u8; 16];
        assert_eq!(k.stream_buffer_receive(b, &mut out, 0), Ok(Wait::Ready(5)));
        assert_eq!(out.get(..5), Some(&b"hello"[..]));
    }

    /// A port that raises ONE tick, at a chosen outermost critical-section
    /// exit -- the sim contract's clock (`ORACLES.md`), aimed. `TestPort`
    /// never ticks, so nothing in this file could switch a task away inside
    /// a kernel call, and the stream's resume-after-wait arm had no test.
    #[derive(Debug, Default)]
    struct TickAt {
        nesting: core::cell::Cell<u32>,
        exits: core::cell::Cell<u64>,
        at: core::cell::Cell<u64>,
        pending: core::cell::Cell<bool>,
        counting: core::cell::Cell<bool>,
        in_tick: core::cell::Cell<bool>,
    }

    impl rusty_rtos_core::port::Port for TickAt {
        fn yield_now(&self) {}
        fn yield_from_isr(&self, _woken: rusty_rtos_core::isr::Woken) {}
        fn enter_critical(&self) {
            self.nesting.set(self.nesting.get().saturating_add(1));
        }
        fn exit_critical(&self) {
            let n = self.nesting.get().saturating_sub(1);
            self.nesting.set(n);
            if n == 0 && self.counting.get() && !self.in_tick.get() {
                let exits = self.exits.get().wrapping_add(1);
                self.exits.set(exits);
                if exits == self.at.get() {
                    self.pending.set(true);
                }
            }
        }
        fn set_interrupt_mask_from_isr(&self) -> u32 {
            0
        }
        fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
        fn in_isr(&self) -> bool {
            self.in_tick.get()
        }
        fn set_in_tick_entry(&self, yes: bool) {
            self.in_tick.set(yes);
        }
        fn take_pending_tick(&self) -> bool {
            self.pending.replace(false)
        }
        fn exits(&self) -> u64 {
            self.exits.get()
        }
        fn scheduler_started(&self) {
            self.counting.set(true);
        }
    }

    type KT = crate::Kernel<
        TestConfig,
        TickAt,
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
        { <TestConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
    >;

    /// `xStreamBufferReceive` that timed out, preempted at the exit ending
    /// its wait, returns 0 when it runs again -- it does not wait a second
    /// time. The C's thread is stopped INSIDE that exit and goes on to read
    /// what the buffer holds; the stackless call records that the wait is
    /// done (`set_stream_waited`) and the resumed call reads it
    /// (`take_stream_waited`). Neither half had an oracle (HOLES.md H14): the
    /// differential delivers ticks between calls, and no corpus scenario
    /// lands a tick on that exit in 100,000 ticks.
    ///
    /// R (priority 2) receives on an empty buffer with a one-tick timeout; H
    /// (priority 3) delays two ticks, so it wakes on the tick AFTER R's
    /// timeout. That tick is raised at each exit of R's resumed call in turn:
    /// wherever it lands, H preempts, and R's next call must answer 0.
    #[test]
    fn a_receive_preempted_as_its_wait_ends_does_not_wait_again() {
        let mut preempted_inside = 0;
        for k in 1..=12_u64 {
            let mut kx =
                KT::new(TickAt::default(), NoTrace).expect("the declared geometry adds up");
            let sb = kx.stream_buffer_create(16, 1).expect("a stream buffer");
            let r = kx.create_task("R", 2).expect("R");
            let h = kx.create_task("H", 3).expect("H");
            let start = kx.start_scheduler().expect("start");
            kx.suspend(Some(start.timer)).expect("park the daemon");
            // H runs first and sleeps past R's timeout.
            while kx.current() != h {
                kx.switch_context();
            }
            kx.delay(2).expect("H delays");
            while kx.current() != r {
                kx.switch_context();
            }
            let mut out = [0u8; 4];
            assert_eq!(kx.stream_buffer_receive(sb, &mut out, 1), Ok(Wait::Blocked));
            // One tick: R times out and is the highest ready task.
            if kx.increment_tick() {
                kx.switch_context();
            }
            while kx.current() != r {
                kx.switch_context();
            }
            // Now R's call resumes; the next tick -- H's -- lands at the
            // k-th exit inside it.
            let base = kx.port().exits.get();
            kx.port().at.set(base.wrapping_add(k));
            let first = kx.stream_buffer_receive(sb, &mut out, 1);
            if first == Ok(Wait::Blocked) {
                if kx.current() == h {
                    preempted_inside += 1;
                }
                // H runs and sleeps again; R runs again and finishes its call.
                while kx.current() != h {
                    kx.switch_context();
                }
                kx.delay(50).expect("H delays");
                while kx.current() != r {
                    kx.switch_context();
                }
                assert_eq!(
                    kx.stream_buffer_receive(sb, &mut out, 1),
                    Ok(Wait::Ready(0)),
                    "tick at exit {k} of the resumed call: the timed-out receive must answer 0, \
                     not wait again"
                );
            } else {
                assert_eq!(first, Ok(Wait::Ready(0)), "tick at exit {k}");
            }
        }
        assert!(
            preempted_inside > 0,
            "no alignment preempted R inside its resumed call: the test reached nothing"
        );
    }
}

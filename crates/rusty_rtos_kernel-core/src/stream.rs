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

use rusty_rtos_core::config::Config;
use rusty_rtos_core::error::{Error, Result};
use rusty_rtos_core::handle::{StreamBufferHandle, TaskHandle};
use rusty_rtos_core::hooks::TickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::trace::{Event, Trace};

use crate::kernel::{Kernel, NotifyAction};
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
> Kernel<C, P, T, H, TASKS, ITEMS, LISTS, QUEUES, SLOTS, BUFFERS, BYTES, TIMERS, GROUPS>
where
    H: TickHook<Self>,
{
    /// How many bytes a message buffer spends on each message's length.
    pub const MESSAGE_LENGTH_BYTES: usize = C::MESSAGE_LENGTH_BYTES;

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
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace.event(
            tick,
            Event::StreamBufferCreate {
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
                *slot = (base.saturating_add(length), free.saturating_sub(length));
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
            i = i.saturating_add(1);
        }
        // A block at the very end goes back to the bump pointer instead of
        // the list, which is what keeps a create/delete loop free.
        if base.saturating_add(length) == self.bytes_used {
            self.bytes_used = base;
            return;
        }
        if let Some(slot) = self.free_blocks.get_mut(self.free_count) {
            *slot = (base, length);
            self.free_count = self.free_count.saturating_add(1);
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
        let _ = self.buffers.remove(buffer);
        self.give_bytes(b.base, b.length);
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
            Ok(b)
                if b.waiting_to_receive == TaskHandle::NULL
                    && b.waiting_to_send == TaskHandle::NULL =>
            {
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
        Ok(self.read_length_prefix(&b, b.tail).0)
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
    pub fn stream_buffer_send(
        &mut self,
        buffer: StreamBufferHandle,
        data: &[u8],
        ticks: u64,
    ) -> Result<Wait<usize>> {
        let caller = self.current;
        let snapshot = *self.buffers.resolve(buffer)?;
        let max_reported = snapshot.length.saturating_sub(1);
        let mut required = data.len();
        let mut ticks = ticks;
        if snapshot.is_message {
            required = required.saturating_add(Self::MESSAGE_LENGTH_BYTES);
            if required > max_reported {
                // It will never fit, so do not wait for it to.
                ticks = 0;
            }
        } else if required > max_reported {
            required = max_reported;
        }

        let mut sampled = self.take_stream_resume(caller);
        if sampled.is_some() {
            // Resuming after the exit that sampled the space: fall through
            // to the write with what that sample said.
        } else if self.notify_wait_pending(caller) {
            match self.notify_wait(snapshot.notify_index, 0, 0, ticks)? {
                Blocked => return Ok(Blocked),
                Ready(_) => {}
            }
            self.buffers.resolve_mut(buffer)?.waiting_to_send = TaskHandle::NULL;
        } else if ticks > 0 {
            self.enter_critical();
            let space = self.buffers.resolve(buffer)?.spaces_available();
            let must_block = space < required;
            if must_block {
                let _ = self.notify_state_clear(None, snapshot.notify_index);
                self.buffers.resolve_mut(buffer)?.waiting_to_send = caller;
            }
            self.exit_critical();
            // That exit can release a tick, and a tick can switch this task
            // away. The C's thread stops inside the exit and everything
            // below it runs when the task is resumed — with the sample it
            // already took, and without paying for the section twice.
            if self.current != caller {
                self.set_stream_resume(caller, space);
                return Ok(Blocked);
            }
            if must_block {
                // `traceBLOCKING_ON_STREAM_BUFFER_SEND` is not one of the
                // harness's hooks, so blocking says nothing on either side.
                match self.notify_wait(snapshot.notify_index, 0, 0, ticks)? {
                    Blocked => return Ok(Blocked),
                    Ready(_) => {
                        self.buffers.resolve_mut(buffer)?.waiting_to_send = TaskHandle::NULL;
                    }
                }
            }
        }

        let space = match sampled.take() {
            Some(space) => space,
            None => self.buffers.resolve(buffer)?.spaces_available(),
        };
        let written = self.write_message(buffer, data, space, required)?;
        if written > 0 {
            let tick = self.tick;
            self.trace.note_exits(self.port.exits());
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

    /// `xStreamBufferReceive`: bytes into `out`, and how many arrived.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn stream_buffer_receive(
        &mut self,
        buffer: StreamBufferHandle,
        out: &mut [u8],
        ticks: u64,
    ) -> Result<Wait<usize>> {
        let caller = self.current;
        let snapshot = *self.buffers.resolve(buffer)?;
        let prefix = if snapshot.is_message {
            Self::MESSAGE_LENGTH_BYTES
        } else {
            0
        };
        let mut available;
        if let Some(local) = self.take_stream_resume(caller) {
            // As above: resume after the exit, with the sample it took.
            available = local;
        } else if self.notify_wait_pending(caller) {
            // Resuming: the C's wait returns here, so its second half runs
            // whatever the buffer now holds.
            match self.notify_wait(snapshot.notify_index, 0, 0, ticks)? {
                Blocked => return Ok(Blocked),
                Ready(_) => {}
            }
            self.buffers.resolve_mut(buffer)?.waiting_to_receive = TaskHandle::NULL;
            available = self.buffers.resolve(buffer)?.bytes_in_buffer();
        } else if ticks > 0 {
            self.enter_critical();
            available = self.buffers.resolve(buffer)?.bytes_in_buffer();
            let must_block = available <= prefix;
            if must_block {
                let _ = self.notify_state_clear(None, snapshot.notify_index);
                self.buffers.resolve_mut(buffer)?.waiting_to_receive = caller;
            }
            self.exit_critical();
            // That exit can release a tick, and a tick can switch this task
            // away. The C's thread stops inside the exit and everything
            // below it runs when the task is resumed — with the sample it
            // already took, and without paying for the section twice.
            if self.current != caller {
                self.set_stream_resume(caller, available);
                return Ok(Blocked);
            }
            if must_block {
                match self.notify_wait(snapshot.notify_index, 0, 0, ticks)? {
                    Blocked => return Ok(Blocked),
                    Ready(_) => {
                        self.buffers.resolve_mut(buffer)?.waiting_to_receive = TaskHandle::NULL;
                        available = self.buffers.resolve(buffer)?.bytes_in_buffer();
                    }
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
                self.trace.note_exits(self.port.exits());
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
    fn send_completed(&mut self, buffer: StreamBufferHandle) -> Result<()> {
        if H::send_completed(self, buffer) {
            return Ok(());
        }
        self.suspend_all();
        let waiting = self.buffers.resolve(buffer)?.waiting_to_receive;
        if waiting != TaskHandle::NULL {
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
        if waiting == TaskHandle::NULL {
            return Ok(Woken::NO);
        }
        let index = self.buffers.resolve(buffer)?.notify_index;
        let (_, woken) = self.notify_from_isr(waiting, index, 0, NotifyAction::None)?;
        self.buffers.resolve_mut(buffer)?.waiting_to_receive = TaskHandle::NULL;
        Ok(woken)
    }

    /// `sbRECEIVE_COMPLETED`.
    fn receive_completed(&mut self, buffer: StreamBufferHandle) -> Result<()> {
        self.suspend_all();
        let waiting = self.buffers.resolve(buffer)?.waiting_to_send;
        if waiting != TaskHandle::NULL {
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
        if waiting == TaskHandle::NULL {
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
        let b = *self.buffers.resolve(buffer)?;
        let mut next_head = b.head;
        let mut length = data.len();
        let mut space = space;
        if b.is_message {
            if space >= required {
                next_head = self.write_length_prefix(&b, length, next_head);
            } else {
                length = 0;
            }
            space = space.saturating_sub(Self::MESSAGE_LENGTH_BYTES);
        }
        let length = length.min(space);
        if length != 0 {
            let chunk = data.get(..length).unwrap_or(data);
            let head = self.write_bytes(&b, chunk, next_head);
            self.buffers.resolve_mut(buffer)?.head = head;
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
        let b = *self.buffers.resolve(buffer)?;
        let mut next_tail = b.tail;
        let mut available = available;
        let next_length;
        if b.is_message {
            let (message_length, tail) = self.read_length_prefix(&b, next_tail);
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
            let slot = out.get_mut(..count);
            let tail = match slot {
                Some(slot) => self.read_bytes(&b, slot, next_tail),
                None => next_tail,
            };
            self.buffers.resolve_mut(buffer)?.tail = tail;
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
    fn write_bytes(&mut self, b: &StreamBuffer, data: &[u8], head: usize) -> usize {
        // The two copies the doc above describes, and the two the C makes.
        // Taken only when the payload fits the ring once -- which is the only
        // shape that reaches here, because a send larger than the buffer is
        // refused before this -- so a payload that would wrap more than once
        // still walks the original loop and behaves exactly as it did.
        if data.len() <= b.length {
            let upto = b.length.wrapping_sub(head);
            let first = data.len().min(upto);
            let from = b.base.wrapping_add(head);
            if let (Some(dst), Some(src)) = (
                self.bytes.get_mut(from..from.wrapping_add(first)),
                data.get(..first),
            ) {
                dst.copy_from_slice(src);
            }
            let rest = data.len().wrapping_sub(first);
            if rest > 0 {
                if let (Some(dst), Some(src)) = (
                    self.bytes.get_mut(b.base..b.base.wrapping_add(rest)),
                    data.get(first..),
                ) {
                    dst.copy_from_slice(src);
                }
                return rest;
            }
            let next = head.wrapping_add(first);
            return if next >= b.length { 0 } else { next };
        }

        let mut head = head;
        for byte in data {
            if let Some(slot) = self.bytes.get_mut(b.base.wrapping_add(head)) {
                *slot = *byte;
            }
            head = head.wrapping_add(1);
            if head >= b.length {
                head = 0;
            }
        }
        head
    }

    /// `prvReadBytesFromBuffer`.
    ///
    /// Bounded as [`Kernel::write_bytes`] is, and for the same reasons.
    fn read_bytes(&mut self, b: &StreamBuffer, out: &mut [u8], tail: usize) -> usize {
        // The mirror of `write_bytes`: two copies rather than a byte at a
        // time, taken only when the request fits the ring once.
        if out.len() <= b.length {
            let upto = b.length.wrapping_sub(tail);
            let first = out.len().min(upto);
            let from = b.base.wrapping_add(tail);
            if let (Some(dst), Some(src)) = (
                out.get_mut(..first),
                self.bytes.get(from..from.wrapping_add(first)),
            ) {
                dst.copy_from_slice(src);
            }
            let rest = out.len().wrapping_sub(first);
            if rest > 0 {
                if let (Some(dst), Some(src)) = (
                    out.get_mut(first..),
                    self.bytes.get(b.base..b.base.wrapping_add(rest)),
                ) {
                    dst.copy_from_slice(src);
                }
                return rest;
            }
            let next = tail.wrapping_add(first);
            return if next >= b.length { 0 } else { next };
        }

        let mut tail = tail;
        for slot in out.iter_mut() {
            *slot = self
                .bytes
                .get(b.base.wrapping_add(tail))
                .copied()
                .unwrap_or(0);
            tail = tail.wrapping_add(1);
            if tail >= b.length {
                tail = 0;
            }
        }
        tail
    }

    /// The message length, written little-endian across
    /// [`Kernel::MESSAGE_LENGTH_BYTES`] bytes of the ring.
    fn write_length_prefix(&mut self, b: &StreamBuffer, length: usize, head: usize) -> usize {
        let bytes = (length as u64).to_le_bytes();
        // The prefix is bytes in the ring like any other, and `write_bytes`
        // already wraps in two copies -- this loop was a third hand-rolled
        // version of the same walk.
        let prefix = bytes.get(..Self::MESSAGE_LENGTH_BYTES).unwrap_or(&bytes);
        self.write_bytes(b, prefix, head)
    }

    /// The message length back out, and where the message starts.
    /// Bounded as [`Kernel::read_bytes`] is, and converted for the same
    /// reasons: `tail` is below `length`, `base + length` is at most
    /// `BYTES`, `first` is `min(want, upto)`, and every index passes through
    /// a `get` that refuses anything the ring does not own.
    fn read_length_prefix(&self, b: &StreamBuffer, tail: usize) -> (usize, usize) {
        let mut raw = [0_u8; 8];
        let want = Self::MESSAGE_LENGTH_BYTES;

        // Two reads rather than a byte at a time, the way the write side now
        // writes it. `read_bytes` would do this, but it needs `&mut self` and
        // this does not have it.
        if want <= b.length {
            let upto = b.length.wrapping_sub(tail);
            let first = want.min(upto);
            let from = b.base.wrapping_add(tail);
            if let (Some(dst), Some(src)) = (
                raw.get_mut(..first),
                self.bytes.get(from..from.wrapping_add(first)),
            ) {
                dst.copy_from_slice(src);
            }
            let rest = want.wrapping_sub(first);
            if rest > 0 {
                if let (Some(dst), Some(src)) = (
                    raw.get_mut(first..want),
                    self.bytes.get(b.base..b.base.wrapping_add(rest)),
                ) {
                    dst.copy_from_slice(src);
                }
                return (u64::from_le_bytes(raw) as usize, rest);
            }
            let next = tail.wrapping_add(first);
            let next = if next >= b.length { 0 } else { next };
            return (u64::from_le_bytes(raw) as usize, next);
        }

        let mut tail = tail;
        for i in 0..want {
            let byte = self
                .bytes
                .get(b.base.wrapping_add(tail))
                .copied()
                .unwrap_or(0);
            if let Some(slot) = raw.get_mut(i) {
                *slot = byte;
            }
            tail = tail.wrapping_add(1);
            if tail >= b.length {
                tail = 0;
            }
        }
        (u64::from_le_bytes(raw) as usize, tail)
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
        { crate::items_for(4, 0) },
        { crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 0) },
        1,
        1,
        2,
        64,
        0,
        0,
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
}

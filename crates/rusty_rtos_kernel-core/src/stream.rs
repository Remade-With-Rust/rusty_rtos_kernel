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
    /// out. Saturating spellings keep that shape without an overflow check
    /// the C does not have.
    pub(crate) const fn bytes_in_buffer(&self) -> usize {
        let mut count = self
            .length
            .saturating_add(self.head)
            .saturating_sub(self.tail);
        if count >= self.length {
            count = count.saturating_sub(self.length);
        }
        count
    }

    /// `xStreamBufferSpacesAvailable`: one less than the gap, because the
    /// ring keeps a spare byte so full and empty do not look alike.
    pub(crate) const fn spaces_available(&self) -> usize {
        let mut space = self
            .length
            .saturating_add(self.tail)
            .saturating_sub(self.head)
            .saturating_sub(1);
        if space >= self.length {
            space = space.saturating_sub(self.length);
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
> Kernel<C, P, T, H, TASKS, ITEMS, LISTS, QUEUES, SLOTS, BUFFERS, BYTES>
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
        if trigger >= length {
            return Ok(false);
        }
        let trigger = if trigger == 0 { 1 } else { trigger };
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
            let bytes = self.buffers.resolve(buffer)?.bytes_in_buffer();
            if self.buffers.resolve(buffer)?.meets_trigger(bytes) {
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
            let bytes = self.buffers.resolve(buffer)?.bytes_in_buffer();
            if self.buffers.resolve(buffer)?.meets_trigger(bytes) {
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
    fn send_completed(&mut self, buffer: StreamBufferHandle) -> Result<()> {
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

    /// `sbSEND_COMPLETED_FROM_ISR`.
    fn send_completed_from_isr(&mut self, buffer: StreamBufferHandle) -> Result<Woken> {
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
    fn write_bytes(&mut self, b: &StreamBuffer, data: &[u8], head: usize) -> usize {
        let mut head = head;
        for byte in data {
            if let Some(slot) = self.bytes.get_mut(b.base.saturating_add(head)) {
                *slot = *byte;
            }
            head = head.saturating_add(1);
            if head >= b.length {
                head = 0;
            }
        }
        head
    }

    /// `prvReadBytesFromBuffer`.
    fn read_bytes(&mut self, b: &StreamBuffer, out: &mut [u8], tail: usize) -> usize {
        let mut tail = tail;
        for slot in out.iter_mut() {
            *slot = self
                .bytes
                .get(b.base.saturating_add(tail))
                .copied()
                .unwrap_or(0);
            tail = tail.saturating_add(1);
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
        let mut head = head;
        for i in 0..Self::MESSAGE_LENGTH_BYTES {
            let byte = bytes.get(i).copied().unwrap_or(0);
            if let Some(slot) = self.bytes.get_mut(b.base.saturating_add(head)) {
                *slot = byte;
            }
            head = head.saturating_add(1);
            if head >= b.length {
                head = 0;
            }
        }
        head
    }

    /// The message length back out, and where the message starts.
    fn read_length_prefix(&self, b: &StreamBuffer, tail: usize) -> (usize, usize) {
        let mut raw = [0_u8; 8];
        let mut tail = tail;
        for i in 0..Self::MESSAGE_LENGTH_BYTES {
            let byte = self
                .bytes
                .get(b.base.saturating_add(tail))
                .copied()
                .unwrap_or(0);
            if let Some(slot) = raw.get_mut(i) {
                *slot = byte;
            }
            tail = tail.saturating_add(1);
            if tail >= b.length {
                tail = 0;
            }
        }
        (u64::from_le_bytes(raw) as usize, tail)
    }
}

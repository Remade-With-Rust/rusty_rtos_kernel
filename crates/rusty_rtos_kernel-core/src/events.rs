//! Event groups: `event_groups.c`.
//!
//! An event group is a word of bits and a list of the tasks waiting on some
//! of them. It is the one FreeRTOS primitive with no queue underneath — a
//! waiter's *condition* lives in its own event list item, so setting a bit
//! is one walk of that list asking each waiter whether it is satisfied.
//!
//! # The item value is the condition
//!
//! `vTaskPlaceOnUnorderedEventList` writes the bits a task is waiting for
//! into `xEventListItem`, where a priority-ordered event list would have
//! kept `configMAX_PRIORITIES - uxPriority`. The top byte carries three
//! flags with it — clear-on-exit, wait-for-all, and the one the unblocker
//! sets to say *why* the task woke — which is why the usable bits are the
//! bottom 24 and every comparison masks first.
//!
//! A task that times out is taken off the event list by the tick, so its
//! item value keeps the flags it blocked with and *not* the
//! unblocked-due-to-bit-set one. That difference is the whole of the C's
//! timeout test, and it is why this module needs no timeout flag of its own.

use rusty_rtos_core::config::Config;
use rusty_rtos_core::error::{Error, Result};
use rusty_rtos_core::handle::{EventGroupHandle, TaskHandle};
use rusty_rtos_core::hooks::TickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::list::ListId;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::trace::{Event, Trace};

use crate::kernel::{Kernel, OwedTrace};
use crate::lists_for;
use crate::queue::Wait::{self, Ready};

/// `EventGroup_t`: the bits, and nothing else — the list of waiting tasks
/// is one of the kernel's, named by the group's arena index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventGroup {
    /// `uxEventBits`.
    pub(crate) bits: u32,
}

/// `eventCLEAR_EVENTS_ON_EXIT_BIT`.
const CLEAR_EVENTS_ON_EXIT: u64 = 0x0100_0000;
/// `eventUNBLOCKED_DUE_TO_BIT_SET`.
const UNBLOCKED_DUE_TO_BIT_SET: u64 = 0x0200_0000;
/// `eventWAIT_FOR_ALL_BITS`.
const WAIT_FOR_ALL_BITS: u64 = 0x0400_0000;
/// `eventEVENT_BITS_CONTROL_BYTES`: the top byte of a 32-bit `EventBits_t`,
/// which the three flags and `taskEVENT_LIST_ITEM_VALUE_IN_USE` share.
pub(crate) const EVENT_BITS_CONTROL_BYTES: u64 = 0xff00_0000;

/// The `function` a `TickHook::pended` sees for `xEventGroupSetBitsFromISR`.
pub const PENDED_SET_BITS: u16 = u16::MAX;
/// The `function` a `TickHook::pended` sees for
/// `xEventGroupClearBitsFromISR`.
pub const PENDED_CLEAR_BITS: u16 = u16::MAX - 1;

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
    /// `xTasksWaitingForBits` of one group.
    pub(crate) fn event_group_list(group: EventGroupHandle) -> ListId {
        let base = u8::try_from(lists_for(C::MAX_PRIORITIES, QUEUES, 0)).unwrap_or(u8::MAX);
        base.saturating_add(u8::try_from(group.index()).unwrap_or(u8::MAX))
    }

    /// `prvTestWaitCondition`.
    const fn wait_condition_met(current: u32, wait_for: u32, all: bool) -> bool {
        if all {
            current & wait_for == wait_for
        } else {
            current & wait_for != 0
        }
    }

    /// `xEventGroupCreate`.
    ///
    /// # Errors
    /// [`Error::Full`] when the arena is full, which is `NULL` in the C.
    pub fn event_group_create(&mut self) -> Result<EventGroupHandle> {
        self.account_for_allocation();
        let group = self
            .groups
            .try_insert(EventGroup::default())
            .map_err(|_| Error::Full)?;
        // `vListInitialise( &( pxEventBits->xTasksWaitingForBits ) )`. The
        // arena hands slots back out, so a group's list is whatever the
        // last group at that index left behind.
        let list = Self::event_group_list(group);
        while let Ok(Some(item)) = self.lists.head(list) {
            let _ = self.lists.remove(item);
        }
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace.event(tick, Event::EventGroupCreate { group });
        Ok(group)
    }

    /// `vEventGroupDelete`.
    ///
    /// Every waiter is unblocked with a value of zero and the bit-set flag,
    /// so each wakes, finds its condition unmet, and reports what the C
    /// reports: a timeout.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn event_group_delete(&mut self, group: EventGroupHandle) -> Result<()> {
        self.groups.resolve(group)?;
        self.suspend_all();
        let list = Self::event_group_list(group);
        while let Ok(Some(item)) = self.lists.head(list) {
            self.remove_from_unordered_event_list(item, UNBLOCKED_DUE_TO_BIT_SET)?;
        }
        let _ = self.groups.remove(group);
        let _ = self.resume_all();
        // `vPortFree( pxEventBits )`, outside the resume — and every
        // `heap_N.c` wraps free the same way it wraps malloc, so it costs
        // its own outermost exit.
        self.account_for_allocation();
        Ok(())
    }

    /// `xEventGroupGetBits`, which the C defines as
    /// `xEventGroupClearBits( xEventGroup, 0 )` — so it costs the same
    /// critical section a clear does.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn event_group_bits(&mut self, group: EventGroupHandle) -> Result<u32> {
        self.event_group_clear_bits(group, 0)
    }

    /// `xEventGroupGetBitsFromISR`. The Posix port's interrupt mask costs
    /// nothing, so neither does this.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn event_group_bits_from_isr(&self, group: EventGroupHandle) -> Result<u32> {
        Ok(self.groups.resolve(group)?.bits)
    }

    /// `xEventGroupClearBits`: the bits as they were, then cleared.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn event_group_clear_bits(&mut self, group: EventGroupHandle, bits: u32) -> Result<u32> {
        self.enter_critical();
        // `traceEVENT_GROUP_CLEAR_BITS` is not one of the harness's hooks.
        let result = self.groups.resolve_mut(group).map(|g| {
            let before = g.bits;
            g.bits &= !bits;
            before
        });
        self.exit_critical();
        result
    }

    /// `xEventGroupSetBits`: set them, then wake every waiter the new value
    /// satisfies and clear whatever those waiters asked to have cleared.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn event_group_set_bits(&mut self, group: EventGroupHandle, bits: u32) -> Result<u32> {
        self.groups.resolve(group)?;
        let list = Self::event_group_list(group);
        self.suspend_all();
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace
            .event(tick, Event::EventGroupSetBits { group, bits });
        if let Ok(g) = self.groups.resolve_mut(group) {
            g.bits |= bits;
        }
        let mut to_clear: u32 = 0;
        let mut item = self.lists.head(list)?;
        while let Some(this) = item {
            // The next one is read before this one is unblocked, because
            // unblocking takes it off the list this walk is standing in.
            let next = self.lists.next(this)?;
            let value = self.lists.value(this)?;
            let control = value & EVENT_BITS_CONTROL_BYTES;
            let waited_for = (value & !EVENT_BITS_CONTROL_BYTES) as u32;
            let current = self.groups.resolve(group)?.bits;
            if Self::wait_condition_met(current, waited_for, control & WAIT_FOR_ALL_BITS != 0) {
                if control & CLEAR_EVENTS_ON_EXIT != 0 {
                    to_clear |= waited_for;
                }
                self.remove_from_unordered_event_list(
                    this,
                    u64::from(current) | UNBLOCKED_DUE_TO_BIT_SET,
                )?;
            }
            item = next;
        }
        let result = match self.groups.resolve_mut(group) {
            Ok(g) => {
                g.bits &= !to_clear;
                g.bits
            }
            Err(_) => 0,
        };
        let _ = self.resume_all();
        Ok(result)
    }

    /// `xEventGroupSetBitsFromISR`.
    ///
    /// An interrupt cannot walk the waiting list, so the C hands the job to
    /// the daemon task through `xTimerPendFunctionCallFromISR` and answers
    /// whether the daemon took it, not what the bits became.
    ///
    /// # Errors
    /// As the queue send behind `xTimerPendFunctionCallFromISR`.
    pub fn event_group_set_bits_from_isr(
        &mut self,
        group: EventGroupHandle,
        bits: u32,
    ) -> Result<(bool, Woken)> {
        self.pend_group_call(PENDED_SET_BITS, group, bits)
    }

    /// `xEventGroupClearBitsFromISR`, deferred the same way.
    ///
    /// # Errors
    /// As the queue send behind `xTimerPendFunctionCallFromISR`.
    pub fn event_group_clear_bits_from_isr(
        &mut self,
        group: EventGroupHandle,
        bits: u32,
    ) -> Result<(bool, Woken)> {
        self.pend_group_call(PENDED_CLEAR_BITS, group, bits)
    }

    /// The group's handle is `pvParameter1` and the bits are
    /// `ulParameter2`, exactly as the C passes them.
    fn pend_group_call(
        &mut self,
        function: u16,
        group: EventGroupHandle,
        bits: u32,
    ) -> Result<(bool, Woken)> {
        self.timer_pend_function_call_from_isr(function, u64::from(group.index()), u64::from(bits))
    }

    /// The daemon task's half of those two: `vEventGroupSetBitsCallback`
    /// and `vEventGroupClearBitsCallback`.
    ///
    /// A [`TickHook::pended`] implementation routes [`PENDED_SET_BITS`] and
    /// [`PENDED_CLEAR_BITS`] here; every other `function` is the
    /// application's own.
    ///
    /// # Errors
    /// [`Error::Gone`] when the group has been deleted since.
    pub fn event_group_pended_call(
        &mut self,
        function: u16,
        param1: u64,
        param2: u64,
    ) -> Result<()> {
        let Some(group) = self.groups.handle_at(param1 as u16) else {
            return Err(Error::Gone);
        };
        let bits = param2 as u32;
        match function {
            PENDED_SET_BITS => {
                self.event_group_set_bits(group, bits)?;
            }
            PENDED_CLEAR_BITS => {
                self.event_group_clear_bits(group, bits)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// `xEventGroupWaitBits`.
    ///
    /// The C blocks in the middle of this function and finishes it on the
    /// far side of the switch. A kernel with no stacks cannot, so a call
    /// that has to block answers [`Wait::Blocked`] and is called again; the
    /// second call *is* the far side, and starts at
    /// `uxTaskResetEventItemValue`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn event_group_wait_bits(
        &mut self,
        group: EventGroupHandle,
        wait_for: u32,
        clear_on_exit: bool,
        wait_for_all: bool,
        ticks: u64,
    ) -> Result<Wait<u32>> {
        let caller = self.current();
        if self.take_event_resume(caller) {
            let value = self.reset_event_item_value(caller)?;
            let bits = self.finish_wait(group, value, wait_for, clear_on_exit, wait_for_all)?;
            self.trace_wait_bits_end(
                caller,
                group,
                wait_for,
                value & UNBLOCKED_DUE_TO_BIT_SET == 0,
            );
            return Ok(Ready(bits));
        }

        self.groups.resolve(group)?;
        self.suspend_all();
        let current = self.groups.resolve(group)?.bits;
        let mut blocked = false;
        let mut timed_out = false;
        let mut result = current;
        if Self::wait_condition_met(current, wait_for, wait_for_all) {
            if clear_on_exit {
                if let Ok(g) = self.groups.resolve_mut(group) {
                    g.bits &= !wait_for;
                }
            }
        } else if ticks == 0 {
            timed_out = true;
        } else {
            let mut control = 0;
            if clear_on_exit {
                control |= CLEAR_EVENTS_ON_EXIT;
            }
            if wait_for_all {
                control |= WAIT_FOR_ALL_BITS;
            }
            self.place_on_unordered_event_list(
                Self::event_group_list(group),
                u64::from(wait_for) | control,
                ticks,
            )?;
            result = 0;
            let tick = self.tick;
            self.trace.note_exits(self.port.exits());
            self.trace.event(
                tick,
                Event::EventGroupWaitBitsBlock {
                    group,
                    bits: wait_for,
                },
            );
            blocked = true;
        }
        let already_yielded = self.resume_all();
        if blocked {
            if !already_yielded {
                self.yield_or_owe(caller);
            }
            self.set_event_resume(caller);
            return Ok(Wait::Blocked);
        }
        self.trace_wait_bits_end(caller, group, wait_for, timed_out);
        Ok(Ready(result))
    }

    /// `xEventGroupSync`: set some bits, then wait for a set that usually
    /// includes them — the rendezvous every task in a group must reach.
    ///
    /// It has its own two trace points, `traceEVENT_GROUP_SYNC_BLOCK` and
    /// `traceEVENT_GROUP_SYNC_END`, which the Kairos harness does not hook.
    /// The `xEventGroupSetBits` inside it traces as itself, and that is the
    /// only line a sync puts in the trace.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn event_group_sync(
        &mut self,
        group: EventGroupHandle,
        set: u32,
        wait_for: u32,
        ticks: u64,
    ) -> Result<Wait<u32>> {
        let caller = self.current();
        if self.take_event_resume(caller) {
            let value = self.reset_event_item_value(caller)?;
            return Ok(Ready(self.finish_sync(group, value, wait_for)?));
        }

        self.groups.resolve(group)?;
        self.suspend_all();
        let original = self.groups.resolve(group)?.bits;
        self.event_group_set_bits(group, set)?;
        let mut blocked = false;
        let result;
        if (original | set) & wait_for == wait_for {
            result = original | set;
            if let Ok(g) = self.groups.resolve_mut(group) {
                g.bits &= !wait_for;
            }
        } else if ticks == 0 {
            result = self.groups.resolve(group)?.bits;
        } else {
            self.place_on_unordered_event_list(
                Self::event_group_list(group),
                u64::from(wait_for) | CLEAR_EVENTS_ON_EXIT | WAIT_FOR_ALL_BITS,
                ticks,
            )?;
            result = 0;
            blocked = true;
        }
        let already_yielded = self.resume_all();
        if blocked {
            if !already_yielded {
                self.yield_or_owe(caller);
            }
            self.set_event_resume(caller);
            return Ok(Wait::Blocked);
        }
        Ok(Ready(result))
    }

    /// The tail of `xEventGroupWaitBits` past the switch: a task woken by
    /// the unblocker already has its answer in the item value, and one
    /// woken by the tick has to look at the group again.
    fn finish_wait(
        &mut self,
        group: EventGroupHandle,
        value: u64,
        wait_for: u32,
        clear_on_exit: bool,
        wait_for_all: bool,
    ) -> Result<u32> {
        if value & UNBLOCKED_DUE_TO_BIT_SET != 0 {
            return Ok((value & !EVENT_BITS_CONTROL_BYTES) as u32);
        }
        self.enter_critical();
        let bits = self.groups.resolve(group).map(|g| g.bits);
        if let Ok(bits) = bits {
            if Self::wait_condition_met(bits, wait_for, wait_for_all) && clear_on_exit {
                if let Ok(g) = self.groups.resolve_mut(group) {
                    g.bits &= !wait_for;
                }
            }
        }
        self.exit_critical();
        Ok((u64::from(bits?) & !EVENT_BITS_CONTROL_BYTES) as u32)
    }

    /// The same tail for `xEventGroupSync`, whose re-test is always
    /// wait-for-all and whose clear is not conditional on a flag.
    fn finish_sync(&mut self, group: EventGroupHandle, value: u64, wait_for: u32) -> Result<u32> {
        if value & UNBLOCKED_DUE_TO_BIT_SET != 0 {
            return Ok((value & !EVENT_BITS_CONTROL_BYTES) as u32);
        }
        self.enter_critical();
        let bits = self.groups.resolve(group).map(|g| g.bits);
        if let Ok(bits) = bits {
            if bits & wait_for == wait_for {
                if let Ok(g) = self.groups.resolve_mut(group) {
                    g.bits &= !wait_for;
                }
            }
        }
        self.exit_critical();
        Ok((u64::from(bits?) & !EVENT_BITS_CONTROL_BYTES) as u32)
    }

    /// `traceEVENT_GROUP_WAIT_BITS_END`, owed to `caller` when the
    /// `xTaskResumeAll` before it has already handed the CPU on.
    fn trace_wait_bits_end(
        &mut self,
        caller: TaskHandle,
        group: EventGroupHandle,
        bits: u32,
        timed_out: bool,
    ) {
        self.trace_failure_or_owe(
            caller,
            OwedTrace::EventGroupWaitBitsEnd {
                group,
                bits,
                timed_out,
            },
        );
    }
}

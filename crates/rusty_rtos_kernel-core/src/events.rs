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

// ================================================================ tests ==

/// `events.rs` had no unit test at all (`docs/HOLES.md`, H3).
///
/// `EventGroupsDemo` does cover this file against the C, so unlike
/// `timer.rs` and `stream.rs` these are not the only evidence there is.
/// What they add is ISOLATION: the scenario exercises the whole file at
/// once and a divergence points at a trace line, whereas each of these
/// names one return-value contract and fails on its own.
///
/// The two worth reading are the ones about what a call ANSWERS, because
/// both return a bit set that a caller is likely to assume is something
/// else.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use rusty_rtos_core::config::Config;
    use rusty_rtos_core::hooks::NoTickHook;

    use crate::events::{PENDED_CLEAR_BITS, PENDED_SET_BITS};
    use crate::queue::Wait;
    use crate::system::tests::{NoTrace, TestConfig, TestPort};

    /// Three tasks, one queue for the timer daemon, and two event groups.
    /// The ISR half of this API defers through the timer command queue, so
    /// a timer slot has to exist even though no timer is ever created.
    type K = crate::Kernel<
        TestConfig,
        TestPort,
        NoTrace,
        NoTickHook,
        4,
        { crate::list_slots_for(4, 1, crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 2)) },
        { crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 2) },
        1,
        2,
        0,
        0,
        1,
        2,
    >;

    fn kernel() -> K {
        K::new(TestPort::default(), NoTrace).expect("the declared geometry adds up")
    }

    /// A started kernel with the daemon PARKED, so work it defers stays
    /// deferred until a test asks for it.
    fn started() -> K {
        let mut k = kernel();
        let h = k.start_scheduler().expect("start");
        k.suspend(Some(h.timer)).expect("park the daemon");
        k
    }

    /// `xEventGroupClearBits`:
    ///
    /// ```c
    /// /* The value returned is the event group value prior to the bits being
    ///  * cleared. */
    /// uxReturn = pxEventBits->uxEventBits;
    /// pxEventBits->uxEventBits &= ~uxBitsToClear;
    /// ```
    ///
    /// BEFORE, not after -- so the return value still contains the bits the
    /// call just cleared, and a caller who reads it as "what is left" has
    /// it exactly backwards.
    #[test]
    fn clear_bits_answers_the_value_from_before_the_clear() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.event_group_set_bits(g, 0b1111).expect("set");

        assert_eq!(
            k.event_group_clear_bits(g, 0b0101),
            Ok(0b1111),
            "the value BEFORE: it still has the bits being cleared in it"
        );
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b1010),
            "and this is what is actually left"
        );
    }

    /// `xEventGroupSetBits` answers the value AFTER any waiter it just
    /// unblocked has taken its bits away:
    ///
    /// ```c
    /// if( xMatchFound != pdFALSE ) {
    ///     if( ( uxControlBits & eventCLEAR_EVENTS_ON_EXIT_BIT ) != 0 ) {
    ///         uxBitsToClear |= uxBitsWaitedFor;
    ///     }
    ///     ...
    /// }
    /// pxEventBits->uxEventBits &= ~uxBitsToClear;
    /// uxReturnBits = pxEventBits->uxEventBits;
    /// ```
    ///
    /// So **a set can return a value that does not contain the bit it just
    /// set.** The waiter woke inside the call, its clear-on-exit ran, and
    /// the snapshot is taken afterwards. A caller that asserts the returned
    /// value has its own bit in it is writing a race.
    #[test]
    fn set_bits_answers_a_value_a_woken_waiter_has_already_cleared() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.create_task("w", 1).expect("a waiter");
        let h = k.start_scheduler().expect("start");
        k.suspend(Some(h.timer)).expect("park the daemon");

        // Get the waiter onto the CPU and block it with CLEAR ON EXIT.
        let mut blocked = false;
        for _ in 0..50_u32 {
            let running = k.name_of(k.current()).expect("a name");
            if running.as_str() == "w" {
                assert_eq!(
                    k.event_group_wait_bits(g, 0b0001, true, false, 20),
                    Ok(Wait::Blocked)
                );
                blocked = true;
                break;
            }
            k.switch_context();
        }
        assert!(blocked, "never got the waiter onto the CPU");

        // Set the bit it is waiting for, from another context.
        assert_eq!(
            k.event_group_set_bits(g, 0b0001),
            Ok(0b0000),
            "the bit was set, the waiter matched, its clear-on-exit ran, \
             and the snapshot came after all of that"
        );
        assert_eq!(k.event_group_bits(g), Ok(0b0000));
    }

    /// The same set with a waiter that does NOT clear on exit leaves the
    /// bit where it is -- which is the control test for the one above, and
    /// the reason the surprise is about clear-on-exit and not about
    /// waking.
    #[test]
    fn set_bits_keeps_the_bit_when_the_waiter_does_not_clear_on_exit() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.create_task("w", 1).expect("a waiter");
        let h = k.start_scheduler().expect("start");
        k.suspend(Some(h.timer)).expect("park the daemon");

        let mut blocked = false;
        for _ in 0..50_u32 {
            let running = k.name_of(k.current()).expect("a name");
            if running.as_str() == "w" {
                assert_eq!(
                    k.event_group_wait_bits(g, 0b0001, false, false, 20),
                    Ok(Wait::Blocked)
                );
                blocked = true;
                break;
            }
            k.switch_context();
        }
        assert!(blocked, "never got the waiter onto the CPU");

        assert_eq!(
            k.event_group_set_bits(g, 0b0001),
            Ok(0b0001),
            "no clear-on-exit, so the bit is still there when the set returns"
        );
    }

    /// `xWaitForAllBits`: ANY is a non-empty intersection, ALL is a
    /// superset. With the bits already present neither call blocks, so
    /// this isolates the predicate from the blocking machinery.
    #[test]
    fn wait_for_all_needs_every_bit_and_wait_for_any_needs_one() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.event_group_set_bits(g, 0b0001).expect("set one of two");

        // ANY, with one of the two present: satisfied at once.
        assert_eq!(
            k.event_group_wait_bits(g, 0b0011, false, false, 0),
            Ok(Wait::Ready(0b0001)),
            "one of the two is enough for ANY"
        );

        // ALL, with the same bits: not satisfied, and a zero block time
        // means it gives up immediately and reports what it found.
        assert_eq!(
            k.event_group_wait_bits(g, 0b0011, false, true, 0),
            Ok(Wait::Ready(0b0001)),
            "ALL is not met, and the answer is still the CURRENT bits -- \
             the return value does not say whether the wait succeeded"
        );

        k.event_group_set_bits(g, 0b0010).expect("set the other");
        assert_eq!(
            k.event_group_wait_bits(g, 0b0011, false, true, 0),
            Ok(Wait::Ready(0b0011)),
            "now ALL is met"
        );
    }

    /// Clear-on-exit clears only the bits that were WAITED FOR, not the
    /// whole group -- so an unrelated bit set by someone else survives a
    /// wait that consumed its own.
    #[test]
    fn clear_on_exit_takes_only_the_bits_that_were_waited_for() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.event_group_set_bits(g, 0b1101).expect("set");

        assert_eq!(
            k.event_group_wait_bits(g, 0b0001, true, false, 0),
            Ok(Wait::Ready(0b1101)),
            "the answer is the value at the moment the wait was satisfied"
        );
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b1100),
            "and only bit 0 was taken: the other two were nobody's business"
        );
    }

    /// A group starts with no bits set, and delete does not leave the
    /// handle usable.
    #[test]
    fn a_new_group_is_empty_and_a_deleted_one_is_gone() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        assert_eq!(k.event_group_bits(g), Ok(0));

        k.event_group_delete(g).expect("delete");
        assert!(
            k.event_group_bits(g).is_err(),
            "a stale handle is an error, not a zero"
        );
    }
    /// The ALL/ANY predicate, with a detector that can actually SEE the
    /// difference.
    ///
    /// `prvTestWaitCondition`:
    ///
    /// ```c
    /// if( xWaitForAllBits == pdFALSE ) {
    ///     if( ( uxCurrentEventBits & uxBitsToWaitFor ) != 0 ) { xWaitConditionMet = pdTRUE; }
    /// } else {
    ///     if( ( uxCurrentEventBits & uxBitsToWaitFor ) == uxBitsToWaitFor ) { xWaitConditionMet = pdTRUE; }
    /// }
    /// ```
    ///
    /// The earlier version of this test compared RETURN VALUES, and with a
    /// zero block time an unsatisfied wait answers the current bits just
    /// like a satisfied one -- so it could not tell the two apart, and
    /// `cargo mutants` proved it: `&` to `|`, `&` to `^` and `==` to `!=`
    /// all survived it.
    ///
    /// CLEAR-ON-EXIT is the detector. It runs only when the wait was
    /// satisfied, so the bits afterwards say which branch was taken.
    #[test]
    fn the_all_and_any_predicates_decide_whether_clear_on_exit_runs() {
        // ANY, one of two bits present: satisfied, so clear-on-exit runs.
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.event_group_set_bits(g, 0b0001).expect("set");
        k.event_group_wait_bits(g, 0b0011, true, false, 0)
            .expect("wait");
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b0000),
            "ANY was satisfied by one bit, so clear-on-exit took it"
        );

        // ALL, one of two bits present: NOT satisfied, so nothing is taken.
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.event_group_set_bits(g, 0b0001).expect("set");
        k.event_group_wait_bits(g, 0b0011, true, true, 0)
            .expect("wait");
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b0001),
            "ALL was NOT satisfied, so clear-on-exit did not run"
        );

        // ALL, both bits present: satisfied, and only the waited-for bits go.
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.event_group_set_bits(g, 0b1011).expect("set");
        k.event_group_wait_bits(g, 0b0011, true, true, 0)
            .expect("wait");
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b1000),
            "ALL satisfied: bits 0 and 1 taken, bit 3 was nobody's business"
        );
    }

    /// `xEventGroupSetBitsFromISR` does not set any bits.
    ///
    /// ```c
    /// xReturn = xTimerPendFunctionCallFromISR( &vEventGroupSetBitsCallback,
    ///                                          ( void * ) xEventGroup,
    ///                                          ( uint32_t ) uxBitsToSet,
    ///                                          pxHigherPriorityTaskWoken );
    /// ```
    ///
    /// The whole body is a deferral to the timer daemon, so the answer is
    /// "the callback was queued" and the group is untouched until the
    /// daemon runs it. An ISR that sets a bit and an ISR that asks for a
    /// bit to be set are different things, and this is the second.
    #[test]
    fn set_bits_from_isr_only_queues_the_work() {
        let mut k = started();
        let g = k.event_group_create().expect("a group");

        let (queued, _woken) = k.event_group_set_bits_from_isr(g, 0b0101).expect("isr");
        assert!(queued, "the callback was queued");
        assert_eq!(
            k.event_group_bits(g),
            Ok(0),
            "and NOTHING is set yet: the daemon has not run"
        );

        // The daemon's half, which is what `TickHook::pended` routes to.
        k.event_group_pended_call(PENDED_SET_BITS, u64::from(g.index()), 0b0101)
            .expect("the daemon runs the callback");
        assert_eq!(k.event_group_bits(g), Ok(0b0101));
    }

    /// `xEventGroupClearBitsFromISR` is the same deferral, with the
    /// clearing callback.
    #[test]
    fn clear_bits_from_isr_only_queues_the_work() {
        let mut k = started();
        let g = k.event_group_create().expect("a group");
        k.event_group_set_bits(g, 0b1111).expect("set");

        let (queued, _woken) = k.event_group_clear_bits_from_isr(g, 0b0101).expect("isr");
        assert!(queued, "the callback was queued");
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b1111),
            "and nothing is cleared yet"
        );

        k.event_group_pended_call(PENDED_CLEAR_BITS, u64::from(g.index()), 0b0101)
            .expect("the daemon runs the callback");
        assert_eq!(k.event_group_bits(g), Ok(0b1010));
    }

    /// `xEventGroupGetBitsFromISR` is the odd one out: it reads the bits
    /// DIRECTLY under an interrupt-safe critical section, with no
    /// deferral, so it is the only ISR call here whose answer is current.
    #[test]
    fn bits_from_isr_reads_directly_and_is_not_deferred() {
        let mut k = started();
        let g = k.event_group_create().expect("a group");
        assert_eq!(k.event_group_bits_from_isr(g), Ok(0));

        k.event_group_set_bits(g, 0b0110).expect("set from a task");
        assert_eq!(
            k.event_group_bits_from_isr(g),
            Ok(0b0110),
            "read straight through, unlike set and clear"
        );
        assert_eq!(k.event_group_bits_from_isr(g), k.event_group_bits(g));
    }

    /// A pended call naming a group that does not exist is an error, not a
    /// silent no-op: the daemon runs these long after the ISR that asked,
    /// and the group can have been deleted in between.
    #[test]
    fn a_pended_call_for_a_deleted_group_is_an_error() {
        let mut k = started();
        let g = k.event_group_create().expect("a group");
        let index = u64::from(g.index());
        k.event_group_delete(g).expect("delete");

        assert!(
            k.event_group_pended_call(PENDED_SET_BITS, index, 0b0001)
                .is_err(),
            "the slot is empty and the daemon must not write into it"
        );
    }
    /// `xEventGroupSync` when the rendezvous completes on the spot.
    ///
    /// ```c
    /// uxOriginalBitValue = pxEventBits->uxEventBits;
    /// ( void ) xEventGroupSetBits( xEventGroup, uxBitsToSet );
    /// if( ( ( uxOriginalBitValue | uxBitsToSet ) & uxBitsToWaitFor ) == uxBitsToWaitFor )
    /// {
    ///     uxReturn = ( uxOriginalBitValue | uxBitsToSet );
    ///     pxEventBits->uxEventBits &= ~uxBitsToWaitFor;
    ///     xTicksToWait = 0;
    /// }
    /// ```
    ///
    /// The answer is the value from BEFORE the clear -- it still contains
    /// the bits the call is about to take away, and it contains the ones
    /// this caller just set. Reading it as "what is left in the group" is
    /// exactly wrong, twice over.
    #[test]
    fn a_sync_that_completes_answers_the_value_from_before_it_clears() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.event_group_set_bits(g, 0b0001)
            .expect("the other half arrived");

        assert_eq!(
            k.event_group_sync(g, 0b0010, 0b0011, 0),
            Ok(Wait::Ready(0b0011)),
            "original | set, INCLUDING the bits it is about to clear"
        );
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b0000),
            "and the rendezvous bits are gone: sync always clears on exit"
        );
    }

    /// The bits are set EVEN WHEN the rendezvous does not complete, which
    /// is the whole point of a rendezvous: this caller's arrival has to be
    /// visible to the participants who have not arrived yet.
    ///
    /// The C sets them unconditionally, before it tests the condition.
    #[test]
    fn a_sync_that_does_not_complete_still_leaves_its_own_bits_set() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");

        assert_eq!(
            k.event_group_sync(g, 0b0001, 0b0011, 0),
            Ok(Wait::Ready(0b0001)),
            "not satisfied, so the answer is the CURRENT bits"
        );
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b0001),
            "and this caller's arrival is still recorded for the others"
        );
    }

    /// A sync takes only the bits it waited for, and leaves anything else
    /// in the group alone.
    #[test]
    fn a_sync_clears_only_its_own_rendezvous_bits() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.event_group_set_bits(g, 0b1000)
            .expect("somebody else's bit");

        assert_eq!(
            k.event_group_sync(g, 0b0001, 0b0001, 0),
            Ok(Wait::Ready(0b1001)),
            "the answer carries the unrelated bit too"
        );
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b1000),
            "and the unrelated bit survives the clear"
        );
    }

    /// The WHOLE round trip of a blocked wait: block, be woken by a set,
    /// and then RUN AGAIN to take the completion path.
    ///
    /// The earlier tests here stopped at the wake-up and never resumed the
    /// waiter, so `finish_wait` -- everything that decides what a resumed
    /// wait ANSWERS -- was never reached at all. `cargo mutants` found it
    /// as a dozen survivors in one function.
    #[test]
    fn a_woken_waiter_resumes_and_is_told_which_bits_satisfied_it() {
        // NOT `started()`: this creates its own task before starting, and
        // the scheduler can only be started once.
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.create_task("w", 1).expect("a waiter");
        let h = k.start_scheduler().expect("start");
        k.suspend(Some(h.timer)).expect("park the daemon");

        // Get the waiter onto the CPU and block it, clearing on exit.
        let mut blocked = false;
        for _ in 0..50_u32 {
            if k.name_of(k.current()).expect("a name").as_str() == "w" {
                assert_eq!(
                    k.event_group_wait_bits(g, 0b0011, true, true, 50),
                    Ok(Wait::Blocked),
                    "ALL of two bits, neither set yet"
                );
                blocked = true;
                break;
            }
            k.switch_context();
        }
        assert!(blocked, "never got the waiter onto the CPU");

        // One bit is not enough for an ALL wait: it must stay blocked.
        k.event_group_set_bits(g, 0b0001).expect("half of it");
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b0001),
            "one bit does not satisfy ALL, so nothing was consumed"
        );

        // The second completes it, and the waiter's clear-on-exit runs.
        k.event_group_set_bits(g, 0b0010).expect("the other half");
        assert_eq!(
            k.event_group_bits(g),
            Ok(0b0000),
            "both bits matched, so the waiter took them"
        );

        // Now RESUME it: the same call at the same pc takes the finish path.
        let mut finished = false;
        for _ in 0..50_u32 {
            if k.name_of(k.current()).expect("a name").as_str() == "w" {
                assert_eq!(
                    k.event_group_wait_bits(g, 0b0011, true, true, 50),
                    Ok(Wait::Ready(0b0011)),
                    "it is told the bits that satisfied it, not what is \
                     left in the group after its own clear"
                );
                finished = true;
                break;
            }
            k.switch_context();
        }
        assert!(finished, "the waiter never ran again");
    }

    /// A real two-task rendezvous, which is the only way to reach
    /// `finish_sync` -- the path that decides what a BLOCKED sync answers
    /// when somebody else completes it.
    ///
    /// `cargo mutants` left sixteen survivors in that one function after
    /// the three single-task sync tests, for the same reason `finish_wait`
    /// had twelve: nothing ever blocked on a sync and then ran again.
    ///
    /// The C blocks the first arrival with
    /// `uxBitsToWaitFor | eventCLEAR_EVENTS_ON_EXIT_BIT | eventWAIT_FOR_ALL_BITS`,
    /// so a rendezvous is always ALL-bits and always clears.
    #[test]
    fn two_tasks_rendezvous_and_the_one_that_waited_is_told_it_completed() {
        let mut k = kernel();
        let g = k.event_group_create().expect("a group");
        k.create_task("a", 1).expect("first arrival");
        k.create_task("b", 1).expect("second arrival");
        let h = k.start_scheduler().expect("start");
        k.suspend(Some(h.timer)).expect("park the daemon");

        const A: u32 = 0b0001;
        const B: u32 = 0b0010;
        const BOTH: u32 = A | B;

        // `a` arrives first: it sets its own bit and blocks on both.
        let mut waited = false;
        for _ in 0..50_u32 {
            if k.name_of(k.current()).expect("a name").as_str() == "a" {
                assert_eq!(
                    k.event_group_sync(g, A, BOTH, 50),
                    Ok(Wait::Blocked),
                    "only one of the two has arrived"
                );
                waited = true;
                break;
            }
            k.switch_context();
        }
        assert!(waited, "task a never reached the CPU");
        assert_eq!(
            k.event_group_bits(g),
            Ok(A),
            "and its arrival is recorded while it waits"
        );

        // `b` arrives second: the rendezvous completes inside ITS call.
        let mut completed = false;
        for _ in 0..50_u32 {
            if k.name_of(k.current()).expect("a name").as_str() == "b" {
                assert_eq!(
                    k.event_group_sync(g, B, BOTH, 50),
                    Ok(Wait::Ready(BOTH)),
                    "the second arrival sees both and does not block"
                );
                completed = true;
                break;
            }
            k.switch_context();
        }
        assert!(completed, "task b never reached the CPU");
        assert_eq!(
            k.event_group_bits(g),
            Ok(0),
            "the rendezvous bits are consumed once, not twice"
        );

        // `a` resumes and takes the completion path.
        let mut resumed = false;
        for _ in 0..50_u32 {
            if k.name_of(k.current()).expect("a name").as_str() == "a" {
                assert_eq!(
                    k.event_group_sync(g, A, BOTH, 50),
                    Ok(Wait::Ready(BOTH)),
                    "it is told the rendezvous completed, even though the                      group is empty again by the time it runs"
                );
                resumed = true;
                break;
            }
            k.switch_context();
        }
        assert!(resumed, "task a never ran again");
    }
}

//! `timers.c`: software timers and the daemon task that runs them.
//!
//! A software timer is not an interrupt and not a thread. It is an entry on
//! one of two sorted lists, and a task — the daemon, `Tmr Svc` — that
//! blocks on a queue until either a command arrives or the head of the list
//! is due. Everything a caller does to a timer is a *message*: start, stop,
//! reset, change period, delete. Nothing touches the lists but the daemon.
//!
//! That indirection is the whole design, and it is why the trace has a
//! `TIMER_COMMAND_SEND` line for every call and a `TIMER_EXPIRED` line only
//! when the daemon gets round to it.
//!
//! Two things this kernel spells differently from the C, both because it
//! has no pointers:
//!
//! * the command queue carries an *index* into a ring of commands rather
//!   than a struct, so the queue keeps the length, the blocking and the
//!   full behaviour the trace depends on while the payload lives beside it;
//! * a callback is a small number the application's hook switches on,
//!   rather than a function pointer. `pvTimerID` rides along with it.

use rusty_rtos_core::config::Config;
use rusty_rtos_core::error::{Error, Result};
use rusty_rtos_core::handle::{QueueHandle, TaskHandle, TimerHandle};
use rusty_rtos_core::hooks::TickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::list::ListId;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::trace::{Event, Trace};

use crate::kernel::{Kernel, OwedTrace};
use crate::name::Name;
use crate::queue::{Position, Ready, Wait};

/// How many `DaemonTaskMessage_t`s the ring beside the timer queue holds.
///
/// The C's queue carries the structs themselves; this one carries indices
/// into here, so the queue keeps the length, the blocking and the
/// full behaviour a trace depends on while the payload lives beside it.
/// [`Config::validate`] refuses a `TIMER_QUEUE_LENGTH` larger than this,
/// for the same reason it bounds the notification arrays.
pub const MAX_TIMER_COMMANDS: usize = 32;

/// `tmrSTATUS_IS_ACTIVE`.
pub(crate) const STATUS_ACTIVE: u8 = 0x01;
/// `tmrSTATUS_IS_AUTORELOAD`.
pub(crate) const STATUS_AUTORELOAD: u8 = 0x04;

/// The command ids, exactly as `timers.h` numbers them — the numbering is
/// load-bearing, because `xTimerGenericCommandFromTask` refuses anything at
/// or above [`Command::FIRST_FROM_ISR`] and the from-ISR half refuses
/// anything below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `tmrCOMMAND_EXECUTE_CALLBACK_FROM_ISR`.
    ExecuteCallbackFromIsr,
    /// `tmrCOMMAND_EXECUTE_CALLBACK`.
    ExecuteCallback,
    /// `tmrCOMMAND_START`.
    Start,
    /// `tmrCOMMAND_RESET`.
    Reset,
    /// `tmrCOMMAND_STOP`.
    Stop,
    /// `tmrCOMMAND_CHANGE_PERIOD`.
    ChangePeriod,
    /// `tmrCOMMAND_DELETE`.
    Delete,
    /// `tmrCOMMAND_START_FROM_ISR`.
    StartFromIsr,
    /// `tmrCOMMAND_RESET_FROM_ISR`.
    ResetFromIsr,
    /// `tmrCOMMAND_STOP_FROM_ISR`.
    StopFromIsr,
    /// `tmrCOMMAND_CHANGE_PERIOD_FROM_ISR`.
    ChangePeriodFromIsr,
}

impl Command {
    /// `tmrFIRST_FROM_ISR_COMMAND`.
    pub const FIRST_FROM_ISR: i32 = 6;

    /// The number `timers.h` gives it, which is what the trace prints.
    #[must_use]
    pub const fn id(self) -> i32 {
        match self {
            Self::ExecuteCallbackFromIsr => -2,
            Self::ExecuteCallback => -1,
            Self::Start => 1,
            Self::Reset => 2,
            Self::Stop => 3,
            Self::ChangePeriod => 4,
            Self::Delete => 5,
            Self::StartFromIsr => 6,
            Self::ResetFromIsr => 7,
            Self::StopFromIsr => 8,
            Self::ChangePeriodFromIsr => 9,
        }
    }

    const fn is_from_isr(self) -> bool {
        self.id() >= Self::FIRST_FROM_ISR
    }

    const fn is_start_or_reset(self) -> bool {
        matches!(
            self,
            Self::Start | Self::StartFromIsr | Self::Reset | Self::ResetFromIsr
        )
    }

    const fn is_stop(self) -> bool {
        matches!(self, Self::Stop | Self::StopFromIsr)
    }

    const fn is_change_period(self) -> bool {
        matches!(self, Self::ChangePeriod | Self::ChangePeriodFromIsr)
    }
}

/// One `DaemonTaskMessage_t`, kept beside the queue rather than in it.
#[derive(Debug, Clone, Copy)]
pub struct Message {
    /// Which command this is.
    pub command: Command,
    /// `xMessageValue` for a timer command; `ulParameter2` for a pended
    /// function call.
    pub(crate) value: u64,
    /// The timer the command is for, if it is a timer command.
    pub(crate) timer: TimerHandle,
    /// `pvParameter1` of a pended function call.
    pub(crate) param1: u64,
    /// Which pended function, for the hook to switch on.
    pub(crate) function: u16,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            command: Command::Stop,
            value: 0,
            timer: TimerHandle::NULL,
            param1: 0,
            function: 0,
        }
    }
}

/// One software timer: a `Timer_t` without the pointers.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timer {
    pub(crate) name: Name,
    /// `xTimerPeriodInTicks`.
    pub(crate) period: u64,
    /// `pvTimerID`.
    pub(crate) id: u64,
    /// Which callback the application's hook should run.
    pub(crate) callback: u16,
    /// `ucStatus`.
    pub(crate) status: u8,
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
    /// `pxCurrentTimerList` and `pxOverflowTimerList`, as list ids. They sit
    /// after the ready lists and the four task lists, before the queues'.
    pub(crate) fn timer_list(&self) -> ListId {
        let base = C::MAX_PRIORITIES.saturating_add(4);
        base.saturating_add(u8::from(self.timers_swapped))
    }

    pub(crate) fn overflow_timer_list(&self) -> ListId {
        let base = C::MAX_PRIORITIES.saturating_add(4);
        base.saturating_add(u8::from(!self.timers_swapped))
    }

    /// The list item a timer sorts by. Task items come first, then one per
    /// timer.
    pub(crate) fn timer_item(timer: TimerHandle) -> u16 {
        (TASKS.saturating_mul(2) as u16).saturating_add(timer.index())
    }

    // ---------------------------------------------------------- creating --

    /// `xTimerCreate`.
    ///
    /// `callback` names which of the application's timer callbacks this
    /// timer runs; `id` is `pvTimerID`.
    ///
    /// # Errors
    /// [`Error::Full`] when the timer arena is full;
    /// [`Error::InvalidArgument`] for a zero period, which the C asserts on.
    pub fn timer_create(
        &mut self,
        name: &str,
        period: u64,
        auto_reload: bool,
        id: u64,
        callback: u16,
    ) -> Result<TimerHandle> {
        if period == 0 {
            return Err(Error::InvalidArgument);
        }
        // `pvPortMalloc( sizeof( Timer_t ) )` comes first.
        self.account_for_allocation();
        let status = if auto_reload { STATUS_AUTORELOAD } else { 0 };
        let handle = self
            .timers
            .try_insert(Timer {
                // A timer keeps the *pointer* to its name in the C, so it
                // is never truncated the way a task's is. `Oneshot Timer`
                // is thirteen characters and `configMAX_TASK_NAME_LEN` is
                // twelve.
                name: Name::new(name, crate::NAME_CAPACITY),
                period,
                id,
                callback,
                status,
            })
            .map_err(|_| Error::Full)?;
        // `prvInitialiseNewTimer` opens with `prvCheckForValidListAndQueue`.
        self.check_for_valid_list_and_queue()?;
        let tick = self.tick;
        let name = self
            .timers
            .resolve(handle)
            .map(|t| t.name)
            .unwrap_or_default();
        self.trace.note_exits(self.port.exits());
        self.trace.event(
            tick,
            Event::TimerCreate {
                timer: handle,
                name: name.as_str(),
            },
        );
        Ok(handle)
    }

    /// `prvCheckForValidListAndQueue`: make the command queue if no timer
    /// has made it yet, inside one critical section either way.
    ///
    /// The C makes it lazily, on the first `xTimerCreate` — which for
    /// `TimerDemo` is *before the scheduler starts*, so the queue is the
    /// first object in the trace and not the third. Everything the create
    /// does happens inside the outer section, so the whole thing is one
    /// outermost exit whether or not it built anything.
    ///
    /// # Errors
    /// As [`Kernel::queue_create`].
    pub(crate) fn check_for_valid_list_and_queue(&mut self) -> Result<()> {
        self.enter_critical();
        let result = if self.timer_queue == QueueHandle::NULL {
            match self.queue_create(C::TIMER_QUEUE_LENGTH) {
                Ok(queue) => {
                    self.timer_queue = queue;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        } else {
            Ok(())
        };
        self.exit_critical();
        result
    }

    /// `pvTimerGetTimerID`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn timer_id(&mut self, timer: TimerHandle) -> Result<u64> {
        // The C wraps this in a critical section.
        self.enter_critical();
        let id = self.timers.resolve(timer).map(|t| t.id);
        self.exit_critical();
        id
    }

    /// `vTimerSetTimerID`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn timer_set_id(&mut self, timer: TimerHandle, id: u64) -> Result<()> {
        self.enter_critical();
        let result = self.timers.resolve_mut(timer).map(|t| {
            t.id = id;
        });
        self.exit_critical();
        result
    }

    /// `xTimerGetPeriod`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn timer_period(&self, timer: TimerHandle) -> Result<u64> {
        self.timers.resolve(timer).map(|t| t.period)
    }

    /// `xTimerIsTimerActive`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn timer_is_active(&mut self, timer: TimerHandle) -> Result<bool> {
        self.enter_critical();
        let active = self
            .timers
            .resolve(timer)
            .map(|t| (t.status & STATUS_ACTIVE) != 0);
        self.exit_critical();
        active
    }

    /// `xTimerGetExpiryTime`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn timer_expiry_time(&self, timer: TimerHandle) -> Result<u64> {
        self.timers.resolve(timer)?;
        self.lists.value(Self::timer_item(timer))
    }

    /// `xTimerGetReloadMode`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn timer_auto_reload(&mut self, timer: TimerHandle) -> Result<bool> {
        self.enter_critical();
        let auto = self
            .timers
            .resolve(timer)
            .map(|t| (t.status & STATUS_AUTORELOAD) != 0);
        self.exit_critical();
        auto
    }

    /// `vTimerSetReloadMode`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn timer_set_auto_reload(&mut self, timer: TimerHandle, auto: bool) -> Result<()> {
        self.enter_critical();
        let result = self.timers.resolve_mut(timer).map(|t| {
            if auto {
                t.status |= STATUS_AUTORELOAD;
            } else {
                t.status &= !STATUS_AUTORELOAD;
            }
        });
        self.exit_critical();
        result
    }

    // --------------------------------------------------- sending commands --

    /// `xTimerGenericCommandFromTask`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn timer_command(
        &mut self,
        timer: TimerHandle,
        command: Command,
        value: u64,
        ticks: u64,
    ) -> Result<Wait<bool>> {
        let message = Message {
            command,
            value,
            timer,
            ..Message::default()
        };
        // The C refuses a from-ISR command here and still traces the send.
        if command.is_from_isr() {
            self.trace_command_send(timer, command, value);
            return Ok(Ready(false));
        }
        // `if( xTaskGetSchedulerState() == taskSCHEDULER_RUNNING )` — before
        // the scheduler runs, a command may not block.
        let ticks = if self.is_running() { ticks } else { 0 };
        // Who to trace against. The C runs `traceTIMER_COMMAND_SEND` on the
        // line after `xQueueSendToBack`, so a send that made the daemon
        // ready switches away first and the line runs when this task has
        // the CPU back — after the `TASK_SWITCHED_IN` that returns it.
        let caller = self.current();
        // `traceTIMER_COMMAND_SEND` fires whatever the send returned — and
        // a full queue is the *point* of TimerDemo's first test, which
        // starts exactly as many timers as the queue holds and then
        // requires the next one to fail.
        match self.post_timer_message(message, ticks) {
            Ok(Wait::Blocked) => Ok(Wait::Blocked),
            Ok(Ready(ok)) => {
                self.trace_command_send_for(caller, timer, command, value);
                Ok(Ready(ok))
            }
            Err(Error::Full) => {
                self.trace_command_send_for(caller, timer, command, value);
                Ok(Ready(false))
            }
            Err(e) => Err(e),
        }
    }

    /// `xTimerStart`.
    ///
    /// # Errors
    /// As [`Kernel::timer_command`].
    pub fn timer_start(&mut self, timer: TimerHandle, ticks: u64) -> Result<Wait<bool>> {
        let now = self.tick;
        self.timer_command(timer, Command::Start, now, ticks)
    }

    /// `xTimerStop`.
    ///
    /// # Errors
    /// As [`Kernel::timer_command`].
    pub fn timer_stop(&mut self, timer: TimerHandle, ticks: u64) -> Result<Wait<bool>> {
        self.timer_command(timer, Command::Stop, 0, ticks)
    }

    /// `xTimerReset`.
    ///
    /// # Errors
    /// As [`Kernel::timer_command`].
    pub fn timer_reset(&mut self, timer: TimerHandle, ticks: u64) -> Result<Wait<bool>> {
        let now = self.tick;
        self.timer_command(timer, Command::Reset, now, ticks)
    }

    /// `xTimerChangePeriod`.
    ///
    /// A period of zero is refused. The C asserts against one, which on this
    /// harness ends the run; refusing is the same answer without ending it,
    /// and it matters because a zero-period auto-reload timer would reload
    /// for ever without the clock moving.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for a zero period; otherwise as
    /// [`Kernel::timer_command`].
    pub fn timer_change_period(
        &mut self,
        timer: TimerHandle,
        period: u64,
        ticks: u64,
    ) -> Result<Wait<bool>> {
        if period == 0 {
            return Err(Error::InvalidArgument);
        }
        self.timer_command(timer, Command::ChangePeriod, period, ticks)
    }

    /// `xTimerDelete`.
    ///
    /// # Errors
    /// As [`Kernel::timer_command`].
    pub fn timer_delete(&mut self, timer: TimerHandle, ticks: u64) -> Result<Wait<bool>> {
        self.timer_command(timer, Command::Delete, 0, ticks)
    }

    /// `xTimerStartFromISR`.
    ///
    /// # Errors
    /// As [`Kernel::timer_command_from_isr`].
    pub fn timer_start_from_isr(&mut self, timer: TimerHandle) -> Result<(bool, Woken)> {
        let now = self.tick_count_from_isr();
        self.timer_command_from_isr(timer, Command::StartFromIsr, now)
    }

    /// `xTimerStopFromISR`.
    ///
    /// # Errors
    /// As [`Kernel::timer_command_from_isr`].
    pub fn timer_stop_from_isr(&mut self, timer: TimerHandle) -> Result<(bool, Woken)> {
        self.timer_command_from_isr(timer, Command::StopFromIsr, 0)
    }

    /// `xTimerResetFromISR`.
    ///
    /// # Errors
    /// As [`Kernel::timer_command_from_isr`].
    pub fn timer_reset_from_isr(&mut self, timer: TimerHandle) -> Result<(bool, Woken)> {
        let now = self.tick_count_from_isr();
        self.timer_command_from_isr(timer, Command::ResetFromIsr, now)
    }

    /// `xTimerChangePeriodFromISR`.
    ///
    /// # Errors
    /// As [`Kernel::timer_command_from_isr`].
    pub fn timer_change_period_from_isr(
        &mut self,
        timer: TimerHandle,
        period: u64,
    ) -> Result<(bool, Woken)> {
        self.timer_command_from_isr(timer, Command::ChangePeriodFromIsr, period)
    }

    /// `xTimerGenericCommandFromISR`: the raw command, value and all.
    ///
    /// Prefer the four wrappers above. They exist because the C's public
    /// from-ISR API does not let a caller choose the value: a start or a
    /// reset always carries `xTaskGetTickCountFromISR()`. A value from
    /// somewhere else is a *command time* the daemon will believe, and a
    /// command time far enough in the future makes `prvReloadTimer` reload
    /// without ever landing in the future — a loop the C has too, and
    /// reaches the same way.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn timer_command_from_isr(
        &mut self,
        timer: TimerHandle,
        command: Command,
        value: u64,
    ) -> Result<(bool, Woken)> {
        if !command.is_from_isr() || (command.is_change_period() && value == 0) {
            self.trace_command_send(timer, command, value);
            return Ok((false, Woken::NO));
        }
        let message = Message {
            command,
            value,
            timer,
            ..Message::default()
        };
        let (ok, woken) = self.post_timer_message_from_isr(message)?;
        self.trace_command_send(timer, command, value);
        Ok((ok, woken))
    }

    /// `xTimerPendFunctionCall`: run `function` on the daemon task.
    ///
    /// # Errors
    /// As the queue send.
    pub fn timer_pend_function_call(
        &mut self,
        function: u16,
        param1: u64,
        param2: u64,
        ticks: u64,
    ) -> Result<Wait<bool>> {
        let message = Message {
            command: Command::ExecuteCallback,
            value: param2,
            timer: TimerHandle::NULL,
            param1,
            function,
        };
        // `tracePEND_FUNC_CALL` is not one of the harness's hooks, so this
        // says nothing on either side.
        self.post_timer_message(message, ticks)
    }

    /// `xTimerPendFunctionCallFromISR`.
    ///
    /// This is how `xEventGroupSetBitsFromISR` does its work: an interrupt
    /// cannot walk an event group's waiting list, so it hands the job to
    /// the daemon.
    ///
    /// # Errors
    /// As the queue send.
    pub fn timer_pend_function_call_from_isr(
        &mut self,
        function: u16,
        param1: u64,
        param2: u64,
    ) -> Result<(bool, Woken)> {
        let message = Message {
            command: Command::ExecuteCallbackFromIsr,
            value: param2,
            timer: TimerHandle::NULL,
            param1,
            function,
        };
        self.post_timer_message_from_isr(message)
    }

    fn trace_command_send(&mut self, timer: TimerHandle, command: Command, value: u64) {
        let tick = self.tick;
        // A sink that never reads a name pays for neither the resolve nor
        // the copy of the whole `Name` that goes with it.
        let name = if T::WANTS_NAMES {
            self.timers
                .resolve(timer)
                .map(|t| t.name)
                .unwrap_or_default()
        } else {
            Name::default()
        };
        self.trace.note_exits(self.port.exits());
        self.trace.event(
            tick,
            Event::TimerCommandSend {
                timer,
                name: name.as_str(),
                command: command.id(),
                value,
            },
        );
    }

    /// The same line, but owed to `caller` if the send has already handed
    /// the CPU to the daemon.
    fn trace_command_send_for(
        &mut self,
        caller: TaskHandle,
        timer: TimerHandle,
        command: Command,
        value: u64,
    ) {
        // A sink that never reads a name pays for neither the resolve nor
        // the copy of the whole `Name` that goes with it.
        let name = if T::WANTS_NAMES {
            self.timers
                .resolve(timer)
                .map(|t| t.name)
                .unwrap_or_default()
        } else {
            Name::default()
        };
        self.trace_failure_or_owe(
            caller,
            OwedTrace::TimerCommandSend {
                timer,
                name,
                command: command.id(),
                value,
            },
        );
    }

    /// Put a message in the ring and its index on the queue.
    fn post_timer_message(&mut self, message: Message, ticks: u64) -> Result<Wait<bool>> {
        let slot = self.stage_timer_message(message);
        match self.queue_send_generic(self.timer_queue, slot, ticks, Position::Back)? {
            Wait::Blocked => Ok(Wait::Blocked),
            Ready(()) => {
                self.commit_timer_message();
                Ok(Ready(true))
            }
        }
    }

    fn post_timer_message_from_isr(&mut self, message: Message) -> Result<(bool, Woken)> {
        let slot = self.stage_timer_message(message);
        match self.queue_send_from_isr(self.timer_queue, slot) {
            Ok(woken) => {
                self.commit_timer_message();
                Ok((true, woken))
            }
            Err(Error::Full) => Ok((false, Woken::NO)),
            Err(e) => Err(e),
        }
    }

    /// Write the message into the slot the next accepted send will use, and
    /// answer with that slot — the index the queue will carry.
    ///
    /// The C caller holds its `DaemonTaskMessage_t` on its own stack, so a
    /// send the queue refuses simply loses it and no other message is
    /// harmed. Here the message lives in a ring the queue carries indices
    /// into, and the queue's indices are always the run ending just before
    /// this pointer, so the slot it names is free exactly when the queue
    /// has room. When it has none the send cannot be accepted either, and
    /// writing would overwrite a message the daemon has not read yet — the
    /// oldest one, which is the next it will read.
    fn stage_timer_message(&mut self, message: Message) -> u64 {
        let slot = self.timer_message_next;
        if self.timer_queue_has_room() {
            if let Some(cell) = self.timer_messages.get_mut(slot) {
                *cell = message;
            }
        }
        slot as u64
    }

    /// The queue took the index, so the message it points at belongs to the
    /// queue until the daemon reads it.
    fn commit_timer_message(&mut self) {
        self.timer_message_next = self
            .timer_message_next
            .saturating_add(1)
            .checked_rem(C::TIMER_QUEUE_LENGTH)
            .unwrap_or(0);
    }

    /// `uxQueueSpacesAvailable( xTimerQueue ) > 0`, without its critical
    /// section — this is the kernel reading its own queue, not a task
    /// asking, so it must cost no sim time.
    fn timer_queue_has_room(&self) -> bool {
        self.queues
            .resolve(self.timer_queue)
            .is_ok_and(|q| q.waiting < q.length)
    }

    // ----------------------------------------------------- the daemon task --

    /// `prvGetNextExpireTime`: when the head of the current list is due, and
    /// whether there is a head at all.
    pub fn timer_next_expire(&self) -> (u64, bool) {
        let list = self.timer_list();
        let empty = self.lists.is_empty(list).unwrap_or(true);
        if empty {
            (0, true)
        } else {
            (self.lists.head_value(list).unwrap_or(0), false)
        }
    }

    /// Whether `pxOverflowTimerList` is empty, which is the second half of
    /// the daemon's "may I wait for ever?" test.
    #[must_use]
    pub fn overflow_timer_list_is_empty(&self) -> bool {
        self.lists
            .is_empty(self.overflow_timer_list())
            .unwrap_or(true)
    }

    /// `prvSampleTimeNow`: the tick, and whether reading it swapped the two
    /// timer lists because it wrapped.
    ///
    /// # Errors
    /// As the list operations.
    pub fn timer_sample_time_now(&mut self) -> Result<(u64, bool)> {
        let now = self.tick;
        let switched = now < self.timer_last_time;
        if switched {
            self.switch_timer_lists()?;
        }
        self.timer_last_time = now;
        Ok((now, switched))
    }

    /// `prvSwitchTimerLists`: everything left on the current list has
    /// expired, so run it, then swap.
    fn switch_timer_lists(&mut self) -> Result<()> {
        while self.lists.is_empty(self.timer_list()) == Ok(false) {
            let next = self.lists.head_value(self.timer_list()).unwrap_or(0);
            self.process_expired_timer(next, Self::MAX_DELAY)?;
        }
        self.timers_swapped = !self.timers_swapped;
        Ok(())
    }

    /// `prvProcessExpiredTimer`: take the head off, reload it if it is an
    /// auto-reload, and run its callback.
    ///
    /// # Errors
    /// As the list operations.
    pub fn process_expired_timer(&mut self, expire_at: u64, now: u64) -> Result<()> {
        let list = self.timer_list();
        let Ok(Some(item)) = self.lists.head(list) else {
            return Ok(());
        };
        // Take it off *first*, whatever it turns out to be. `prvSwitchTimerLists`
        // loops until this list is empty, so an item this cannot resolve —
        // a timer deleted while its entry was still queued — would spin
        // here for ever if the removal came after the lookup.
        let _ = self.lists.remove(item);
        let Some(timer) = self.timer_of_item(item) else {
            return Ok(());
        };
        let auto = self
            .timers
            .resolve(timer)
            .map(|t| (t.status & STATUS_AUTORELOAD) != 0)
            .unwrap_or(false);
        if auto {
            self.reload_timer(timer, expire_at, now)?;
        } else if let Ok(t) = self.timers.resolve_mut(timer) {
            t.status &= !STATUS_ACTIVE;
        }
        self.fire_timer(timer);
        Ok(())
    }

    /// `prvReloadTimer`: put it back on the list one period later, and keep
    /// firing while that period has already gone by.
    fn reload_timer(&mut self, timer: TimerHandle, expired_at: u64, now: u64) -> Result<()> {
        let mut expired_at = expired_at;
        loop {
            // A period of zero cannot reach here — `timer_create` and
            // `timer_change_period` both refuse one — but the loop only
            // terminates because the expiry advances, so it says so.
            let period = self
                .timers
                .resolve(timer)
                .map(|t| t.period)
                .unwrap_or(1)
                .max(1);
            let next = expired_at.wrapping_add(period) & Self::MAX_DELAY;
            if !self.insert_timer_in_active_list(timer, next, now, expired_at)? {
                return Ok(());
            }
            expired_at = next;
            self.fire_timer(timer);
        }
    }

    /// The callback itself, through the application's hook.
    fn fire_timer(&mut self, timer: TimerHandle) {
        let (name, callback, id) = match self.timers.resolve(timer) {
            Ok(t) => (t.name, t.callback, t.id),
            Err(_) => return,
        };
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace.event(
            tick,
            Event::TimerExpired {
                timer,
                name: name.as_str(),
            },
        );
        H::timer(self, timer, callback, id);
    }

    /// `prvInsertTimerInActiveList`: `true` when the timer is already due
    /// and the daemon should run it now rather than wait for it.
    ///
    /// # Errors
    /// As the list operations.
    pub fn insert_timer_in_active_list(
        &mut self,
        timer: TimerHandle,
        expiry: u64,
        now: u64,
        command_time: u64,
    ) -> Result<bool> {
        let item = Self::timer_item(timer);
        if self.lists.container(item)?.is_some() {
            let _ = self.lists.remove(item);
        }
        self.lists.set_value(item, expiry)?;
        let period = self.timers.resolve(timer).map(|t| t.period).unwrap_or(1);
        if expiry <= now {
            if now.wrapping_sub(command_time) & Self::MAX_DELAY >= period {
                return Ok(true);
            }
            let list = self.overflow_timer_list();
            self.lists.insert(list, item, expiry)?;
        } else {
            if now < command_time && expiry >= command_time {
                return Ok(true);
            }
            let list = self.timer_list();
            self.lists.insert(list, item, expiry)?;
        }
        Ok(false)
    }

    /// `prvProcessReceivedCommands`: drain the queue, one command per call.
    ///
    /// The C loops until the queue is empty; a kernel with no stacks does
    /// one and says whether there is more, because a callback in the middle
    /// of that loop can block and the daemon has to be able to come back.
    ///
    /// # The block time is not a convenience
    ///
    /// `ticks` is how long the daemon is willing to WAIT on the queue, and
    /// passing zero has a consequence beyond promptness: a daemon that
    /// polls is never on the queue's receive list, so a task posting a
    /// command finds no waiter to remove and `queue_send_generic` does not
    /// call `port_yield`. The daemon is higher priority and still does not
    /// run, because nothing asked for a switch.
    ///
    /// That is visible from the C. `TimerDemo.c` stops a timer and asserts
    /// on the next line that it is inactive, and its comment says it may:
    /// "this will appear to happen immediately to this task because this
    /// task is running at a priority below the timer service task". With a
    /// polling daemon the assertion fails, and nothing else in the corpus
    /// notices.
    ///
    /// So a caller acting as the daemon should pass what
    /// `prvProcessTimerOrBlockTask` computes — the time to the next expiry,
    /// or the maximum delay when no timer is active. Zero is correct only
    /// for a caller that is draining the queue rather than serving it, such
    /// as a trace-exact state machine that schedules the wait itself.
    ///
    /// # Errors
    /// As the queue receive.
    pub fn process_one_timer_command(&mut self, ticks: u64) -> Result<Wait<bool>> {
        let slot = match self.queue_receive(self.timer_queue, ticks) {
            Ok(Ready(slot)) => slot,
            Ok(Wait::Blocked) => return Ok(Wait::Blocked),
            Err(_) => return Ok(Ready(false)),
        };
        let message = self
            .timer_messages
            .get(slot as usize)
            .copied()
            .unwrap_or_default();

        if message.command.id() < 0 {
            // A pended function call: the daemon just runs it.
            H::pended(self, message.function, message.param1, message.value);
            return Ok(Ready(true));
        }

        let timer = message.timer;
        if self.timers.resolve(timer).is_err() {
            return Ok(Ready(true));
        }
        let item = Self::timer_item(timer);
        if self.lists.container(item)?.is_some() {
            let _ = self.lists.remove(item);
        }
        // `traceTIMER_COMMAND_RECEIVED` is not one of the harness's hooks.
        let (now, _switched) = self.timer_sample_time_now()?;

        if message.command.is_start_or_reset() {
            if let Ok(t) = self.timers.resolve_mut(timer) {
                t.status |= STATUS_ACTIVE;
            }
            let period = self.timers.resolve(timer).map(|t| t.period).unwrap_or(1);
            let expiry = message.value.wrapping_add(period) & Self::MAX_DELAY;
            if self.insert_timer_in_active_list(timer, expiry, now, message.value)? {
                let auto = self
                    .timers
                    .resolve(timer)
                    .map(|t| (t.status & STATUS_AUTORELOAD) != 0)
                    .unwrap_or(false);
                if auto {
                    self.reload_timer(timer, expiry, now)?;
                } else if let Ok(t) = self.timers.resolve_mut(timer) {
                    t.status &= !STATUS_ACTIVE;
                }
                self.fire_timer(timer);
            }
        } else if message.command.is_stop() {
            if let Ok(t) = self.timers.resolve_mut(timer) {
                t.status &= !STATUS_ACTIVE;
            }
        } else if message.command.is_change_period() {
            if let Ok(t) = self.timers.resolve_mut(timer) {
                t.status |= STATUS_ACTIVE;
                t.period = message.value;
            }
            let period = self.timers.resolve(timer).map(|t| t.period).unwrap_or(1);
            let expiry = now.wrapping_add(period) & Self::MAX_DELAY;
            let _ = self.insert_timer_in_active_list(timer, expiry, now, now)?;
        } else if message.command == Command::Delete {
            let _ = self.timers.remove(timer);
            // `vPortFree( pxTimer )`, which every `heap_N.c` wraps in
            // `vTaskSuspendAll` / `xTaskResumeAll` exactly as it wraps
            // malloc -- so a delete costs one outermost critical-section
            // exit, which under the sim contract is one unit of time,
            // while emitting no trace line at all (this harness defines no
            // `traceTIMER_DELETE`).
            //
            // `queue_delete` and `event_group_delete` both carry this
            // note; the timer command was the third object with a free and
            // the one that had not been told. `TaskNotify` is the scenario
            // that finds it -- it is the first in the corpus to delete a
            // timer -- and it showed up exactly as the other two did:
            // every event agreed and the exit column was one short from
            // the first delete onwards.
            self.account_for_allocation();
        }
        Ok(Ready(true))
    }

    /// Which timer a list item belongs to.
    fn timer_of_item(&self, item: u16) -> Option<TimerHandle> {
        let base = TASKS.saturating_mul(2) as u16;
        let index = item.checked_sub(base)?;
        self.timers.handle_at(index)
    }
}

// ================================================================ tests ==

/// `timer.rs` had no unit test at all, and three of its APIs are also ones
/// no conformance scenario reaches (`docs/HOLES.md`, H2 and H3).
///
/// Every test here quotes the `timers.c` line it pins, and pins **the C's
/// contract** rather than this kernel's behaviour.
///
/// The theme is that the timer API is a COMMAND QUEUE. Every mutator
/// returns "the command was queued", not "the thing happened", and the
/// queries read state the daemon has not updated yet. That is the single
/// most surprising thing about this API and no scenario in the corpus
/// isolates it, because a scenario always lets the daemon run.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use rusty_rtos_core::config::Config;
    use rusty_rtos_core::hooks::NoTickHook;

    use crate::queue::Wait;
    use crate::system::tests::{NoTrace, TestConfig, TestPort};

    /// Three tasks (idle, the timer daemon, one spare), one queue for the
    /// timer commands, and room for two timers.
    type K = crate::Kernel<
        TestConfig,
        TestPort,
        NoTrace,
        NoTickHook,
        3,
        { crate::items_for(3, 2) },
        { crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 0) },
        1,
        2,
        0,
        0,
        2,
        0,
    >;

    /// A started kernel with the daemon PARKED, so a queued command stays
    /// queued until a test asks for it to be processed.
    fn started() -> K {
        let mut k = K::new(TestPort::default(), NoTrace).expect("the declared geometry adds up");
        let h = k.start_scheduler().expect("start");
        k.suspend(Some(h.timer)).expect("park the daemon");
        k
    }

    /// `xTimerStart` queues a command; it does not start a timer.
    ///
    /// `prvProcessReceivedCommands` is where the flag is actually set:
    ///
    /// ```c
    /// case tmrCOMMAND_START:
    ///     ...
    ///     pxTimer->ucStatus |= tmrSTATUS_IS_ACTIVE;
    /// ```
    ///
    /// so `pdPASS` from `xTimerStart` means "the daemon has been told", and
    /// `xTimerIsTimerActive` keeps saying pdFALSE until the daemon runs. A
    /// caller that starts a timer and immediately asks whether it is active
    /// gets "no", correctly.
    #[test]
    fn a_start_only_queues_and_the_timer_is_not_active_until_it_is_processed() {
        let mut k = started();
        let t = k.timer_create("t", 10, false, 0, 0).expect("a timer");
        assert_eq!(k.timer_is_active(t), Ok(false), "created dormant");

        assert_eq!(
            k.timer_start(t, 0),
            Ok(Wait::Ready(true)),
            "the COMMAND was accepted"
        );
        assert_eq!(
            k.timer_is_active(t),
            Ok(false),
            "and the timer is still not running: the daemon has not run"
        );

        k.process_one_timer_command(0).expect("the daemon runs");
        assert_eq!(k.timer_is_active(t), Ok(true), "now it is running");
    }

    /// `xTimerGetPeriod`:
    ///
    /// ```c
    /// return pxTimer->xTimerPeriodInTicks;
    /// ```
    ///
    /// a plain field read -- and the field is written by the DAEMON:
    ///
    /// ```c
    /// case tmrCOMMAND_CHANGE_PERIOD:
    ///     pxTimer->xTimerPeriodInTicks = xMessage.u.xTimerParameters.xMessageValue;
    /// ```
    ///
    /// So between `xTimerChangePeriod` returning pdPASS and the daemon
    /// running, `xTimerGetPeriod` answers the OLD period. Two calls that
    /// look like a setter and its getter, and they disagree.
    #[test]
    fn the_period_is_the_old_one_until_a_change_is_processed() {
        let mut k = started();
        let t = k.timer_create("t", 10, false, 0, 0).expect("a timer");
        assert_eq!(k.timer_period(t), Ok(10));

        assert_eq!(k.timer_change_period(t, 25, 0), Ok(Wait::Ready(true)));
        assert_eq!(
            k.timer_period(t),
            Ok(10),
            "the setter returned pdPASS and the getter still says 10"
        );

        k.process_one_timer_command(0).expect("the daemon runs");
        assert_eq!(k.timer_period(t), Ok(25));
        assert_eq!(
            k.timer_is_active(t),
            Ok(true),
            "and a change period STARTS a dormant timer, which is the C's rule"
        );
    }

    /// `xTimerGetExpiryTime`:
    ///
    /// ```c
    /// xReturn = listGET_LIST_ITEM_VALUE( &( pxTimer->xTimerListItem ) );
    /// return xReturn;
    /// ```
    ///
    /// An unconditional list-item read. **Nothing checks that the timer is
    /// active**, so on a dormant one this answers whatever the item was
    /// last left holding, and the return type cannot say "not running".
    ///
    /// The only correct use is gated on `xTimerIsTimerActive`, and that is
    /// what this pins -- so that a future caller reads it here rather than
    /// discovering it from a wrong wake-up.
    #[test]
    fn expiry_time_is_an_unguarded_list_read_and_needs_is_active_first() {
        let mut k = started();
        let t = k.timer_create("t", 10, false, 0, 0).expect("a timer");

        // Dormant: the call SUCCEEDS and the answer means nothing.
        assert!(
            k.timer_expiry_time(t).is_ok(),
            "it answers happily for a timer that was never started"
        );
        assert_eq!(
            k.timer_is_active(t),
            Ok(false),
            "and this is the only call that says the answer is meaningless"
        );

        k.timer_start(t, 0).expect("queue the start");
        k.process_one_timer_command(0).expect("the daemon runs");

        assert_eq!(k.timer_is_active(t), Ok(true));
        assert_eq!(
            k.timer_expiry_time(t),
            Ok(k.tick_count().wrapping_add(10)),
            "now it is now-plus-the-period, and now it means something"
        );
    }

    /// `vTimerSetReloadMode`:
    ///
    /// ```c
    /// taskENTER_CRITICAL();
    /// if( xAutoReload != pdFALSE ) { pxTimer->ucStatus |=  tmrSTATUS_IS_AUTORELOAD; }
    /// else                         { pxTimer->ucStatus &= ~tmrSTATUS_IS_AUTORELOAD; }
    /// taskEXIT_CRITICAL();
    /// ```
    ///
    /// The odd one out: it is NOT a queued command, it writes the flag
    /// directly. So unlike every other mutator in this file it takes effect
    /// at once -- and it does not reschedule anything, so a running timer
    /// keeps the expiry it already has and the new mode applies from the
    /// next expiry on.
    #[test]
    fn set_auto_reload_is_immediate_and_reschedules_nothing() {
        let mut k = started();
        let t = k.timer_create("t", 10, false, 0, 0).expect("a one-shot");
        assert_eq!(k.timer_auto_reload(t), Ok(false));

        k.timer_start(t, 0).expect("queue the start");
        k.process_one_timer_command(0).expect("the daemon runs");
        let expiry = k.timer_expiry_time(t).expect("an expiry");

        // No daemon round trip, unlike start/stop/change-period.
        k.timer_set_auto_reload(t, true).expect("set");
        assert_eq!(
            k.timer_auto_reload(t),
            Ok(true),
            "immediate: this one is not a queued command"
        );
        assert_eq!(
            k.timer_expiry_time(t),
            Ok(expiry),
            "and it moved nothing: the pending expiry is untouched"
        );
        assert_eq!(k.timer_is_active(t), Ok(true));
    }

    /// The id is the caller's own storage and the kernel never reads it.
    /// `TimerDemo` uses it to count callbacks, which is the only reason it
    /// has any coverage at all.
    #[test]
    fn the_id_is_the_callers_and_round_trips() {
        let mut k = started();
        let t = k.timer_create("t", 10, false, 7, 0).expect("a timer");
        assert_eq!(k.timer_id(t), Ok(7), "the creation id");
        k.timer_set_id(t, 0xdead_beef).expect("set");
        assert_eq!(k.timer_id(t), Ok(0xdead_beef));

        // And it survives the daemon, because no command touches it.
        k.timer_start(t, 0).expect("queue the start");
        k.process_one_timer_command(0).expect("the daemon runs");
        assert_eq!(k.timer_id(t), Ok(0xdead_beef));
    }

    /// The command queue is finite, and a full one is reported as `pdFAIL`
    /// from a call whose name suggests it did something.
    ///
    /// `TestConfig` declares `configTIMER_QUEUE_LENGTH` of one, so the
    /// second command with no block time has nowhere to go. It is the same
    /// answer a caller gets on a busy system, where it is a race rather
    /// than an arithmetic certainty -- which is why it is worth pinning
    /// somewhere it is certain.
    #[test]
    fn a_full_command_queue_refuses_without_erroring() {
        let mut k = started();
        let t = k.timer_create("t", 10, false, 0, 0).expect("a timer");

        assert_eq!(k.timer_start(t, 0), Ok(Wait::Ready(true)), "queued");
        assert_eq!(
            k.timer_stop(t, 0),
            Ok(Wait::Ready(false)),
            "refused: the queue is full and the block time is zero"
        );
        assert_eq!(
            k.timer_is_active(t),
            Ok(false),
            "neither command has run yet"
        );

        k.process_one_timer_command(0).expect("drain the start");
        assert_eq!(k.timer_is_active(t), Ok(true), "the start, not the stop");
    }
}

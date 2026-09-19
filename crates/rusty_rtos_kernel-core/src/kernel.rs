//! The scheduler: `tasks.c` and `queue.c` as a state machine over indices.
//!
//! Every public function here is a FreeRTOS API function, and its body
//! follows the C body statement for statement — including **where the
//! critical sections open and close**, because on the sim that is where
//! time passes (`ORACLES.md`, sim contract v1, rule 3), and including where
//! the `trace*` macros fire, because the order of those lines is the gate.
//!
//! The subtlety that costs a diff if you miss it: FreeRTOS yields from
//! *inside* the critical section in `vTaskResume`, `vTaskPrioritySet`,
//! `xTaskResumeAll` and the queue calls, and from *outside* it in
//! `vTaskSuspend` and `vTaskDelay`. Since `portYIELD()` on the Posix port
//! is itself a critical section, an inside yield nests (and counts one
//! outermost exit) where an outside yield counts two. Each site below says
//! which it is.
//!
//! Where the C has a `configASSERT`, this has an [`Error`]: the C kernel
//! stops the world on a bad handle, and the whole point of the remake is
//! that we do not.

use core::marker::PhantomData;

use rusty_rtos_core::arena::Arena;
use rusty_rtos_core::config::Config;
use rusty_rtos_core::error::{Error, Result};
use rusty_rtos_core::handle::{
    EventGroup as EventGroupKind, EventGroupHandle, Queue as QueueKind, QueueHandle,
    StreamBuffer as StreamKind, Task as TaskKind, TaskHandle, Timer as TimerKind, TimerHandle,
};
use rusty_rtos_core::hooks::TickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::list::{ItemId, ListId, Lists};
use rusty_rtos_core::port::Port;
use rusty_rtos_core::priority::Priority;
use rusty_rtos_core::tick::TickWidth;
use rusty_rtos_core::trace::{Event, Trace};

use crate::events::EventGroup;
use crate::name::Name;
use crate::queue::Queue;
use crate::queue::Wait;
use crate::stream::StreamBuffer;
use crate::timer::{MAX_TIMER_COMMANDS, Message, Timer};
use crate::{OVERHEAD_LISTS, items_for, lists_for};

/// A trace line owed by a task that was switched out before it could
/// emit one. Only the queue failure paths can owe one: they are the only
/// places FreeRTOS traces *after* leaving a critical section.
/// Why `vTaskSwitchContext` could not choose a task to run.
///
/// # Law 3: no silent failures
///
/// Every variant here was, until 2026-09-11, a bare `return` — the scheduler
/// declining to switch and saying nothing. Three of them are impossible in a
/// healthy kernel and the fourth is what the C asserts on
/// (`configASSERT( uxTopPriority )`), so reaching any of them means an
/// invariant has already broken somewhere else.
///
/// They cost a multi-flash hardware hunt on the day the Xtensa port first
/// ran a stacked task: every ready list had emptied, the kernel kept
/// quietly running whoever was current, and nothing said so. A counter and
/// a reason turn that into one printed line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stall {
    /// The scheduler chose normally.
    #[default]
    None,
    /// No ready task at ANY priority. The idle task keeps priority 0
    /// non-empty, so reaching this means the idle task is gone — which is
    /// exactly what `configASSERT( uxTopPriority )` fires on.
    NoReadyTask,
    /// A ready list refused to answer at all.
    ListError,
    /// The chosen priority's list had nothing to rotate to, having just
    /// reported itself non-empty.
    EmptyRotation,
    /// The chosen list item named no live task.
    UnknownTask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwedTrace {
    /// Nothing owed.
    None,
    /// `traceQUEUE_SEND_FAILED`.
    SendFailed(QueueHandle),
    /// `traceQUEUE_RECEIVE_FAILED`.
    ReceiveFailed(QueueHandle),
    /// `traceTIMER_COMMAND_SEND`, which the C runs on the line *after*
    /// `xQueueSendToBack` — so a send that made the daemon ready traces
    /// only once this task has the CPU back.
    TimerCommandSend {
        timer: TimerHandle,
        name: Name,
        command: i32,
        value: u64,
    },
    /// `traceEVENT_GROUP_WAIT_BITS_END`, which the C runs after the
    /// `xTaskResumeAll` that ends the wait — so a resume that switched away
    /// traces once this task has the CPU back.
    EventGroupWaitBitsEnd {
        group: EventGroupHandle,
        bits: u32,
        timed_out: bool,
    },
    /// `prvAddNewTaskToReadyList`, the second half of `xTaskCreate`.
    ///
    /// A create costs three outermost exits before it reaches this — two
    /// `pvPortMalloc`s and the port's `pthread_create` section — and a tick
    /// can land on any of them. The C is a real thread, so it simply stops
    /// there and the new task is not on a ready list until the creator runs
    /// again. This is that pause, and it is the only owed item that is not
    /// purely a trace line: it carries the ready-list insertion too.
    AddNewTaskToReadyList { task: TaskHandle, priority: u8 },
}

/// `eTaskState`: what a task is doing, derived from the list it is in
/// exactly as `eTaskGetState` derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// The task `current` names.
    Running,
    /// On a ready list.
    Ready,
    /// On a delayed list, or on an event list with a timeout.
    Blocked,
    /// On the suspended list with no event pending.
    Suspended,
    /// The handle names no live task.
    Deleted,
}

/// One task control block: the C `TCB_t` minus everything that is a
/// pointer. No stack, no TLS, no `pxTopOfStack`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tcb {
    name: Name,
    /// `uxPriority`, the possibly-inherited one the scheduler sorts by.
    priority: u8,
    /// `uxBasePriority`, what the task asked for.
    base_priority: u8,
    /// `uxMutexesHeld`: how many mutexes this task holds, which is what
    /// decides whether giving one back may drop an inherited priority.
    mutexes_held: u32,
    /// The locals a blocking queue call keeps on the C stack across a
    /// block. This kernel has no stack, so they live here — see
    /// [`crate::queue`].
    wait: WaitFrame,
    /// `ulNotifiedValue[]`.
    notified: [u32; MAX_NOTIFICATION_ENTRIES],
    /// `ucNotifyState[]`.
    notify_state: [NotifyState; MAX_NOTIFICATION_ENTRIES],
    /// Whether a stream-buffer call this task made was preempted at the
    /// critical-section exit that samples the buffer, and must resume
    /// *after* that exit rather than repeat it.
    stream_resume: bool,
    /// The stack local that exit produced — `xBytesAvailable` on a receive,
    /// `xSpace` on a send — kept because the frame it belonged to is gone.
    stream_local: usize,
    /// Whether this task has already blocked inside the notification wait
    /// it is in. The C blocks inside `xTaskGenericNotifyWait`; a kernel
    /// with no stacks returns `Blocked` and is called again, so it needs to
    /// know the second call is a resumption rather than a fresh wait.
    notify_blocked: bool,
    /// The same, for the event-group wait this task is inside. Those two
    /// functions have no retry loop — the C blocks once and then reads the
    /// event item value — so the second call has to know it is the far side
    /// of the switch and not a fresh wait.
    event_blocked: bool,
}

/// How many notification slots a task has room for.
///
/// The C sizes its arrays from `configTASK_NOTIFICATION_ARRAY_ENTRIES`
/// directly. A `Config` is a type here and stable Rust cannot size an array
/// from one of its associated consts, so the arrays are the largest a
/// configuration may ask for and [`Config::validate`] refuses more. The
/// cost is a few bytes per task per unused slot; the alternative was a
/// tenth const generic on every kernel type.
pub const MAX_NOTIFICATION_ENTRIES: usize = 4;

/// `ucNotifyState[]`: what a task's notification slot is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifyState {
    /// `taskNOT_WAITING_NOTIFICATION`.
    #[default]
    NotWaiting,
    /// `taskWAITING_NOTIFICATION`.
    Waiting,
    /// `taskNOTIFICATION_RECEIVED`.
    Received,
}

/// `eNotifyAction`: what a notification does to the value already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifyAction {
    /// `eNoAction`: wake the task, leave the value alone.
    #[default]
    None,
    /// `eSetBits`.
    SetBits,
    /// `eIncrement`.
    Increment,
    /// `eSetValueWithOverwrite`.
    Overwrite,
    /// `eSetValueWithoutOverwrite`: fails if one is already pending.
    NoOverwrite,
}

/// What `xQueueGenericSend` and friends keep between passes of their
/// `for(;;)`: the queue being waited on, the ticks left, and the timeout
/// bookkeeping (`TimeOut_t`).
#[derive(Debug, Clone, Copy, Default)]
struct WaitFrame {
    /// The queue this task is part-way through a blocking call on;
    /// `TaskHandle::NULL`-equivalent when there is none.
    queue: QueueHandle,
    /// `xTicksToWait`, decremented by each `xTaskCheckForTimeOut`.
    ticks: u64,
    /// `xTimeOut.xTimeOnEntering`.
    entering: u64,
    /// `xTimeOut.xOverflowCount`.
    overflows: u64,
    /// `xEntryTimeSet`.
    entry_set: bool,
    /// `xInheritanceOccurred`.
    inherited: bool,
}

/// What [`Kernel::start_scheduler`] created, so a runner can attach bodies.
#[derive(Debug, Clone, Copy)]
pub struct StartHandles {
    /// The idle task.
    pub idle: TaskHandle,
    /// The timer service task.
    pub timer: TaskHandle,
    /// The timer command queue.
    pub timer_queue: QueueHandle,
}

/// The scheduler.
///
/// `TASKS`, `ITEMS`, `LISTS`, `QUEUES`, `SLOTS`, `BUFFERS` and `BYTES` are
/// the geometry: how many tasks, list items, lists, queues, queue item
/// slots, stream buffers, and stream-buffer bytes this kernel has room
/// for. See the crate docs and [`items_for`] / [`lists_for`].
/// [`Kernel::new`] refuses a geometry that does not add up.
pub struct Kernel<
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
> {
    pub(crate) port: P,
    pub(crate) trace: T,
    pub(crate) tcbs: Arena<TaskKind, Tcb, TASKS>,
    pub(crate) queues: Arena<QueueKind, Queue, QUEUES>,
    pub(crate) lists: Lists<ITEMS, LISTS>,
    pub(crate) slots: [u64; SLOTS],
    pub(crate) buffers: Arena<StreamKind, StreamBuffer, BUFFERS>,
    pub(crate) timers: Arena<TimerKind, Timer, TIMERS>,
    pub(crate) groups: Arena<EventGroupKind, EventGroup, GROUPS>,
    /// The ring of `DaemonTaskMessage_t`s the timer queue carries indices
    /// into. It is exactly as long as the queue, so a message can only be
    /// overwritten once the queue has already refused to hold its index.
    pub(crate) timer_messages: [Message; MAX_TIMER_COMMANDS],
    pub(crate) timer_message_next: usize,
    /// Which of the two timer lists is `pxCurrentTimerList` right now.
    pub(crate) timers_swapped: bool,
    /// `xLastTime` in `prvSampleTimeNow`.
    pub(crate) timer_last_time: u64,
    /// `xTimerQueue`.
    pub(crate) timer_queue: QueueHandle,
    pub(crate) bytes: [u8; BYTES],
    pub(crate) bytes_used: usize,
    /// Blocks of the byte arena that a deleted stream buffer gave back,
    /// as `(base, length)`, kept sorted and coalesced.
    ///
    /// This is the one allocator in the kernel, and it exists because the
    /// C's stream buffers are heap objects: `MessageBufferDemo`'s echo
    /// server creates one and deletes it again on every loop, and a bump
    /// allocator would run out in a few hundred ticks. At most `BUFFERS`
    /// buffers can be alive, so at most `BUFFERS` holes can exist between
    /// them, which is why the list is that long and cannot overflow.
    pub(crate) free_blocks: [(usize, usize); BUFFERS],
    pub(crate) free_count: usize,
    pub(crate) slots_used: usize,
    /// `pxCurrentTCB`.
    pub(crate) current: TaskHandle,
    /// `uxTopReadyPriority`.
    top_ready_priority: u8,
    /// `xTickCount`, masked to the configuration's tick width.
    pub(crate) tick: u64,
    /// `xPendedTicks`.
    pended_ticks: u64,
    /// `uxSchedulerSuspended`.
    suspended_depth: u32,
    /// `xSchedulerRunning`.
    running: bool,
    /// `xNextTaskUnblockTime`.
    next_unblock_time: u64,
    /// `xYieldPendings[0]`.
    yield_pending: bool,
    /// `uxCurrentNumberOfTasks`.
    task_count: usize,
    /// How many times the scheduler could not choose a task, and why the
    /// FIRST time. See [`Stall`].
    stalls: u32,
    first_stall: Stall,
    /// A task that deleted ITSELF and whose slot the idle task has not
    /// reclaimed yet — the C's `xTasksWaitingTermination`, which only ever
    /// holds the running task, because any other task is freed on the spot.
    ///
    /// One slot is enough, and that is a property rather than a guess: a
    /// self-deleting task is off every ready list and yields immediately, so
    /// a second self-delete cannot happen until a switch has occurred, and
    /// the switch is where this is reaped.
    awaiting_reap: Option<TaskHandle>,
    /// Which of the two delayed lists is `pxDelayedTaskList` right now.
    delayed_swapped: bool,
    /// `xNumOfOverflows`.
    overflows: u64,
    /// Which tasks have been switched in at least once. A task's first
    /// switch-in is where a real port hands it a fresh stack, so the
    /// outgoing task's open critical sections are abandoned there
    /// (`Port::reset_nesting_for_first_start`).
    started: [bool; TASKS],
    /// A task that was switched out *in the middle of a kernel call* still
    /// owes the tail of that call — in practice always the `portYIELD()`
    /// that `vTaskDelay` and `vTaskSuspend` make after their critical
    /// sections. On a real port the tail simply sits on the task's frozen
    /// stack; here the stack is a program counter, so the debt is recorded
    /// and paid by [`Kernel::resume_pending`] when the task runs again.
    owes_yield: [bool; TASKS],
    /// A trace line a task owes from a call it was switched out of.
    ///
    /// `xQueueReceive`'s failure path is `taskEXIT_CRITICAL();
    /// traceQUEUE_RECEIVE_FAILED( pxQueue ); return errQUEUE_EMPTY;` — and
    /// if the tick that the exit released switches the task away, those two
    /// lines sit on a stack that is not running. The C emits the trace when
    /// the task resumes, so this kernel does too.
    owed_trace: [OwedTrace; TASKS],
    /// How many outermost critical-section exits a task owes the clock.
    ///
    /// This is `uxSavedCriticalNesting` in the Posix port's
    /// `prvSwitchThread` and then some. A thread stops at the switch; a
    /// stackless call does not, so the abandoned frame runs on — closing
    /// the sections it had open and, on some paths, opening one more. The
    /// port tallies every exit that frame makes instead of counting it as
    /// sim time, and the tally is paid here when the task runs again. That
    /// is what puts a sim tick where the C one lands.
    owed_exits: [u32; TASKS],
    /// Does this task owe anything at all? A conservative hint: set by
    /// every writer of the three fields above, cleared only by
    /// [`Kernel::resume_pending`] once it has checked all three. Wrong in
    /// the `true` direction costs one slow path; it is never wrong the
    /// other way, because every writer raises it.
    owes_anything: [bool; TASKS],
    /// Does this task have a wait frame set?
    ///
    /// A mirror of its `wait.entry_set`, so [`Kernel::end_wait`] can
    /// answer "nothing to clear" -- which is almost every call -- without
    /// resolving the TCB to find out. Two writers, the same two that move
    /// the flag itself.
    wait_set: [bool; TASKS],
    /// The task whose abandoned frame is running right now, if any.
    unwinding: Option<TaskHandle>,
    /// `vApplicationTickHook`, held by value so it can borrow the kernel.
    pub(crate) tick_hook: H,
    /// `ucDelayAborted`: the task was pulled out of the Blocked state by
    /// [`Kernel::abort_delay`] rather than by its own block time running
    /// out, so it must not re-evaluate that block time and block again.
    delay_aborted: [bool; TASKS],
    _config: PhantomData<C>,
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
    /// `portMAX_DELAY` at this configuration's tick width.
    pub const MAX_DELAY: u64 = <C::Tick as TickWidth>::MAX;

    // ------------------------------------------------------------ setup --

    /// A kernel with no tasks, ready for [`Kernel::create_task`].
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if the configuration is invalid
    /// ([`Config::validate`]), if the name capacity cannot hold
    /// `MAX_TASK_NAME_LEN`, or if the const geometry does not match the
    /// configuration: `ITEMS` must be [`items_for`]`(TASKS)` and `LISTS`
    /// must be [`lists_for`]`(MAX_PRIORITIES, QUEUES)`.
    pub fn new(port: P, trace: T) -> Result<Self>
    where
        H: Default,
    {
        Self::with_tick_hook(port, trace, H::default())
    }

    /// [`Kernel::new`] with `vApplicationTickHook` installed.
    ///
    /// # Errors
    /// As [`Kernel::new`].
    pub fn with_tick_hook(port: P, trace: T, tick_hook: H) -> Result<Self> {
        C::validate()?;
        if ITEMS != items_for(TASKS, TIMERS)
            || LISTS != lists_for(C::MAX_PRIORITIES, QUEUES, GROUPS)
            || TASKS == 0
            || C::MAX_TASK_NAME_LEN > crate::NAME_CAPACITY
        {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            port,
            trace,
            tcbs: Arena::new(),
            queues: Arena::new(),
            lists: Lists::new(),
            slots: [0; SLOTS],
            slots_used: 0,
            buffers: Arena::new(),
            timers: Arena::new(),
            groups: Arena::new(),
            timer_messages: [Message::default(); MAX_TIMER_COMMANDS],
            timer_message_next: 0,
            timers_swapped: false,
            timer_last_time: 0,
            timer_queue: QueueHandle::NULL,
            bytes: [0; BYTES],
            bytes_used: 0,
            free_blocks: [(0, 0); BUFFERS],
            free_count: 0,
            current: TaskHandle::NULL,
            top_ready_priority: 0,
            tick: C::INITIAL_TICK_COUNT,
            pended_ticks: 0,
            suspended_depth: 0,
            running: false,
            next_unblock_time: Self::MAX_DELAY,
            yield_pending: false,
            task_count: 0,
            stalls: 0,
            first_stall: Stall::None,
            awaiting_reap: None,
            delayed_swapped: false,
            overflows: 0,
            started: [false; TASKS],
            owes_yield: [false; TASKS],
            owed_exits: [0; TASKS],
            owes_anything: [false; TASKS],
            wait_set: [false; TASKS],
            unwinding: None,
            tick_hook,
            delay_aborted: [false; TASKS],
            owed_trace: [OwedTrace::None; TASKS],
            _config: PhantomData,
        })
    }

    /// The port, for a runner that needs its counters.
    pub const fn port(&self) -> &P {
        &self.port
    }

    /// The trace sink.
    pub const fn trace(&self) -> &T {
        &self.trace
    }

    /// The trace sink, mutably.
    pub const fn trace_mut(&mut self) -> &mut T {
        &mut self.trace
    }

    /// Take the trace sink back when the run is over, so a deliverable can
    /// flush whatever it was writing to.
    pub fn into_trace(self) -> T {
        self.trace
    }

    /// `xNumOfOverflows`, for a scenario that reports counters.
    #[must_use]
    pub const fn overflows(&self) -> u64 {
        self.overflows
    }

    // ------------------------------------------------------ list indices --

    // `ListId` is a `u8` and `ItemId` a `u16` (the core's `list`): the
    // whole scheduler addresses lists and items by small integers, which is
    // what keeps a TCB free of pointers.

    pub(crate) const fn ready_list(priority: u8) -> ListId {
        priority
    }

    const fn delayed_list(&self) -> ListId {
        if self.delayed_swapped {
            C::MAX_PRIORITIES.saturating_add(1)
        } else {
            C::MAX_PRIORITIES
        }
    }

    const fn overflow_delayed_list(&self) -> ListId {
        if self.delayed_swapped {
            C::MAX_PRIORITIES
        } else {
            C::MAX_PRIORITIES.saturating_add(1)
        }
    }

    const fn pending_ready_list() -> ListId {
        C::MAX_PRIORITIES.saturating_add(2)
    }

    const fn suspended_list() -> ListId {
        C::MAX_PRIORITIES.saturating_add(3)
    }

    /// `xTasksWaitingToSend` of a queue.
    pub(crate) fn queue_send_list(queue: QueueHandle) -> ListId {
        let offset = u8::try_from(queue.index().saturating_mul(2)).unwrap_or(u8::MAX);
        C::MAX_PRIORITIES
            .saturating_add(u8::try_from(OVERHEAD_LISTS).unwrap_or(u8::MAX))
            .saturating_add(offset)
    }

    /// `xTasksWaitingToReceive` of a queue.
    pub(crate) fn queue_receive_list(queue: QueueHandle) -> ListId {
        Self::queue_send_list(queue).saturating_add(1)
    }

    /// A task's `xStateListItem`: the task's own arena index.
    pub(crate) const fn state_item(task: TaskHandle) -> ItemId {
        task.index()
    }

    /// A task's `xEventListItem`: `TASKS` above its state item, so the two
    /// never collide and either maps back to its task by arithmetic alone.
    pub(crate) fn event_item(task: TaskHandle) -> ItemId {
        Self::task_item_base().saturating_add(task.index())
    }

    fn task_item_base() -> ItemId {
        u16::try_from(TASKS).unwrap_or(u16::MAX)
    }

    fn task_of_state_item(&self, item: ItemId) -> Result<TaskHandle> {
        self.tcbs.handle_at(item).ok_or(Error::Gone)
    }

    fn task_of_event_item(&self, item: ItemId) -> Result<TaskHandle> {
        let index = item
            .checked_sub(Self::task_item_base())
            .ok_or(Error::InvalidArgument)?;
        self.tcbs.handle_at(index).ok_or(Error::Gone)
    }

    // ----------------------------------------------------------- getters --

    /// `pxCurrentTCB`.
    #[must_use]
    pub const fn current(&self) -> TaskHandle {
        self.current
    }

    /// `xTaskGetTickCount`.
    #[must_use]
    pub const fn tick_count(&self) -> u64 {
        self.tick
    }

    /// `xSchedulerRunning`.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// `uxCurrentNumberOfTasks`.
    #[must_use]
    pub const fn task_count(&self) -> usize {
        self.task_count
    }

    /// `uxSchedulerSuspended`, for a stepper reporting why a tick pended.
    #[must_use]
    pub const fn scheduler_suspended(&self) -> u32 {
        self.suspended_depth
    }

    /// `xPendedTicks`, likewise.
    #[must_use]
    pub const fn pended_ticks(&self) -> u64 {
        self.pended_ticks
    }

    /// The item ids in `priority`'s ready list, in order, into `out`.
    ///
    /// Returns how many were written. The companion to
    /// [`Kernel::ready_cursor`]: when a ready task is never chosen, the
    /// cursor says whether the rotation moved and this says what it was
    /// moving THROUGH.
    pub fn ready_items(&self, priority: u8, out: &mut [u16]) -> usize {
        let mut n = 0;
        for item in self.lists.iter(Self::ready_list(priority)) {
            let Some(slot) = out.get_mut(n) else {
                break;
            };
            *slot = item;
            n = n.wrapping_add(1);
        }
        n
    }

    /// Where the round-robin cursor sits in `priority`'s ready list.
    ///
    /// A diagnostic, paired with [`Kernel::ready_len`]: a ready task that
    /// is never chosen is either not in the list the scheduler looks at,
    /// or the cursor is not moving. These two answer both halves.
    #[must_use]
    pub fn ready_cursor(&self, priority: u8) -> u16 {
        self.lists
            .cursor_of(Self::ready_list(priority))
            .unwrap_or(0)
    }

    /// The live handle in arena slot `index`, if that slot holds a task.
    ///
    /// The generation is the point: a bare index names a SLOT, and a slot
    /// outlives the tasks that occupy it, so an index is not a task. This
    /// answers the handle, which is.
    ///
    /// It exists so a diagnostic can walk every task without knowing any
    /// of their names -- `task_get_handle` needs the name it is looking
    /// for, and a crash report is exactly the case where you do not have
    /// one.
    #[must_use]
    pub fn task_at(&self, index: usize) -> Option<TaskHandle> {
        let index = u16::try_from(index).ok()?;
        self.tcbs.handle_at(index)
    }

    /// A task's name.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale or null handle.
    pub fn name_of(&self, task: TaskHandle) -> Result<Name> {
        self.tcbs.resolve(task).map(|t| t.name)
    }

    /// `uxTaskPriorityGet`. `None` means the current task, as in C.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn priority_of(&self, task: Option<TaskHandle>) -> Result<u8> {
        let h = task.unwrap_or(self.current);
        self.tcbs.resolve(h).map(|t| t.priority)
    }

    pub(crate) fn current_priority(&self) -> u8 {
        self.priority_of(None).unwrap_or(0)
    }

    /// `uxTaskPriorityGet`: the C API, which takes a critical section.
    ///
    /// The distinction matters: on the sim a critical-section exit is where
    /// time passes, so reading a priority through the API costs a tick
    /// sixteen calls later, exactly as it does in C
    /// (`portBASE_TYPE_ENTER_CRITICAL` is `taskENTER_CRITICAL` on the Posix
    /// port). The kernel's own reads use the field directly, as C's do.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn task_priority_get(&mut self, task: Option<TaskHandle>) -> Result<u8> {
        self.enter_critical();
        let result = self.priority_of(task);
        self.exit_critical();
        result
    }

    /// `eTaskGetState`: the C API, which takes a critical section for any
    /// task but the running one.
    ///
    /// # Errors
    /// A list error surfaces as itself.
    pub fn task_state_get(&mut self, task: TaskHandle) -> Result<TaskState> {
        if task == self.current {
            return Ok(TaskState::Running);
        }
        self.enter_critical();
        let result = self.state_of(task);
        self.exit_critical();
        result
    }

    /// `eTaskGetState`, derived from the list the task is in.
    ///
    /// # Errors
    /// A list error surfaces as itself.
    pub fn state_of(&self, task: TaskHandle) -> Result<TaskState> {
        if !self.tcbs.contains(task) {
            return Ok(TaskState::Deleted);
        }
        if task == self.current {
            return Ok(TaskState::Running);
        }
        let Some(list) = self.lists.container(Self::state_item(task))? else {
            return Ok(TaskState::Deleted);
        };
        if list == Self::suspended_list() {
            if self.lists.container(Self::event_item(task))?.is_some() {
                return Ok(TaskState::Blocked);
            }
            // A task blocked indefinitely on a notification is in the
            // suspended list and on no event list at all, so the C scans
            // the notification array before it calls such a task suspended.
            // Miss this and a stream buffer's reader looks suspended to
            // `eTaskGetState` while it is plainly waiting.
            let waiting_notification = self
                .tcbs
                .resolve(task)
                .map(|t| {
                    t.notify_state
                        .iter()
                        .take(C::NOTIFICATION_ARRAY_ENTRIES)
                        .any(|s| *s == NotifyState::Waiting)
                })
                .unwrap_or(false);
            return Ok(if waiting_notification {
                TaskState::Blocked
            } else {
                TaskState::Suspended
            });
        }
        if list == self.delayed_list() || list == self.overflow_delayed_list() {
            return Ok(TaskState::Blocked);
        }
        Ok(TaskState::Ready)
    }

    /// How many tasks sit on a priority's ready list — what the idle task
    /// checks before yielding (`configIDLE_SHOULD_YIELD`).
    ///
    /// # Errors
    /// [`Error::InvalidPriority`] for a priority outside the configuration.
    pub fn ready_len(&self, priority: u8) -> Result<usize> {
        if priority >= C::MAX_PRIORITIES {
            return Err(Error::InvalidPriority);
        }
        self.lists.len(Self::ready_list(priority))
    }

    /// Record that the scheduler could not choose.
    ///
    /// Counts every occurrence and keeps the FIRST reason: the first is the
    /// diagnosis and the rest are consequences.
    fn note_stall(&mut self, why: Stall) {
        self.stalls = self.stalls.saturating_add(1);
        if self.first_stall == Stall::None {
            self.first_stall = why;
        }
    }

    /// How many times the scheduler could not choose a task.
    ///
    /// **Non-zero means an invariant broke.** A healthy kernel never stalls:
    /// the idle task is always ready. A firmware that prints this alongside
    /// its own counters turns a hang into a sentence.
    #[must_use]
    pub const fn stalls(&self) -> u32 {
        self.stalls
    }

    /// Why the scheduler first failed to choose. [`Stall::None`] if it never
    /// has.
    #[must_use]
    pub const fn first_stall(&self) -> Stall {
        self.first_stall
    }

    // ------------------------------------------------------------ tracing --

    pub(crate) fn trace_task<F>(&mut self, task: TaskHandle, make: F)
    where
        F: for<'a> FnOnce(TaskHandle, &'a str) -> Event<'a>,
    {
        // A sink that never reads the name pays for neither the lookup nor
        // the UTF-8 validation that turning one into a `&str` costs.
        let name = if T::WANTS_NAMES {
            self.tcbs.get(task).map(|t| t.name).unwrap_or_default()
        } else {
            Name::default()
        };
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace.event(
            tick,
            make(task, if T::WANTS_NAMES { name.as_str() } else { "" }),
        );
    }

    pub(crate) fn priority_value(raw: u8) -> Priority {
        Priority::new(raw, C::MAX_PRIORITIES).unwrap_or(Priority::IDLE)
    }

    // --------------------------------------------------- critical section --

    /// What `pvPortMalloc` costs in sim time on a heap-backed C kernel:
    /// `vTaskSuspendAll()` around the allocation, then `xTaskResumeAll()`,
    /// whose critical section is one outermost exit.
    ///
    /// This kernel allocates nothing, so the call is the whole of it. On a
    /// configuration with [`Config::DYNAMIC_ALLOCATION`] off it compiles
    /// away to nothing at all.
    pub(crate) fn account_for_allocation(&mut self) {
        if C::DYNAMIC_ALLOCATION {
            self.suspend_all();
            let _ = self.resume_all();
        }
    }

    /// `taskENTER_CRITICAL()`.
    pub fn enter_critical(&mut self) {
        self.port.enter_critical();
    }

    /// `taskEXIT_CRITICAL()`.
    ///
    /// On the sim this is where time passes: the port raises a tick on
    /// every sixteenth outermost exit and the kernel takes it here, which
    /// is exactly where the C kernel's `SIGALRM` handler would have run.
    pub fn exit_critical(&mut self) {
        self.port.exit_critical();
        if self.port.take_pending_tick() {
            self.tick_from_isr();
        }
    }

    /// `vPortSystemTickHandler`: the tick, from interrupt context.
    ///
    /// A silicon port calls this from its timer interrupt. The sim reaches
    /// it from [`Kernel::exit_critical`] and from the idle hook.
    pub fn tick_from_isr(&mut self) {
        self.port.set_in_tick_entry(true);
        self.port.count_tick();
        // The C handler bumps `uxCriticalNesting` by hand and drops it the
        // same way — a raw `--`, never `vPortExitCritical` — so the bump is
        // invisible to the exit count and to anything that unwinds later.
        // Modelling it as a real critical section would invent an exit at
        // every tick that caused a switch.
        if self.increment_tick() {
            self.switch_context();
        }
        self.port.set_in_tick_entry(false);
    }

    /// The idle hook's tick, `vPortKairosTick()` on the C side: a critical
    /// section around one unconditional tick. The enclosing section counts
    /// toward rule 3 exactly as the C one does.
    pub fn idle_hook_tick(&mut self) {
        self.enter_critical();
        self.tick_from_isr();
        self.exit_critical();
    }

    /// `portYIELD()` as the Posix port spells it: a critical section around
    /// the context switch, so a tick raised during it lands on the way out.
    pub(crate) fn port_yield(&mut self) {
        self.enter_critical();
        self.port.count_yield();
        if P::COMMITS_SWITCH {
            // A STACKED port. Raise its switching exception and leave
            // `current` where it is: the exception will call
            // `switch_context` itself, so the decision and the register
            // swap happen together and nothing runs in between as a task
            // the kernel has already moved on from.
            //
            // The exception cannot fire yet -- `exit_critical` below is
            // what unmasks -- so the kernel is consistent at the moment it
            // is taken.
            self.port.yield_now();
        } else {
            // A STACKLESS kernel: moving `current` IS the switch, because
            // no task owns a stack. This is the path every corpus
            // architecture takes, and it is unchanged.
            self.switch_context();
        }
        self.exit_critical();
    }

    /// `taskYIELD()` from a task body (the idle task's yield).
    pub fn task_yield(&mut self) {
        self.port_yield();
    }

    // -------------------------------------------------------- task create --

    /// `xTaskCreate`.
    ///
    /// # Errors
    /// [`Error::Full`] when the task arena is full (the C
    /// `errCOULD_NOT_ALLOCATE_REQUIRED_MEMORY`).
    pub fn create_task(&mut self, name: &str, priority: u8) -> Result<TaskHandle> {
        let caller = self.current;
        // configASSERT( uxPriority < configMAX_PRIORITIES ), then C clamps.
        let priority = priority.min(C::MAX_PRIORITIES.saturating_sub(1));
        // `prvCreateTask` takes the stack and then the TCB from the heap
        // before anything is initialised — `portSTACK_GROWTH` is negative on
        // the oracle's port, which fixes that order — and `pvPortMallocStack`
        // *is* `pvPortMalloc` with the MPU wrappers off. On heap_3 each one
        // suspends the scheduler, so a create costs two outermost exits.
        //
        // Every scenario before `death` created its tasks BEFORE
        // `vTaskStartScheduler`, and the sim port counts no exits there (the
        // C patch counts only on a FreeRTOS thread), so this cost was
        // invisible to the whole corpus. `death` is the first scenario that
        // creates a task with the scheduler running, and the first that can
        // tell the difference.
        self.account_for_allocation();
        self.account_for_allocation();
        let tcb = Tcb {
            name: Name::new(name, C::MAX_TASK_NAME_LEN),
            priority,
            base_priority: priority,
            mutexes_held: 0,
            wait: WaitFrame::default(),
            notified: [0; MAX_NOTIFICATION_ENTRIES],
            notify_state: [NotifyState::NotWaiting; MAX_NOTIFICATION_ENTRIES],
            notify_blocked: false,
            event_blocked: false,
            stream_resume: false,
            stream_local: 0,
        };
        let handle = match self.tcbs.try_insert(tcb) {
            Ok(h) => h,
            Err(_) => {
                let tick = self.tick;
                self.trace.note_exits(self.port.exits());
                self.trace.event(tick, Event::TaskCreateFailed);
                return Err(Error::Full);
            }
        };
        // The event item sorts by `configMAX_PRIORITIES - uxPriority`, so a
        // higher-priority task waits nearer the head of an event list.
        let event_value = u64::from(C::MAX_PRIORITIES.saturating_sub(priority));
        self.lists
            .set_value(Self::event_item(handle), event_value)?;
        // `prvInitialiseNewTask` ends by calling `pxPortInitialiseStack`,
        // which on the oracle's Posix port wraps `pthread_create` in a
        // critical section — one more exit, and a PORT cost rather than a
        // kernel one, which is why it has a flag of its own.
        if C::PORT_STACK_INIT_CRITICAL {
            self.enter_critical();
            self.exit_critical();
        }
        // Any of the three exits above can release a tick that switches the
        // creator out. In the C the creator is a thread and simply STOPS
        // there: `prvAddNewTaskToReadyList` has not run, so the new task is
        // on no ready list and `traceTASK_CREATE` has not fired. Running it
        // now would put both events on the wrong side of the switch — which
        // is precisely how `death` first diverged.
        if self.current != caller {
            if let Some(slot) = self.owed_trace.get_mut(usize::from(caller.index())) {
                *slot = OwedTrace::AddNewTaskToReadyList {
                    task: handle,
                    priority,
                };
            }
            self.owe(caller);
            return Ok(handle);
        }
        self.add_new_task_to_ready_list(handle, priority)?;
        Ok(handle)
    }

    /// `prvAddNewTaskToReadyList`.
    fn add_new_task_to_ready_list(&mut self, task: TaskHandle, priority: u8) -> Result<()> {
        self.enter_critical();
        {
            self.task_count = self.task_count.wrapping_add(1);
            if self.current.is_null() {
                self.current = task;
            } else if !self.running {
                // `<=`, so the last-created task of the highest priority is
                // the one that runs first.
                if self.current_priority() <= priority {
                    self.current = task;
                }
            }
            self.trace_task(task, |task, name| Event::TaskCreate {
                task,
                name,
                priority: Self::priority_value(priority),
            });
            self.add_task_to_ready_list(task)?;
        }
        self.exit_critical();
        // taskYIELD_IF_USING_PREEMPTION(), outside the section.
        if self.running && C::USE_PREEMPTION && self.current_priority() < priority {
            self.port_yield();
        }
        Ok(())
    }

    /// `prvAddTaskToReadyList`.
    pub(crate) fn add_task_to_ready_list(&mut self, task: TaskHandle) -> Result<()> {
        let priority = self.tcbs.resolve(task)?.priority;
        self.trace_task(task, |task, name| Event::MovedTaskToReadyState {
            task,
            name,
        });
        // taskRECORD_READY_PRIORITY
        if priority > self.top_ready_priority {
            self.top_ready_priority = priority;
        }
        self.lists
            .insert_end(Self::ready_list(priority), Self::state_item(task))
    }

    // ---------------------------------------------------- scheduler start --

    /// `vTaskStartScheduler`, minus the part that never returns.
    ///
    /// Creates the idle task, the timer command queue and the timer service
    /// task, fires `TASK_SWITCHED_IN` for whichever task will run first and
    /// then `STARTING_SCHEDULER`, and returns the handles so the runner can
    /// attach their bodies.
    ///
    /// # Errors
    /// As [`Kernel::create_task`].
    pub fn start_scheduler(&mut self) -> Result<StartHandles> {
        let idle = self.create_task("IDLE", 0)?;
        // `xTimerCreateTimerTask` opens with `prvCheckForValidListAndQueue`,
        // which makes the command queue only if no `xTimerCreate` has made
        // it already.
        self.check_for_valid_list_and_queue()?;
        let timer_queue = self.timer_queue;
        let timer = self.create_task("Tmr Svc", C::TIMER_TASK_PRIORITY)?;
        self.next_unblock_time = Self::MAX_DELAY;
        self.running = true;
        self.tick = C::INITIAL_TICK_COUNT;
        self.port.scheduler_started();
        // `vPortStartFirstTask` runs the first task through the same
        // first-start path as every other.
        let current = self.current;
        self.note_first_start(current);
        self.trace_task(current, |task, name| Event::TaskSwitchedIn { task, name });
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace.event(tick, Event::StartingScheduler);
        Ok(StartHandles {
            idle,
            timer,
            timer_queue,
        })
    }

    // -------------------------------------------------------- the switch --

    /// `vTaskSwitchContext`.
    pub fn switch_context(&mut self) {
        if self.suspended_depth != 0 {
            self.yield_pending = true;
            return;
        }
        self.yield_pending = false;
        let current = self.current;
        self.trace_task(current, |task, name| Event::TaskSwitchedOut { task, name });
        // taskSELECT_HIGHEST_PRIORITY_TASK
        let mut top = self.top_ready_priority;
        loop {
            match self.lists.is_empty(Self::ready_list(top)) {
                Ok(false) => break,
                Ok(true) => {
                    if top == 0 {
                        // The C `configASSERT( uxTopPriority )`. The idle
                        // task keeps priority 0 non-empty, so reaching here
                        // means it is gone. The kernel keeps the current
                        // task rather than indexing out of range -- and now
                        // SAYS so, because carrying on quietly is how this
                        // costs an afternoon on a board.
                        self.note_stall(Stall::NoReadyTask);
                        return;
                    }
                    // At least 1: the arm above returns at zero.
                    top = top.wrapping_sub(1);
                }
                Err(_) => {
                    self.note_stall(Stall::ListError);
                    return;
                }
            }
        }
        let Ok(Some(item)) = self.lists.next_round_robin(Self::ready_list(top)) else {
            self.note_stall(Stall::EmptyRotation);
            return;
        };
        let Ok(next) = self.task_of_state_item(item) else {
            self.note_stall(Stall::UnknownTask);
            return;
        };
        self.top_ready_priority = top;
        if next != current {
            self.hand_over(current, next);
        }
        self.current = next;
        self.trace_task(next, |task, name| Event::TaskSwitchedIn { task, name });
    }

    // ----------------------------------------------------------- the tick --

    /// Pay whatever the running task still owes from a call it was
    /// preempted inside. The runner calls this before stepping a body;
    /// `true` means work was done and the current task may have changed
    /// again, so nothing else should be assumed.
    pub fn resume_pending(&mut self) -> bool {
        self.settle_unwind();
        let index = usize::from(self.current.index());

        // One load and one test for the overwhelmingly common case. The
        // combined check below is the fallback, and it is what clears the hint
        // -- so a hint left standing costs one slow path and then goes away.
        if !self.owes_anything.get(index).copied().unwrap_or(true) {
            return false;
        }

        // Almost every call answers "nothing owed", and the sequence below
        // reaches that answer in three stages -- a compare, a match over
        // `OwedTrace`, then another compare -- each reachable only after the
        // one before. The same three loads, but one branch instead of three.
        if self.owed_exits.get(index).copied().unwrap_or(0) == 0
            && matches!(
                self.owed_trace.get(index).copied(),
                Some(OwedTrace::None) | None
            )
            && self.owes_yield.get(index).copied() != Some(true)
        {
            if let Some(flag) = self.owes_anything.get_mut(index) {
                *flag = false;
            }
            return false;
        }
        // First the stack unwinds — the sections the task had open when it
        // was switched out, and whatever its abandoned frame opened after
        // that — and only then does the statement after the yield run.
        // Reversing the two moves every tick.
        let owed = self.owed_exits.get(index).copied().unwrap_or(0);
        if owed > 0 {
            if let Some(slot) = self.owed_exits.get_mut(index) {
                *slot = 0;
            }
            // One counted exit each: the tally is already in outermost
            // exits, so replaying it as nesting would lose the sections the
            // tail opened and closed on its own.
            for _ in 0..owed {
                self.enter_critical();
                self.exit_critical();
                // A replayed exit can release a tick that switches this
                // task straight back out. The rest of the loop is then the
                // tail of *this* frame, which the port tallies and the next
                // resume pays — so there is nothing to unwind by hand.
            }
            return true;
        }
        // Then the line the abandoned frame had not reached yet.
        match self.owed_trace.get(index).copied() {
            Some(OwedTrace::SendFailed(queue)) => {
                if let Some(slot) = self.owed_trace.get_mut(index) {
                    *slot = OwedTrace::None;
                }
                let tick = self.tick;
                self.trace.note_exits(self.port.exits());
                self.trace
                    .event(tick, Event::QueueSendFailed { queue, name: "" });
                return true;
            }
            Some(OwedTrace::ReceiveFailed(queue)) => {
                if let Some(slot) = self.owed_trace.get_mut(index) {
                    *slot = OwedTrace::None;
                }
                let tick = self.tick;
                self.trace.note_exits(self.port.exits());
                self.trace
                    .event(tick, Event::QueueReceiveFailed { queue, name: "" });
                return true;
            }
            Some(OwedTrace::EventGroupWaitBitsEnd {
                group,
                bits,
                timed_out,
            }) => {
                if let Some(slot) = self.owed_trace.get_mut(index) {
                    *slot = OwedTrace::None;
                }
                let tick = self.tick;
                self.trace.note_exits(self.port.exits());
                self.trace.event(
                    tick,
                    Event::EventGroupWaitBitsEnd {
                        group,
                        bits,
                        timed_out,
                    },
                );
                return true;
            }
            Some(OwedTrace::TimerCommandSend {
                timer,
                name,
                command,
                value,
            }) => {
                if let Some(slot) = self.owed_trace.get_mut(index) {
                    *slot = OwedTrace::None;
                }
                let tick = self.tick;
                self.trace.note_exits(self.port.exits());
                self.trace.event(
                    tick,
                    Event::TimerCommandSend {
                        timer,
                        name: name.as_str(),
                        command,
                        value,
                    },
                );
                return true;
            }
            Some(OwedTrace::AddNewTaskToReadyList { task, priority }) => {
                if let Some(slot) = self.owed_trace.get_mut(index) {
                    *slot = OwedTrace::None;
                }
                let _ = self.add_new_task_to_ready_list(task, priority);
                return true;
            }
            Some(OwedTrace::None) | None => {}
        }
        if self.owes_yield.get(index).copied() != Some(true) {
            return false;
        }
        if let Some(flag) = self.owes_yield.get_mut(index) {
            *flag = false;
        }
        self.port_yield();
        true
    }

    /// Collect the tally of an abandoned frame that has finished running.
    ///
    /// The frame runs to its end before control comes back to the runner,
    /// so this is called once, at the top of [`Kernel::resume_pending`],
    /// which is the first thing the runner does.
    fn settle_unwind(&mut self) {
        // Nothing to settle is the common case by a wide margin, and `take()`
        // writes `None` back even then -- a store, on every call, to clear a
        // slot that was already clear. Peeking first leaves the store for the
        // calls that actually have something to clear.
        if self.unwinding.is_none() {
            return;
        }
        let Some(task) = self.unwinding.take() else {
            return;
        };
        let owed = self.port.end_unwind();
        if let Some(slot) = self.owed_exits.get_mut(usize::from(task.index())) {
            // Accumulate: a replay that is itself interrupted leaves the
            // rest of its own loop as a tail, and that tail is more of the
            // same debt.
            *slot = slot.saturating_add(owed);
        }
        if owed > 0 {
            self.owe(task);
        }
    }

    /// Emit a queue failure line now, or owe it if a tick switched the
    /// caller out of the call that was about to emit it.
    pub(crate) fn trace_failure_or_owe(&mut self, caller: TaskHandle, owed: OwedTrace) {
        if self.current == caller {
            let tick = self.tick;
            self.trace.note_exits(self.port.exits());
            match owed {
                OwedTrace::SendFailed(queue) => self
                    .trace
                    .event(tick, Event::QueueSendFailed { queue, name: "" }),
                OwedTrace::ReceiveFailed(queue) => self
                    .trace
                    .event(tick, Event::QueueReceiveFailed { queue, name: "" }),
                OwedTrace::TimerCommandSend {
                    timer,
                    name,
                    command,
                    value,
                } => self.trace.event(
                    tick,
                    Event::TimerCommandSend {
                        timer,
                        name: name.as_str(),
                        command,
                        value,
                    },
                ),
                OwedTrace::EventGroupWaitBitsEnd {
                    group,
                    bits,
                    timed_out,
                } => self.trace.event(
                    tick,
                    Event::EventGroupWaitBitsEnd {
                        group,
                        bits,
                        timed_out,
                    },
                ),
                // Not reachable through this function: `create_task` owes
                // its second half directly, because that half is not a
                // trace line — it inserts into a ready list — and it must
                // run INSIDE the section this one has already left.
                OwedTrace::AddNewTaskToReadyList { .. } | OwedTrace::None => {}
            }
            return;
        }
        if let Some(slot) = self.owed_trace.get_mut(usize::from(caller.index())) {
            *slot = owed;
        }
        self.owe(caller);
    }

    /// Raise the hint that `task` owes something.
    ///
    /// Called by every writer of `owed_trace`, `owes_yield` and `owed_exits`.
    /// Missing one would let `resume_pending` skip work that was really owed,
    /// which the conformance differential would show as a missing trace line.
    fn owe(&mut self, task: TaskHandle) {
        if let Some(flag) = self.owes_anything.get_mut(usize::from(task.index())) {
            *flag = true;
        }
    }

    /// Record that `task` was preempted before the `portYIELD()` at the end
    /// of the call it is inside.
    pub(crate) fn owe_yield(&mut self, task: TaskHandle) {
        if let Some(flag) = self.owes_yield.get_mut(usize::from(task.index())) {
            *flag = true;
        }
        self.owe(task);
    }

    /// Hand the CPU from `outgoing` to `incoming`: what `prvSwitchThread`
    /// does to `uxCriticalNesting`.
    ///
    /// Everything the outgoing task's call still does from here belongs to
    /// that task at the time it runs again, so the port stops counting the
    /// frame's exits as sim time and starts tallying them;
    /// [`Kernel::settle_unwind`] collects the tally once the frame has
    /// finished. A task being switched in for the first time has a fresh
    /// stack and owes nothing.
    fn hand_over(&mut self, outgoing: TaskHandle, incoming: TaskHandle) {
        // A tail that switches again is still the first frame's tail: the
        // code after the second switch is on the same abandoned stack.
        if self.unwinding.is_none() {
            self.unwinding = Some(outgoing);
            self.port.begin_unwind();
        }
        let index = usize::from(incoming.index());
        if let Some(flag) = self.started.get_mut(index) {
            if !*flag {
                *flag = true;
                if let Some(slot) = self.owed_exits.get_mut(index) {
                    *slot = 0;
                }
                if let Some(flag) = self.owes_yield.get_mut(index) {
                    *flag = false;
                }
                if let Some(slot) = self.owed_trace.get_mut(index) {
                    *slot = OwedTrace::None;
                }
            }
        }
    }

    /// Mark a task as having run, without a hand-over: the first task the
    /// scheduler starts has no predecessor.
    fn note_first_start(&mut self, task: TaskHandle) {
        if let Some(flag) = self.started.get_mut(usize::from(task.index())) {
            *flag = true;
        }
    }

    /// `xTaskIncrementTick`; `true` when a context switch is required.
    pub fn increment_tick(&mut self) -> bool {
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace.event(tick, Event::TaskIncrementTick { tick });
        let mut switch_required = false;
        if self.suspended_depth == 0 {
            let next = self.tick.wrapping_add(1) & Self::MAX_DELAY;
            self.tick = next;
            if next == 0 {
                self.switch_delayed_lists();
            }
            if next >= self.next_unblock_time {
                switch_required = self.wake_due_tasks(next);
            }
            // `!switch_required` first: when the tick already woke a
            // task, this can only set what is already set, and asking
            // costs a TCB resolve for the running priority plus a list
            // read. Setting a `true` to `true` is not worth either.
            if C::USE_PREEMPTION
                && C::USE_TIME_SLICING
                && !switch_required
                && self
                    .lists
                    .len(Self::ready_list(self.current_priority()))
                    .unwrap_or(0)
                    > 1
            {
                switch_required = true;
            }
            // The C guards this site with `xPendedTicks == 0`, so a hook
            // does not fire again for each tick being unwound by
            // `xTaskResumeAll`. It sits after the time-slicing test and
            // before the yield-pending one, and a `FromISR` call inside it
            // can set `yield_pending`, so the order is load-bearing.
            if self.pended_ticks == 0 {
                self.run_tick_hook();
            }
            if C::USE_PREEMPTION && self.yield_pending {
                switch_required = true;
            }
        } else {
            self.pended_ticks = self.pended_ticks.wrapping_add(1);
            // "The tick hook gets called at regular intervals, even if the
            // scheduler is locked" — and here with no guard, because this
            // tick is being pended rather than unwound.
            self.run_tick_hook();
        }
        switch_required
    }

    // ------------------------------------------- task notifications --
    //
    // A notification is the lightest thing a task can block on: no object,
    // no event list, just a slot in the TCB and the delayed list. That is
    // why the stream buffers use one rather than a queue, and why waking a
    // task this way is a list move rather than an event-list walk.

    /// `xTaskGenericNotifyWait`.
    ///
    /// `true` is `pdTRUE`: a notification was received. `Blocked` means the
    /// caller must call again — the C blocks inside this function, and a
    /// kernel with no stacks returns instead.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for an index past the configured array.
    pub fn notify_wait(
        &mut self,
        index: usize,
        clear_on_entry: u32,
        clear_on_exit: u32,
        ticks: u64,
    ) -> Result<Wait<(bool, u32)>> {
        if index >= C::NOTIFICATION_ARRAY_ENTRIES {
            return Err(Error::InvalidArgument);
        }
        let caller = self.current;
        let state = self.notify_state_of(caller, index);
        if state != NotifyState::Received && ticks > 0 && !self.notify_blocked(caller) {
            self.suspend_all();
            self.enter_critical();
            let mut should_block = false;
            if self.notify_state_of(caller, index) != NotifyState::Received {
                if let Ok(tcb) = self.tcbs.resolve_mut(caller) {
                    if let Some(slot) = tcb.notified.get_mut(index) {
                        *slot &= !clear_on_entry;
                    }
                    if let Some(slot) = tcb.notify_state.get_mut(index) {
                        *slot = NotifyState::Waiting;
                    }
                }
                should_block = true;
            }
            self.exit_critical();
            if should_block {
                let tick = self.tick;
                self.trace.note_exits(self.port.exits());
                self.trace_task(caller, |task, name| Event::TaskNotifyWaitBlock {
                    task,
                    name,
                    index,
                });
                let _ = tick;
                self.add_current_task_to_delayed_list(ticks, true)?;
            }
            let already_yielded = self.resume_all();
            if should_block && !already_yielded {
                if self.current == caller {
                    self.port_yield();
                } else {
                    self.owe_yield(caller);
                }
            }
            // The C blocks here; this kernel comes back and is called again.
            self.set_notify_blocked(caller, true);
            return Ok(Wait::Blocked);
        }
        self.set_notify_blocked(caller, false);
        self.enter_critical();
        self.trace.note_exits(self.port.exits());
        self.trace_task(caller, |task, name| Event::TaskNotifyWait {
            task,
            name,
            index,
        });
        let value = self.notified_value_of(caller, index);
        let received = self.notify_state_of(caller, index) == NotifyState::Received;
        if let Ok(tcb) = self.tcbs.resolve_mut(caller) {
            if received {
                if let Some(slot) = tcb.notified.get_mut(index) {
                    *slot &= !clear_on_exit;
                }
            }
            if let Some(slot) = tcb.notify_state.get_mut(index) {
                *slot = NotifyState::NotWaiting;
            }
        }
        self.exit_critical();
        Ok(Wait::Ready((received, value)))
    }

    /// `ulTaskGenericNotifyTake`: the notification used as a lightweight
    /// counting semaphore — block until the value is non-zero, then either
    /// zero it or decrement it by one.
    ///
    /// This is the half of the notification API the kernel was missing.
    /// `notify_wait` waits on *bits*; this waits on a *count*, and the two
    /// are separate calls in the C with their own trace events. Its
    /// partner, `xTaskNotifyGive`, needs no method of its own: the C
    /// defines it as `xTaskGenericNotify( .., 0, eIncrement, NULL )` and
    /// it traces as an ordinary notify, so
    /// `notify(task, index, 0, NotifyAction::Increment)` already is it.
    ///
    /// # The shape, and why it is not `notify_wait`'s
    ///
    /// The C blocks on `ulNotifiedValue == 0`, **not** on the notify
    /// state, and it does no clear-on-entry. Both halves run on the way
    /// out: `traceTASK_NOTIFY_TAKE` fires whether or not the call ever
    /// blocked, which is what makes a take that finds a count already
    /// waiting still emit one line.
    ///
    /// This kernel has no stack, so the block is a return: the first call
    /// answers [`Wait::Blocked`] and the caller is entered again when the
    /// scheduler runs it, taking the second half. `notify_wait_pending`
    /// answers for this call too — a task is only ever inside one of the
    /// two.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for an index past the configured array.
    pub fn notify_take(
        &mut self,
        index: usize,
        clear_on_exit: bool,
        ticks: u64,
    ) -> Result<Wait<u32>> {
        if index >= C::NOTIFICATION_ARRAY_ENTRIES {
            return Err(Error::InvalidArgument);
        }
        let caller = self.current;
        if self.notified_value_of(caller, index) == 0 && ticks > 0 && !self.notify_blocked(caller) {
            self.suspend_all();
            self.enter_critical();
            let mut should_block = false;
            // Re-checked inside the section, as the C does: a notify from
            // an ISR between the two reads would otherwise be lost.
            if self.notified_value_of(caller, index) == 0 {
                if let Ok(tcb) = self.tcbs.resolve_mut(caller) {
                    if let Some(slot) = tcb.notify_state.get_mut(index) {
                        *slot = NotifyState::Waiting;
                    }
                }
                should_block = true;
            }
            self.exit_critical();
            if should_block {
                // The C traces BEFORE it adds itself to the delayed list.
                self.trace.note_exits(self.port.exits());
                self.trace_task(caller, |task, name| Event::TaskNotifyTakeBlock {
                    task,
                    name,
                    index,
                });
                self.add_current_task_to_delayed_list(ticks, true)?;
            }
            let already_yielded = self.resume_all();
            if should_block && !already_yielded {
                if self.current == caller {
                    self.port_yield();
                } else {
                    self.owe_yield(caller);
                }
            }
            self.set_notify_blocked(caller, true);
            return Ok(Wait::Blocked);
        }
        self.set_notify_blocked(caller, false);
        self.enter_critical();
        self.trace.note_exits(self.port.exits());
        self.trace_task(caller, |task, name| Event::TaskNotifyTake {
            task,
            name,
            index,
        });
        let value = self.notified_value_of(caller, index);
        if let Ok(tcb) = self.tcbs.resolve_mut(caller) {
            if value != 0 {
                if let Some(slot) = tcb.notified.get_mut(index) {
                    // Guarded by `value != 0` above, so the saturation
                    // never fires; it is here because the workspace forbids
                    // bare arithmetic, and a decrement is exactly the place
                    // an unguarded one would wrap.
                    *slot = if clear_on_exit {
                        0
                    } else {
                        value.saturating_sub(1)
                    };
                }
            }
            if let Some(slot) = tcb.notify_state.get_mut(index) {
                *slot = NotifyState::NotWaiting;
            }
        }
        self.exit_critical();
        Ok(Wait::Ready(value))
    }

    /// `xTaskGetHandle`: the task registered under `name`.
    ///
    /// # It is not free, and that is the point
    ///
    /// The C walks every ready list, then the delayed lists, then the
    /// suspended and waiting-termination lists, all inside
    /// `vTaskSuspendAll` / `xTaskResumeAll` — and `xTaskResumeAll` takes a
    /// critical section. So a lookup that emits **no trace event** still
    /// costs **one critical-section exit**, which under the sim contract is
    /// one unit of time.
    ///
    /// That is why this exists rather than a scenario keeping the handle it
    /// was given at creation. `AbortDelay`'s remake did exactly that, on the
    /// reasoning that a lookup emitting no event could not change the trace,
    /// and the corpus caught it: every event still agreed and the
    /// exit column was one short at the first block.
    ///
    /// The arena is searched instead of five lists, which cannot change the
    /// answer — a live task is in exactly one of them — and does not change
    /// the accounting either, because the cost is the suspend/resume pair.
    ///
    /// # Errors
    /// [`Error::Gone`] when no live task carries that name.
    pub fn task_get_handle(&mut self, name: &str) -> Result<TaskHandle> {
        self.suspend_all();
        let mut found = None;
        for index in 0..u16::try_from(self.tcbs.capacity()).unwrap_or(u16::MAX) {
            let Some(handle) = self.tcbs.handle_at(index) else {
                continue;
            };
            if self
                .tcbs
                .resolve(handle)
                .is_ok_and(|tcb| tcb.name.as_str() == name)
            {
                found = Some(handle);
                break;
            }
        }
        let _ = self.resume_all();
        found.ok_or(Error::Gone)
    }

    /// `xTaskGenericNotify`.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for a bad index; [`Error::Gone`] for a
    /// stale handle.
    pub fn notify(
        &mut self,
        task: TaskHandle,
        index: usize,
        value: u32,
        action: NotifyAction,
    ) -> Result<bool> {
        if index >= C::NOTIFICATION_ARRAY_ENTRIES {
            return Err(Error::InvalidArgument);
        }
        self.enter_critical();
        let result = self.notify_locked(task, index, value, action, false);
        self.exit_critical();
        result.map(|(ok, _)| ok)
    }

    /// `xTaskGenericNotifyFromISR`: the same, without a critical section
    /// that costs anything, and putting a woken task on the pending-ready
    /// list when the scheduler is suspended.
    ///
    /// # Errors
    /// As [`Kernel::notify`].
    pub fn notify_from_isr(
        &mut self,
        task: TaskHandle,
        index: usize,
        value: u32,
        action: NotifyAction,
    ) -> Result<(bool, Woken)> {
        if index >= C::NOTIFICATION_ARRAY_ENTRIES {
            return Err(Error::InvalidArgument);
        }
        let mask = self.port.enter_critical_from_isr();
        let result = self.notify_locked(task, index, value, action, true);
        self.port.exit_critical_from_isr(mask);
        result.map(|(ok, woken)| (ok, if woken { Woken::YES } else { Woken::NO }))
    }

    /// The body both notify paths share.
    fn notify_locked(
        &mut self,
        task: TaskHandle,
        index: usize,
        value: u32,
        action: NotifyAction,
        from_isr: bool,
    ) -> Result<(bool, bool)> {
        let original = self.notify_state_of(task, index);
        let mut ok = true;
        {
            let tcb = self.tcbs.resolve_mut(task)?;
            if let Some(slot) = tcb.notify_state.get_mut(index) {
                *slot = NotifyState::Received;
            }
            if let Some(slot) = tcb.notified.get_mut(index) {
                match action {
                    NotifyAction::SetBits => *slot |= value,
                    NotifyAction::Increment => *slot = slot.wrapping_add(1),
                    NotifyAction::Overwrite => *slot = value,
                    NotifyAction::NoOverwrite => {
                        if original == NotifyState::Received {
                            ok = false;
                        } else {
                            *slot = value;
                        }
                    }
                    NotifyAction::None => {}
                }
            }
        }
        // `traceTASK_NOTIFY` is hooked; `traceTASK_NOTIFY_FROM_ISR` is not,
        // so an interrupt's notification says nothing on either side.
        if !from_isr {
            self.trace.note_exits(self.port.exits());
            self.trace_task(task, |t, name| Event::TaskNotify {
                task: t,
                name,
                index,
            });
        }
        let mut woke_higher = false;
        if original == NotifyState::Waiting {
            let item = Self::state_item(task);
            if from_isr && self.suspended_depth != 0 {
                let _ = self.lists.remove(Self::event_item(task));
                let value = self.lists.value(Self::event_item(task))?;
                self.lists
                    .insert(Self::pending_ready_list(), Self::event_item(task), value)?;
            } else {
                if self.lists.container(item)?.is_some() {
                    let _ = self.lists.remove(item);
                }
                self.add_task_to_ready_list(task)?;
            }
            let woken = self.tcbs.resolve(task).map(|t| t.priority).unwrap_or(0);
            if woken > self.current_priority() {
                woke_higher = true;
                if from_isr {
                    // The C sets `xYieldPendings[ 0 ]` here as well as
                    // reporting the wake through the caller's flag — so an
                    // interrupt that discards the flag still gets the
                    // switch, on the way out of `xTaskIncrementTick`. That
                    // is why the tick hook runs before the yield-pending
                    // test and not after it.
                    self.yield_pending = true;
                } else {
                    // taskYIELD_ANY_CORE_IF_USING_PREEMPTION, inside the
                    // section.
                    self.port_yield();
                }
            }
        }
        Ok((ok, woke_higher))
    }

    /// `xTaskGenericNotifyStateClear`. `None` is the calling task.
    ///
    /// # Errors
    /// As [`Kernel::notify`].
    pub fn notify_state_clear(&mut self, task: Option<TaskHandle>, index: usize) -> Result<bool> {
        if index >= C::NOTIFICATION_ARRAY_ENTRIES {
            return Err(Error::InvalidArgument);
        }
        let target = task.unwrap_or(self.current);
        self.enter_critical();
        let cleared = self.notify_state_of(target, index) == NotifyState::Received;
        if cleared {
            if let Ok(tcb) = self.tcbs.resolve_mut(target) {
                if let Some(slot) = tcb.notify_state.get_mut(index) {
                    *slot = NotifyState::NotWaiting;
                }
            }
        }
        self.exit_critical();
        Ok(cleared)
    }

    /// `ulNotifiedValue[ index ]` for a task. `None` is the calling task.
    ///
    /// The C reaches this through `xTaskNotifyAndQuery`, which hands back
    /// the value a notification is about to REPLACE, and through
    /// `ulTaskNotifyValueClear`. Neither could be implemented without it:
    /// `xTaskNotifyAndQuery` is `xTaskGenericNotify` with a
    /// `pulPreviousNotificationValue` out-parameter, and `TaskNotify.c:373`
    /// asserts on what comes back. The field existed; only the way to read
    /// it was missing, which is the kind of gap a C ABI finds and a Rust
    /// face never does.
    ///
    /// # Errors
    /// `InvalidArgument` if `index` is outside the notification array, or
    /// the resolve error if the handle is stale.
    pub fn notify_value(&mut self, task: Option<TaskHandle>, index: usize) -> Result<u32> {
        if index >= C::NOTIFICATION_ARRAY_ENTRIES {
            return Err(Error::InvalidArgument);
        }
        let target = task.unwrap_or(self.current);
        self.enter_critical();
        let value = self
            .tcbs
            .resolve(target)
            .map(|tcb| tcb.notified.get(index).copied().unwrap_or(0));
        self.exit_critical();
        value
    }

    /// `ulTaskGenericNotifyValueClear`: clear `bits` in the task's
    /// notification value and answer what it was BEFORE the clear.
    ///
    /// # Errors
    /// As [`Kernel::notify_value`].
    pub fn notify_value_clear(
        &mut self,
        task: Option<TaskHandle>,
        index: usize,
        bits: u32,
    ) -> Result<u32> {
        if index >= C::NOTIFICATION_ARRAY_ENTRIES {
            return Err(Error::InvalidArgument);
        }
        let target = task.unwrap_or(self.current);
        self.enter_critical();
        let before = match self.tcbs.resolve_mut(target) {
            Ok(tcb) => match tcb.notified.get_mut(index) {
                Some(slot) => {
                    let was = *slot;
                    *slot &= !bits;
                    Ok(was)
                }
                None => Err(Error::InvalidArgument),
            },
            Err(e) => Err(e),
        };
        self.exit_critical();
        before
    }

    /// Whether `task` is part-way through a notification wait — it blocked
    /// once and has not yet run the half of `xTaskGenericNotifyWait` that
    /// happens when a task wakes.
    ///
    /// A caller that blocks on a notification needs this: the C's wait
    /// returns *after* the wake, so its second half always runs, and a
    /// stackless caller that decided not to block again would otherwise
    /// skip it and leave the slot marked as received for ever.
    #[must_use]
    pub fn notify_wait_pending(&self, task: TaskHandle) -> bool {
        self.notify_blocked(task)
    }

    /// Whether `task` has a stream-buffer call to resume, and the local it
    /// left behind. Taking it clears the marker.
    pub(crate) fn take_stream_resume(&mut self, task: TaskHandle) -> Option<usize> {
        let tcb = self.tcbs.resolve_mut(task).ok()?;
        if !tcb.stream_resume {
            return None;
        }
        tcb.stream_resume = false;
        Some(tcb.stream_local)
    }

    /// Remember where a preempted stream-buffer call has to start again.
    pub(crate) fn set_stream_resume(&mut self, task: TaskHandle, local: usize) {
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            tcb.stream_resume = true;
            tcb.stream_local = local;
        }
    }

    fn notify_blocked(&self, task: TaskHandle) -> bool {
        self.tcbs
            .resolve(task)
            .map(|t| t.notify_blocked)
            .unwrap_or(false)
    }

    fn set_notify_blocked(&mut self, task: TaskHandle, blocked: bool) {
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            tcb.notify_blocked = blocked;
        }
    }

    fn notify_state_of(&self, task: TaskHandle, index: usize) -> NotifyState {
        self.tcbs
            .resolve(task)
            .ok()
            .and_then(|t| t.notify_state.get(index).copied())
            .unwrap_or(NotifyState::NotWaiting)
    }

    fn notified_value_of(&self, task: TaskHandle, index: usize) -> u32 {
        self.tcbs
            .resolve(task)
            .ok()
            .and_then(|t| t.notified.get(index).copied())
            .unwrap_or(0)
    }

    /// `xTaskGetTickCountFromISR`.
    ///
    /// No critical section, exactly as `xTaskGetTickCount` takes none: the
    /// Posix port sets `portTICK_TYPE_IS_ATOMIC`, so reading the count is
    /// a single load on both sides and costs no sim time.
    #[must_use]
    pub const fn tick_count_from_isr(&self) -> u64 {
        self.tick
    }

    /// `vTaskMissedYield`: a yield that could not be taken where it was
    /// asked for — because a task is walking an event list, or because the
    /// scheduler is suspended — and must happen on the way out instead.
    pub(crate) fn missed_yield(&mut self) {
        self.yield_pending = true;
    }

    /// `vApplicationTickHook()`, when `configUSE_TICK_HOOK` is 1.
    ///
    /// The hook is copied out, run, and the result stored: that is what
    /// lets it borrow the kernel mutably while the kernel owns it. A hook
    /// that reads the kernel's own copy of itself therefore sees the value
    /// from before this call.
    fn run_tick_hook(&mut self) {
        if !C::USE_TICK_HOOK {
            return;
        }
        let hook = self.tick_hook;
        self.tick_hook = hook.tick(self);
    }

    /// The tick hook, as it now stands.
    ///
    /// A scenario's `xAre...StillRunning()` reads the status its interrupt
    /// half latched, so the owner of the kernel needs to see it.
    pub const fn tick_hook(&self) -> &H {
        &self.tick_hook
    }

    /// The tick hook, to install or reset one after construction.
    pub const fn tick_hook_mut(&mut self) -> &mut H {
        &mut self.tick_hook
    }

    /// The wake loop inside `xTaskIncrementTick`.
    // Out of line: this walks the delayed list and runs only when a tick
    // reaches the next unblock time, but `increment_tick` runs on EVERY tick
    // and was carrying a frame sized for the walk regardless.
    #[inline(never)]
    fn wake_due_tasks(&mut self, now: u64) -> bool {
        let mut switch_required = false;
        loop {
            let delayed = self.delayed_list();
            if self.lists.is_empty(delayed) != Ok(false) {
                self.next_unblock_time = Self::MAX_DELAY;
                return switch_required;
            }
            let Ok(Some(item)) = self.lists.head(delayed) else {
                self.next_unblock_time = Self::MAX_DELAY;
                return switch_required;
            };
            let Ok(wake_at) = self.lists.value(item) else {
                return switch_required;
            };
            if now < wake_at {
                self.next_unblock_time = wake_at;
                return switch_required;
            }
            let Ok(task) = self.task_of_state_item(item) else {
                return switch_required;
            };
            let _ = self.lists.remove(item);
            if self
                .lists
                .container(Self::event_item(task))
                .unwrap_or(None)
                .is_some()
            {
                let _ = self.lists.remove(Self::event_item(task));
            }
            if self.add_task_to_ready_list(task).is_err() {
                return switch_required;
            }
            if C::USE_PREEMPTION {
                let woken = self.tcbs.resolve(task).map(|t| t.priority).unwrap_or(0);
                if woken > self.current_priority() {
                    switch_required = true;
                }
            }
        }
    }

    /// `taskSWITCH_DELAYED_LISTS()`.
    // Out of line for the same reason: the overflow swap happens when the
    // tick count wraps, which is once in a very long while.
    #[inline(never)]
    fn switch_delayed_lists(&mut self) {
        self.delayed_swapped = !self.delayed_swapped;
        self.overflows = self.overflows.wrapping_add(1);
        self.reset_next_task_unblock_time();
    }

    /// `prvResetNextTaskUnblockTime`.
    fn reset_next_task_unblock_time(&mut self) {
        let delayed = self.delayed_list();
        self.next_unblock_time = self.lists.head_value(delayed).unwrap_or(Self::MAX_DELAY);
    }

    // ------------------------------------------------------------- delay --

    /// `vTaskDelay`.
    ///
    /// # Errors
    /// A list error surfaces as itself.
    pub fn delay(&mut self, ticks: u64) -> Result<()> {
        let caller = self.current;
        let mut already_yielded = false;
        if ticks > 0 {
            self.suspend_all();
            {
                let current = self.current;
                self.trace_task(current, |task, name| Event::TaskDelay { task, name, ticks });
                self.add_current_task_to_delayed_list(ticks, false)?;
            }
            already_yielded = self.resume_all();
        }
        // taskYIELD_WITHIN_API(), outside any critical section — unless a
        // tick switched us out somewhere above, in which case this line is
        // on a stack that is not running, and runs when the task does.
        if !already_yielded {
            if self.current == caller {
                self.port_yield();
            } else {
                self.owe_yield(caller);
            }
        }
        Ok(())
    }

    /// `xTaskAbortDelay`: pull a task out of the Blocked state early.
    ///
    /// `true` is `pdPASS` — the task really was blocked and is now ready.
    /// A task that was not blocked is left alone and `false` comes back,
    /// which is the C's `pdFAIL`.
    ///
    /// The shape matters as much as the effect: the whole thing runs with
    /// the scheduler suspended, the event list is touched inside a critical
    /// section of its own because an interrupt can reach it, and a yield
    /// that the higher priority of the woken task calls for is *pended*
    /// rather than taken, so it happens on the way out of `xTaskResumeAll`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn abort_delay(&mut self, task: TaskHandle) -> Result<bool> {
        if !self.tcbs.contains(task) {
            return Err(Error::Gone);
        }
        self.suspend_all();
        // eTaskGetState: a critical section of its own on any task but the
        // running one, and the running one is never Blocked.
        let blocked = self.task_state_get(task)? == TaskState::Blocked;
        if !blocked {
            let _ = self.resume_all();
            return Ok(false);
        }
        let item = Self::state_item(task);
        // `remove` is its own guard, as above.
        let _ = self.lists.remove(item);
        self.enter_critical();
        {
            let event = Self::event_item(task);
            // `remove` answers `NotActive` when the item is in no list, so
            // it is its own guard -- and `is_ok` is exactly "something was
            // removed", which is what the flag below depends on.
            if self.lists.remove(event).is_ok() {
                if let Some(flag) = self.delay_aborted.get_mut(usize::from(task.index())) {
                    *flag = true;
                }
            }
        }
        self.exit_critical();
        self.add_task_to_ready_list(task)?;
        // configUSE_PREEMPTION, one core: pend the yield rather than take
        // it, so it lands where `xTaskResumeAll` puts it.
        let woken = self.tcbs.resolve(task).map(|t| t.priority).unwrap_or(0);
        if woken > self.current_priority() {
            self.yield_pending = true;
        }
        let _ = self.resume_all();
        Ok(true)
    }

    /// `prvAddCurrentTaskToDelayedList`.
    pub(crate) fn add_current_task_to_delayed_list(
        &mut self,
        ticks: u64,
        can_block_indefinitely: bool,
    ) -> Result<()> {
        let now = self.tick;
        let current = self.current;
        // About to enter a delayed list, so the abort flag is cleared here
        // and can only be seen set by a task that really was aborted.
        if let Some(flag) = self.delay_aborted.get_mut(usize::from(current.index())) {
            *flag = false;
        }
        let item = Self::state_item(current);
        // `uxListRemove` already answers `NotActive` for an item that is
        // in no list, and both it and the container test are one read of
        // the same node — so asking first was reading it twice. This is
        // the running task's state item, so it is in the ready list and
        // the test passed anyway.
        let _ = self.lists.remove(item);
        if ticks == Self::MAX_DELAY && can_block_indefinitely {
            self.lists.insert_end(Self::suspended_list(), item)?;
            return Ok(());
        }
        let wake_at = now.wrapping_add(ticks) & Self::MAX_DELAY;
        // `vListInsert` sets the item's value from the one it sorts by, so
        // both arms below write `wake_at` themselves. Setting it here as
        // well was a second lookup of the same item to store the same
        // number into it.
        if wake_at < now {
            self.trace_task(current, |task, name| {
                Event::MovedTaskToOverflowDelayedList { task, name }
            });
            let list = self.overflow_delayed_list();
            self.lists.insert(list, item, wake_at)?;
        } else {
            self.trace_task(current, |task, name| Event::MovedTaskToDelayedList {
                task,
                name,
            });
            let list = self.delayed_list();
            self.lists.insert(list, item, wake_at)?;
            if wake_at < self.next_unblock_time {
                self.next_unblock_time = wake_at;
            }
        }
        Ok(())
    }

    // --------------------------------------------------- suspend / resume --

    /// `vTaskSuspend`. `None` suspends the calling task.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn suspend(&mut self, task: Option<TaskHandle>) -> Result<()> {
        let caller = self.current;
        let target = task.unwrap_or(caller);
        self.enter_critical();
        {
            if !self.tcbs.contains(target) {
                self.exit_critical();
                return Err(Error::Gone);
            }
            self.trace_task(target, |task, name| Event::TaskSuspend { task, name });
            let item = Self::state_item(target);
            // `remove` is its own guard: it answers `NotActive` for an item
            // in no list, and the result was discarded either way.
            let _ = self.lists.remove(item);
            let event = Self::event_item(target);
            if self.lists.container(event)?.is_some() {
                let _ = self.lists.remove(event);
            }
            self.lists.insert_end(Self::suspended_list(), item)?;
            // `vTaskSuspend`, `tasks.c`:
            //
            // ```c
            // if( pxTCB->ucNotifyState[ x ] == taskWAITING_NOTIFICATION )
            // {
            //     /* The task was blocked to wait for a notification, but
            //      * is now suspended, so no notification was received. */
            //     pxTCB->ucNotifyState[ x ] = taskNOT_WAITING_NOTIFICATION;
            // }
            // ```
            //
            // Not tidying: `eTaskGetState` calls a task on the suspended
            // list BLOCKED rather than SUSPENDED if any of its notification
            // slots is still waiting, so leaving the flag set makes a
            // suspended task report as blocked for ever.
            // `TaskNotify.c:498` asserts exactly that, in a timer callback
            // that suspends a task which is waiting for a notification --
            // and a task that reports the wrong state there is a task the
            // rest of that test never resumes correctly.
            if let Ok(tcb) = self.tcbs.resolve_mut(target) {
                for slot in &mut tcb.notify_state {
                    if *slot == NotifyState::Waiting {
                        *slot = NotifyState::NotWaiting;
                    }
                }
            }
        }
        self.exit_critical();
        // A second, separate critical section — two outermost exits, which
        // is what the C does and therefore what the tick count depends on.
        if self.running {
            self.enter_critical();
            self.reset_next_task_unblock_time();
            self.exit_critical();
        }
        if target == caller {
            if self.running {
                // portYIELD_WITHIN_API(), outside the section.
                if self.current == caller {
                    self.port_yield();
                } else {
                    self.owe_yield(caller);
                }
            } else if self.lists.len(Self::suspended_list()).unwrap_or(0) != self.task_count {
                self.switch_context();
            }
        }
        Ok(())
    }

    /// `vTaskResume`.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn resume(&mut self, task: TaskHandle) -> Result<()> {
        if task == self.current || task.is_null() {
            return Ok(());
        }
        if !self.tcbs.contains(task) {
            return Err(Error::Gone);
        }
        self.enter_critical();
        {
            if self.task_is_suspended(task)? {
                self.trace_task(task, |task, name| Event::TaskResume { task, name });
                let _ = self.lists.remove(Self::state_item(task));
                self.add_task_to_ready_list(task)?;
                // taskYIELD_ANY_CORE_IF_USING_PREEMPTION: inside the
                // section, and only when the resumed task outranks the
                // running one.
                let resumed = self.tcbs.resolve(task)?.priority;
                if C::USE_PREEMPTION && self.current_priority() < resumed {
                    self.port_yield();
                }
            }
        }
        self.exit_critical();
        Ok(())
    }

    /// `prvTaskIsTaskSuspended`: on the suspended list, not on the pending
    /// ready list, and not waiting on an event.
    fn task_is_suspended(&self, task: TaskHandle) -> Result<bool> {
        if self.lists.container(Self::state_item(task))? != Some(Self::suspended_list()) {
            return Ok(false);
        }
        let event_list = self.lists.container(Self::event_item(task))?;
        if event_list == Some(Self::pending_ready_list()) {
            return Ok(false);
        }
        Ok(event_list.is_none())
    }

    // -------------------------------------------------------- priorities --

    /// `vTaskDelete`. `None` deletes the calling task.
    ///
    /// # Two paths, and the C picks between them for a reason
    ///
    /// Deleting *another* task frees its TCB on the spot. Deleting the
    /// **running** task cannot: the caller is still executing out of it. The
    /// C parks such a task on `xTasksWaitingTermination` and lets the idle
    /// task reclaim it, which is what `prvCheckTasksWaitingTermination` is
    /// for — so [`Kernel::check_tasks_waiting_termination`] is where the
    /// deferred half lands here, called from the idle body at the same point
    /// in the same loop.
    ///
    /// # The frees are outside the critical section, and they cost exits
    ///
    /// `prvDeleteTCB` runs **after** `taskEXIT_CRITICAL()` and frees twice —
    /// `vPortFreeStack( pxStack )` then `vPortFree( pxTCB )`. On the oracle's
    /// heap every free suspends the scheduler, so a delete is one exit for
    /// its own section plus two for the frees, in that order. Folding the
    /// frees into the section, or forgetting them, moves every later tick.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn task_delete(&mut self, task: Option<TaskHandle>) -> Result<()> {
        // `prvGetTCBFromHandle`: NULL is the calling task.
        let target = task.unwrap_or(self.current);
        self.tcbs.resolve(target)?;

        self.enter_critical();
        // `uxListRemove( &( pxTCB->xStateListItem ) )`. The C follows this
        // with `taskRESET_READY_PRIORITY`, which is an EMPTY MACRO under the
        // oracle's `configUSE_PORT_OPTIMISED_TASK_SELECTION 0` — there is no
        // priority bitmap to clear, and `vTaskSwitchContext` rediscovers the
        // top priority by walking down. Nothing to model.
        let state = Self::state_item(target);
        if matches!(self.lists.container(state), Ok(Some(_))) {
            let _ = self.lists.remove(state);
        }
        // `if( listLIST_ITEM_CONTAINER( &( pxTCB->xEventListItem ) ) != NULL )`
        // — a task blocked on a queue is on two lists, and both must let go.
        let event = Self::event_item(target);
        if matches!(self.lists.container(event), Ok(Some(_))) {
            let _ = self.lists.remove(event);
        }
        // `uxTaskNumber++` is not modelled: it exists so kernel-aware
        // debuggers can tell the task lists changed, and nothing the trace
        // records reads it.

        // `xSchedulerRunning != pdFALSE && taskTASK_IS_RUNNING_OR_SCHEDULED_
        // _TO_YIELD( pxTCB )`, which at `configNUMBER_OF_CORES 1` is exactly
        // `pxTCB == pxCurrentTCB`.
        let defer = self.running && target == self.current;
        if defer {
            // `vListInsertEnd( &xTasksWaitingTermination, ... )` and
            // `++uxDeletedTasksWaitingCleanUp`. The count is NOT decremented
            // here — the idle task does that when it reaps.
            self.awaiting_reap = Some(target);
            self.trace_task(target, |task, name| Event::TaskDelete { task, name });
        } else {
            self.task_count = self.task_count.saturating_sub(1);
            self.trace_task(target, |task, name| Event::TaskDelete { task, name });
            self.reset_next_task_unblock_time();
        }
        self.exit_critical();

        // `if( xDeleteTCBInIdleTask != pdTRUE ) { prvDeleteTCB( pxTCB ); }`,
        // outside the section.
        if !defer {
            self.delete_tcb(target);
        }

        // `taskYIELD_WITHIN_API()`. The C guards this with
        // `xSchedulerRunning && pxTCB == pxCurrentTCB`, which is the same
        // condition that chose the deferred path.
        if defer {
            self.port_yield();
        }
        Ok(())
    }

    /// `prvDeleteTCB`: two frees, and on the oracle's heap each one suspends
    /// the scheduler, so each is one outermost exit.
    fn delete_tcb(&mut self, task: TaskHandle) {
        // `vPortFreeStack( pxTCB->pxStack )`.
        self.account_for_allocation();
        // `vPortFree( pxTCB )`.
        self.account_for_allocation();
        let _ = self.tcbs.remove(task);

        // The index is now free for reuse, so mark the task UNSTARTED.
        //
        // The C frees the TCB and a later `xTaskCreate` gets fresh memory,
        // so a new task cannot inherit anything. Our arena hands back the
        // same index, and the per-index bookkeeping — owed exits, an owed
        // yield, an owed trace line — is keyed on that index, not on the
        // TCB. Without this a task deleted while it still owed the tail of
        // a call bequeaths that debt to its successor, and the successor
        // pays an exit it never incurred: exactly how `death`'s second
        // cycle first diverged, one exit early, 1,100 lines after the first
        // cycle had matched perfectly.
        //
        // Clearing this ONE flag is the whole fix, and deliberately so.
        // `hand_over` already empties all three of those on a task's first
        // switch-in, and a reused index is a first switch-in again. Zeroing
        // them here as well would work and would be worse: two places that
        // must agree about what a fresh task owes, and a gate that cannot
        // catch either one being dropped because the other still covers it.
        if let Some(slot) = self.started.get_mut(usize::from(task.index())) {
            *slot = false;
        }
    }

    /// `prvCheckTasksWaitingTermination`, called from the idle task.
    ///
    /// # This must cost NOTHING when nothing is pending
    ///
    /// The C's body is `while( uxDeletedTasksWaitingCleanUp > 0 )`, so an
    /// idle loop with no deleted task takes no critical section and pays no
    /// exit. That is what lets this be wired into the idle body without
    /// disturbing a single scenario in the corpus — and it is the property
    /// the corpus is checking when it stays byte-identical.
    ///
    /// At most one task can be pending: only the *running* task defers, and
    /// it yields before it can return, so a second deferred delete cannot
    /// happen until this has run.
    pub fn check_tasks_waiting_termination(&mut self) {
        let Some(task) = self.awaiting_reap else {
            return;
        };
        // The C takes the section per reaped task, decrements both counts
        // inside it, and calls `prvDeleteTCB` outside.
        self.enter_critical();
        self.awaiting_reap = None;
        self.task_count = self.task_count.saturating_sub(1);
        self.exit_critical();
        self.delete_tcb(task);
    }

    /// `vTaskPrioritySet`. `None` is the calling task.    /// `vTaskPrioritySet`. `None` is the calling task.
    ///
    /// # Errors
    /// [`Error::Gone`] for a stale handle.
    pub fn set_priority(&mut self, task: Option<TaskHandle>, new_priority: u8) -> Result<()> {
        let new_priority = new_priority.min(C::MAX_PRIORITIES.saturating_sub(1));
        let target = task.unwrap_or(self.current);
        let mut yield_required = false;
        self.enter_critical();
        {
            if !self.tcbs.contains(target) {
                self.exit_critical();
                return Err(Error::Gone);
            }
            self.trace_task(target, |task, name| Event::TaskPrioritySet {
                task,
                name,
                priority: Self::priority_value(new_priority),
            });
            let base = self.tcbs.resolve(target)?.base_priority;
            if base != new_priority {
                if new_priority > base {
                    if target != self.current && new_priority > self.current_priority() {
                        yield_required = true;
                    }
                } else if target == self.current {
                    yield_required = true;
                }
                let used_on_entry = self.tcbs.resolve(target)?.priority;
                {
                    let tcb = self.tcbs.resolve_mut(target)?;
                    if tcb.base_priority == tcb.priority || new_priority > tcb.priority {
                        tcb.priority = new_priority;
                    }
                    tcb.base_priority = new_priority;
                }
                let event_value = u64::from(C::MAX_PRIORITIES.saturating_sub(new_priority));
                self.lists
                    .set_value(Self::event_item(target), event_value)?;
                let item = Self::state_item(target);
                if self.lists.container(item)? == Some(Self::ready_list(used_on_entry)) {
                    let _ = self.lists.remove(item);
                    self.add_task_to_ready_list(target)?;
                }
                // taskYIELD_TASK_CORE_IF_USING_PREEMPTION: inside the
                // section.
                if yield_required && C::USE_PREEMPTION {
                    self.port_yield();
                }
            }
        }
        self.exit_critical();
        Ok(())
    }

    // ------------------------------------------------- scheduler suspend --

    /// `vTaskSuspendAll`.
    pub fn suspend_all(&mut self) {
        self.suspended_depth = self.suspended_depth.wrapping_add(1);
    }

    /// Move everything an ISR readied onto the ready lists.
    ///
    /// Out of line because it is the rare half of [`Kernel::resume_all`]: the
    /// pending list is empty on essentially every call, and inlining this made
    /// every one of those calls carry a frame sized for the walk.
    #[inline(never)]
    fn drain_pending_ready(&mut self) {
        let mut moved_any = false;
        while self.lists.is_empty(Self::pending_ready_list()) == Ok(false) {
            let Ok(Some(item)) = self.lists.head(Self::pending_ready_list()) else {
                break;
            };
            let Ok(task) = self.task_of_event_item(item) else {
                break;
            };
            let _ = self.lists.remove(Self::event_item(task));
            let _ = self.lists.remove(Self::state_item(task));
            if self.add_task_to_ready_list(task).is_err() {
                break;
            }
            moved_any = true;
            let woken = self.tcbs.resolve(task).map(|t| t.priority).unwrap_or(0);
            if woken > self.current_priority() {
                self.yield_pending = true;
            }
        }
        if moved_any {
            self.reset_next_task_unblock_time();
        }
    }

    /// Run the ticks that landed while the scheduler was suspended.
    ///
    /// Out of line for the same reason as its sibling: `pended_ticks` is zero
    /// on essentially every call.
    #[inline(never)]
    fn unwind_pended_ticks(&mut self) {
        let mut pended = self.pended_ticks;
        while pended > 0 {
            if self.increment_tick() {
                self.yield_pending = true;
            }
            pended = pended.saturating_sub(1);
        }
        self.pended_ticks = 0;
    }

    /// `xTaskResumeAll`; `true` when it yielded on the caller's behalf.
    pub fn resume_all(&mut self) -> bool {
        let mut already_yielded = false;
        self.enter_critical();
        {
            self.suspended_depth = self.suspended_depth.saturating_sub(1);
            if self.suspended_depth == 0 && self.task_count > 0 {
                if self.lists.is_empty(Self::pending_ready_list()) == Ok(false) {
                    self.drain_pending_ready();
                }
                if self.pended_ticks > 0 {
                    self.unwind_pended_ticks();
                }
                if self.yield_pending {
                    if C::USE_PREEMPTION {
                        already_yielded = true;
                    }
                    // taskYIELD_TASK_CORE_IF_USING_PREEMPTION: inside the
                    // section.
                    self.port_yield();
                }
            }
        }
        self.exit_critical();
        already_yielded
    }

    // ------------------------------------------- the blocking-call frame --

    /// Start (or continue) a blocking call's `for(;;)`.
    ///
    /// The first pass records the block time and the entry timestamp, the
    /// way `xEntryTimeSet` / `vTaskInternalSetTimeOutState` do; later
    /// passes leave the running total alone, because the `ticks` the caller
    /// passes is the original block time and the kernel is holding what is
    /// left of it.
    pub(crate) fn begin_wait(
        &mut self,
        task: TaskHandle,
        queue: QueueHandle,
        ticks: u64,
    ) -> Result<()> {
        let tick = self.tick;
        let overflows = self.overflows;
        let Ok(tcb) = self.tcbs.resolve_mut(task) else {
            // A queue call made before the scheduler started, from what the
            // C would call `main()`: there is no task to keep a frame for,
            // and such a call never blocks — `xSemaphoreGive` on a fresh
            // semaphore is the usual one.
            return Ok(());
        };
        if tcb.wait.queue != queue || !tcb.wait.entry_set {
            tcb.wait = WaitFrame {
                queue,
                ticks,
                entering: tick,
                overflows,
                entry_set: true,
                inherited: false,
            };
        }
        // The frame is set either way on this path -- either it was written
        // just now, or it was already this queue's.
        if let Some(flag) = self.wait_set.get_mut(usize::from(task.index())) {
            *flag = true;
        }
        Ok(())
    }

    /// The call finished, one way or the other.
    pub(crate) fn end_wait(&mut self, task: TaskHandle) {
        // Almost every call arrives with no frame to clear, and the mirror
        // says so in one byte where resolving the TCB to read `entry_set`
        // cost an arena lookup. Out of range reads as SET, so an impossible
        // index still takes the slow path below.
        let at = usize::from(task.index());
        if !self.wait_set.get(at).copied().unwrap_or(true) {
            return;
        }
        if let Some(flag) = self.wait_set.get_mut(at) {
            *flag = false;
        }
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            // Now that the frame is only set on the path that blocks, most
            // calls reach here with nothing to clear, and this was writing
            // a default frame over a default frame. `entry_set` is the
            // sentinel the C calls `xEntryTimeSet`, and it is the first
            // thing set — `inherited` is only reached far inside the
            // blocking path, so it cannot be true while this is false.
            if tcb.wait.entry_set {
                tcb.wait = WaitFrame::default();
            }
        }
    }

    /// `xTicksToWait` as it now stands.
    pub(crate) fn remaining_ticks(&self, task: TaskHandle) -> u64 {
        self.tcbs.resolve(task).map(|t| t.wait.ticks).unwrap_or(0)
    }

    /// `xInheritanceOccurred`.
    pub(crate) fn wait_inherited(&self, task: TaskHandle) -> bool {
        self.tcbs
            .resolve(task)
            .map(|t| t.wait.inherited)
            .unwrap_or(false)
    }

    pub(crate) fn set_wait_inherited(&mut self, task: TaskHandle) {
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            tcb.wait.inherited = true;
        }
    }

    /// `xTaskCheckForTimeOut`: `true` when the block time has run out.
    /// Takes a critical section, as the C does, and charges the elapsed
    /// time against what is left.
    pub(crate) fn check_for_timeout(&mut self, task: TaskHandle) -> bool {
        self.enter_critical();
        // An aborted delay is not the same as a time out, but it has the
        // same result: stop waiting.
        if self.delay_aborted.get(usize::from(task.index())).copied() == Some(true) {
            if let Some(flag) = self.delay_aborted.get_mut(usize::from(task.index())) {
                *flag = false;
            }
            self.exit_critical();
            return true;
        }
        let now = self.tick;
        let overflows = self.overflows;
        let result = match self.tcbs.resolve_mut(task) {
            Ok(tcb) => {
                let elapsed = now.wrapping_sub(tcb.wait.entering) & Self::MAX_DELAY;
                if tcb.wait.ticks == Self::MAX_DELAY {
                    // An indefinite block never times out.
                    false
                } else if overflows != tcb.wait.overflows && now >= tcb.wait.entering {
                    tcb.wait.ticks = 0;
                    true
                } else if elapsed < tcb.wait.ticks {
                    tcb.wait.ticks = tcb.wait.ticks.saturating_sub(elapsed);
                    tcb.wait.entering = now;
                    tcb.wait.overflows = overflows;
                    false
                } else {
                    tcb.wait.ticks = 0;
                    true
                }
            }
            Err(_) => true,
        };
        self.exit_critical();
        result
    }

    /// `vTaskPlaceOnUnorderedEventList`: the item value carries the
    /// condition rather than the priority, so the item goes on the end and
    /// the list is never sorted.
    ///
    /// `taskEVENT_LIST_ITEM_VALUE_IN_USE` is not set here the way the C
    /// sets it. The C needs it because the same `ListItem_t` is a
    /// priority-ordered event item the rest of the time and the flag says
    /// which; the value is only ever read back by the event-group code,
    /// which masks the control byte off, so setting it would change
    /// nothing but the arithmetic in the doc comment.
    pub(crate) fn place_on_unordered_event_list(
        &mut self,
        list: ListId,
        value: u64,
        ticks: u64,
    ) -> Result<()> {
        let item = Self::event_item(self.current);
        self.lists.set_value(item, value)?;
        self.lists.insert_end(list, item)?;
        self.add_current_task_to_delayed_list(ticks, true)
    }

    /// `vTaskRemoveFromUnorderedEventList`: write the answer into the item
    /// value, then wake the task.
    ///
    /// Unlike `xTaskRemoveFromEventList` this always goes straight to the
    /// ready list. It is only ever called with the scheduler suspended —
    /// `xEventGroupSetBits` holds it across the whole walk — and the C
    /// still bypasses the pending-ready list, because the value it just
    /// wrote is the task's return value and a second pass would overwrite
    /// it.
    pub(crate) fn remove_from_unordered_event_list(
        &mut self,
        item: ItemId,
        value: u64,
    ) -> Result<()> {
        self.lists.set_value(item, value)?;
        let task = self.task_of_event_item(item)?;
        let _ = self.lists.remove(item);
        let _ = self.lists.remove(Self::state_item(task));
        self.add_task_to_ready_list(task)?;
        if self.tcbs.resolve(task)?.priority > self.current_priority() {
            self.yield_pending = true;
        }
        Ok(())
    }

    /// `uxTaskResetEventItemValue`: read the answer the unblocker left, and
    /// put the item back to the priority order an ordinary event list wants.
    pub(crate) fn reset_event_item_value(&mut self, task: TaskHandle) -> Result<u64> {
        let item = Self::event_item(task);
        let value = self.lists.value(item)?;
        let priority = self.tcbs.resolve(task)?.priority;
        self.lists
            .set_value(item, u64::from(C::MAX_PRIORITIES.saturating_sub(priority)))?;
        Ok(value)
    }

    /// Whether `task` is resuming an event-group wait rather than starting
    /// one. Taking it clears the marker.
    pub(crate) fn take_event_resume(&mut self, task: TaskHandle) -> bool {
        match self.tcbs.resolve_mut(task) {
            Ok(tcb) if tcb.event_blocked => {
                tcb.event_blocked = false;
                true
            }
            _ => false,
        }
    }

    /// Mark that `task` blocked inside an event-group wait, so the call it
    /// is inside resumes rather than restarts.
    pub(crate) fn set_event_resume(&mut self, task: TaskHandle) {
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            tcb.event_blocked = true;
        }
    }

    /// `vTaskPlaceOnEventList`: sorted by priority, then blocked.
    pub(crate) fn place_on_event_list(&mut self, list: ListId, ticks: u64) -> Result<()> {
        let current = self.current;
        let item = Self::event_item(current);
        // The item keeps the value its call set; reading it out only to
        // hand it back made the list read the item twice and write it once
        // for no change.
        self.lists.insert_keeping_value(list, item)?;
        self.add_current_task_to_delayed_list(ticks, true)
    }

    /// Take the trailing `portYIELD()` now, or owe it if a tick already
    /// switched us out of this call.
    pub(crate) fn yield_or_owe(&mut self, caller: TaskHandle) {
        if self.current == caller {
            self.port_yield();
        } else {
            self.owe_yield(caller);
        }
    }

    /// `pvTaskIncrementMutexHeldCount`.
    pub(crate) fn increment_mutexes_held(&mut self, task: TaskHandle) {
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            tcb.mutexes_held = tcb.mutexes_held.wrapping_add(1);
        }
    }

    // ------------------------------------------------ priority inheritance --

    /// `xTaskPriorityInherit`: lift the mutex holder to the waiter's
    /// priority. `true` when the holder is (or already was) lifted, which
    /// is what the waiter remembers so it can undo it on a timeout.
    pub(crate) fn priority_inherit(&mut self, holder: TaskHandle) -> Result<bool> {
        if holder.is_null() || !self.tcbs.contains(holder) {
            return Ok(false);
        }
        let waiter_priority = self.current_priority();
        let (holder_priority, holder_base) = {
            let tcb = self.tcbs.resolve(holder)?;
            (tcb.priority, tcb.base_priority)
        };
        if holder_priority >= waiter_priority {
            // Already at least as urgent; the waiter still records that the
            // holder is running above its base, if it is.
            return Ok(holder_base < waiter_priority);
        }
        let event_value = u64::from(C::MAX_PRIORITIES.saturating_sub(waiter_priority));
        self.lists
            .set_value(Self::event_item(holder), event_value)?;
        let item = Self::state_item(holder);
        if self.lists.container(item)? == Some(Self::ready_list(holder_priority)) {
            let _ = self.lists.remove(item);
            self.tcbs.resolve_mut(holder)?.priority = waiter_priority;
            self.add_task_to_ready_list(holder)?;
        } else {
            self.tcbs.resolve_mut(holder)?.priority = waiter_priority;
        }
        self.trace_task(holder, |task, name| Event::TaskPriorityInherit {
            task,
            name,
            priority: Self::priority_value(waiter_priority),
        });
        Ok(true)
    }

    /// `xTaskPriorityDisinherit`: giving the mutex back drops the
    /// inherited priority. `true` when a yield is wanted.
    pub(crate) fn priority_disinherit(&mut self, holder: TaskHandle) -> Result<bool> {
        if holder.is_null() {
            return Ok(false);
        }
        // One lookup where there were three: `contains` asked the arena
        // whether the holder is there, `resolve` asked again to read its
        // three fields, and `resolve_mut` asked a third time to put one back
        // -- with nothing but a subtraction in between. A holder the arena
        // does not have still answers `Ok(false)`, which is what `contains`
        // was for.
        let (priority, base, remaining) = match self.tcbs.resolve_mut(holder) {
            Ok(tcb) => {
                let remaining = tcb.mutexes_held.saturating_sub(1);
                tcb.mutexes_held = remaining;
                (tcb.priority, tcb.base_priority, remaining)
            }
            Err(_) => return Ok(false),
        };
        if priority == base || remaining != 0 {
            return Ok(false);
        }
        let item = Self::state_item(holder);
        if self.lists.remove(item).is_err() {
            return Ok(false);
        }
        self.trace_task(holder, |task, name| Event::TaskPriorityDisinherit {
            task,
            name,
            priority: Self::priority_value(base),
        });
        {
            let tcb = self.tcbs.resolve_mut(holder)?;
            tcb.priority = base;
        }
        let event_value = u64::from(C::MAX_PRIORITIES.saturating_sub(base));
        self.lists
            .set_value(Self::event_item(holder), event_value)?;
        self.add_task_to_ready_list(holder)?;
        Ok(true)
    }

    /// `vTaskPriorityDisinheritAfterTimeout`: a waiter gave up, so the
    /// holder keeps only what the tasks still waiting justify.
    pub(crate) fn priority_disinherit_after_timeout(
        &mut self,
        holder: TaskHandle,
        highest_waiting: u8,
    ) -> Result<()> {
        if holder.is_null() || !self.tcbs.contains(holder) {
            return Ok(());
        }
        let (priority, base, held) = {
            let tcb = self.tcbs.resolve(holder)?;
            (tcb.priority, tcb.base_priority, tcb.mutexes_held)
        };
        if priority == base || held != 1 {
            return Ok(());
        }
        let target = if highest_waiting > base {
            highest_waiting
        } else {
            base
        };
        if priority == target {
            return Ok(());
        }
        self.trace_task(holder, |task, name| Event::TaskPriorityDisinherit {
            task,
            name,
            priority: Self::priority_value(target),
        });
        self.tcbs.resolve_mut(holder)?.priority = target;
        let event_value = u64::from(C::MAX_PRIORITIES.saturating_sub(target));
        self.lists
            .set_value(Self::event_item(holder), event_value)?;
        let item = Self::state_item(holder);
        if self.lists.container(item)? == Some(Self::ready_list(priority)) {
            let _ = self.lists.remove(item);
            self.add_task_to_ready_list(holder)?;
        }
        Ok(())
    }

    // ------------------------------------------------------- delay until --

    /// `xTaskDelayUntil`: wake at `*previous_wake + period`, whatever the
    /// task did in between. `false` when the deadline has already passed,
    /// in which case nothing blocks — exactly as the C returns `pdFALSE`.
    ///
    /// # Errors
    /// A list error surfaces as itself.
    pub fn delay_until(&mut self, previous_wake: &mut u64, period: u64) -> Result<bool> {
        let caller = self.current;
        let mut should_delay = false;
        self.suspend_all();
        {
            let now = self.tick;
            let wake_at = previous_wake.wrapping_add(period) & Self::MAX_DELAY;
            if now < *previous_wake {
                // The tick count overflowed since the last wake.
                if wake_at < *previous_wake && wake_at > now {
                    should_delay = true;
                }
            } else if wake_at < *previous_wake || wake_at > now {
                should_delay = true;
            }
            *previous_wake = wake_at;
            if should_delay {
                self.trace_task(caller, |task, name| Event::TaskDelayUntil {
                    task,
                    name,
                    wake_at,
                });
                let ticks = wake_at.wrapping_sub(now) & Self::MAX_DELAY;
                self.add_current_task_to_delayed_list(ticks, false)?;
            }
        }
        if !self.resume_all() {
            self.yield_or_owe(caller);
        }
        Ok(should_delay)
    }

    /// `xTaskRemoveFromEventList`; `true` when the woken task outranks the
    /// running one and a yield is therefore required.
    pub(crate) fn remove_from_event_list(&mut self, list: ListId) -> Result<bool> {
        let Some(item) = self.lists.head(list)? else {
            return Ok(false);
        };
        let task = self.task_of_event_item(item)?;
        let _ = self.lists.remove(item);
        if self.suspended_depth == 0 {
            let _ = self.lists.remove(Self::state_item(task));
            self.add_task_to_ready_list(task)?;
        } else {
            self.lists.insert_end(Self::pending_ready_list(), item)?;
        }
        let woken = self.tcbs.resolve(task)?.priority;
        if woken > self.current_priority() {
            self.yield_pending = true;
            return Ok(true);
        }
        Ok(false)
    }
}

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
    StreamBuffer as StreamKind, StreamBufferHandle, Task as TaskKind, TaskHandle,
    Timer as TimerKind, TimerHandle,
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
use crate::timer::{Message, Timer};
use crate::{OVERHEAD_LISTS, list_slots_for, lists_for};

/// How many times [`Kernel::trace_failure_or_owe`] has been entered.
///
/// A reachability anchor, not a measurement. Every row of
/// `riscv32-qemu-tick-work` read **zero** here until the failure rows were
/// added, which is why a change to the `OwedTrace` machinery measured as pure
/// code layout. It is behind its own feature because the `fetch_add` sits
/// INSIDE the measured bracket, so a build that quotes instruction counts must
/// not carry it.
#[cfg(feature = "census")]
pub static CENSUS_OWE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

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
    /// `traceSTREAM_BUFFER_CREATE`, which the C runs after the
    /// `pvPortMalloc` that made the buffer.
    ///
    /// `heap_4` brackets that malloc with `vTaskSuspendAll` /
    /// `xTaskResumeAll`, so a tick can land on its exit -- and the C's
    /// thread then stops inside the allocator, with the trace still ahead
    /// of it. Emitting it anyway puts the line in another task's run of the
    /// trace. `StreamBufferDemo`'s echo client showed it at tick 429, one
    /// line early and a whole task's work out of place.
    StreamBufferCreate {
        buffer: StreamBufferHandle,
        is_message_buffer: bool,
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

/// Where a preempted queue call has to start again.
///
/// A queue call has SEVERAL critical-section exits that can release a tick
/// and switch the caller away, and the C's thread resumes below whichever
/// one it stopped at. One marker is not enough to say which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum QueueResume {
    /// Nothing to resume; enter at the top.
    #[default]
    No,
    /// Below the section that samples the queue, in the caller.
    BelowSample,
    /// Below the `xTaskResumeAll` of the TIMED-OUT branch, which is the one
    /// `IntQueue` reaches at tick 16,260: the tick pended inside the
    /// scheduler suspension is replayed there, and replaying it switches
    /// the caller away with `prvIsQueueEmpty` still to run.
    BelowTimedOutResume,
}

/// One task control block: the C `TCB_t` minus everything that is a
/// pointer. No stack, no TLS, no `pxTopOfStack`.
#[derive(Debug, Clone, Copy, Default)]
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
    /// Where a preempted queue call resumes. See [`QueueResume`].
    ///
    /// `xQueueGenericSend` and `xQueueReceive` exit their sampling section
    /// before suspending the scheduler to block, and THAT exit can release
    /// a tick which switches the caller away. The C's thread stops inside
    /// the exit; everything below it -- `traceBLOCKING_ON_QUEUE_SEND` and
    /// `vTaskPlaceOnEventList` -- runs when the task is next scheduled, and
    /// so names the right task. Without this marker the trace attributes
    /// the block to whoever the tick switched TO, which `IntQueue` catches
    /// at tick 5 and no other scenario reaches.
    queue_resume: QueueResume,
    /// Whether a blocking `xStreamBufferSend` has already run its
    /// `vTaskSetTimeOutState`. See [`Kernel::take_stream_timed`].
    stream_timed: bool,
    /// Whether a stream-buffer call's notify wait has already finished.
    /// See [`Kernel::take_stream_waited`].
    stream_waited: bool,
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
    /// Padding that rounds `Slot<Tcb>` up to a POWER OF TWO (128 bytes), so
    /// `index * size_of::<Slot<Tcb>>()` is a shift and not a multiply.
    ///
    /// `bench/kernel-ram` pins the stride at 128 and `bench/kernel-flash` pins
    /// `mul` at 0; without this the stride is 92 and every TCB resolve pays a
    /// `mul`. It is a deliberate flash-for-RAM trade: 254 bytes of flash
    /// against 36 bytes of RAM per task, so it pays below roughly eight tasks
    /// and is the owner's call above that.
    _stride_pad: [u32; STRIDE_PAD_WORDS],
}

/// How many `u32`s of [`Tcb::_stride_pad`] it takes to make `Slot<Tcb>` a power
/// of two, which is what keeps `index * stride` a shift instead of a multiply.
///
/// It differs by pointer width because the fields it pads out do: `usize` and
/// the `Name` alignment (see [`crate::name::Name`], aligned to eight on a
/// 64-bit host only) both change size. Nine words gives 128 bytes on a 32-bit
/// target -- the number `bench/kernel-ram` pins and `bench/kernel-flash`'s
/// `mul = 0` depends on -- and four gives 128 on the host.
///
/// Before this was split, the host slot was 144 bytes and every TCB index cost
/// a `lea`+`shl` where a power-of-two stride costs one `shl`.
#[cfg(target_pointer_width = "64")]
pub(crate) const STRIDE_PAD_WORDS: usize = 4;

/// See the 64-bit case above; nine words is what makes an rv32 `Slot<Tcb>` 128
/// bytes, and that number is pinned by two benches.
#[cfg(not(target_pointer_width = "64"))]
pub(crate) const STRIDE_PAD_WORDS: usize = 9;

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
    ///
    /// All three are SPLIT (`timer::Split64`), so a `WaitFrame` does not align
    /// a `Tcb` to eight. An arena slot pays its value's alignment as padding,
    /// and `Tcb` is one per task.
    ticks: crate::timer::Split64,
    /// `xTimeOut.xTimeOnEntering`.
    entering: crate::timer::Split64,
    /// `xTimeOut.xOverflowCount`.
    /// Counted in a `u32`: it advances once per delayed-list swap, so four
    /// billion of them is not a bound anything reaches, and a `u64` here was
    /// two instructions an operation on rv32.
    overflows: u32,
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
#[repr(C)]
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
    const TIMER_CMDS: usize,
> {
    /// `xTickCount`, masked to the configuration's tick width.
    pub(crate) tick: u64,
    /// `xPendedTicks`.
    /// Counted in a `u32`, for the reason `overflows` is: it counts ticks
    /// that arrived while the scheduler was suspended, which a suspension
    /// long enough to overflow four billion would have to outlive.
    pended_ticks: u32,
    /// `xNextTaskUnblockTime`.
    next_unblock_time: u64,
    /// `xNumOfOverflows`.
    overflows: u32,
    /// `xLastTime` in `prvSampleTimeNow`.
    pub(crate) timer_last_time: u64,
    /// `pxCurrentTCB`.
    pub(crate) current: TaskHandle,
    /// `uxSchedulerSuspended`.
    suspended_depth: u32,
    /// `uxCurrentNumberOfTasks`.
    task_count: usize,
    /// How many times the scheduler could not choose a task, and why the
    /// FIRST time. See [`Stall`].
    stalls: u32,
    pub(crate) timer_message_next: usize,
    pub(crate) bytes_used: usize,
    pub(crate) free_count: usize,
    pub(crate) free_slot_count: usize,
    pub(crate) slots_used: usize,
    /// The task whose abandoned frame is running right now, if any.
    /// `TaskHandle::NULL` for "nobody", not `Option<TaskHandle>`.
    ///
    /// A `Handle` is two `u16`s with no niche, so the `Option` was eight bytes
    /// and a real discriminant — and `resume_pending` tests this field on
    /// every step the runner takes, which is the most-called entry in the
    /// kernel. `NULL` is already the sentinel (`generation == 0`), so the
    /// representation carries the fact the discriminant was carrying.
    unwinding: TaskHandle,
    /// A task that deleted ITSELF and whose slot the idle task has not
    /// reclaimed yet — the C's `xTasksWaitingTermination`, which only ever
    /// holds the running task, because any other task is freed on the spot.
    ///
    /// One slot is enough, and that is a property rather than a guess: a
    /// self-deleting task is off every ready list and yields immediately, so
    /// a second self-delete cannot happen until a switch has occurred, and
    /// the switch is where this is reaped.
    awaiting_reap: TaskHandle,
    /// `xTimerQueue`.
    pub(crate) timer_queue: QueueHandle,
    /// How many tasks are parked at a queue call's sampling exit.
    ///
    /// A summary, so that [`Kernel::take_queue_resume`] — which runs at the
    /// top of every queue send and every queue receive — can answer "nothing
    /// to resume" from one field instead of resolving the caller's TCB out of
    /// the arena. Only the three sites that park a task raise it, and only a
    /// take that finds a slot set lowers it, so it counts exactly the slots
    /// that are not `QueueResume::No`.
    queue_resumes: u16,
    /// How many tasks are parked inside an event-group wait, for the same
    /// reason [`Kernel::queue_resumes`] exists: `take_event_resume` runs at
    /// the top of every `event_group_wait_bits` and every `event_group_sync`,
    /// and on all but the parked few the answer is no.
    event_resumes: u16,
    /// `uxTopReadyPriority`.
    top_ready_priority: u8,
    /// The running task's scheduling priority, kept beside `current`.
    ///
    /// The C reads `pxCurrentTCB->uxPriority`: one pointer dereference,
    /// because the pointer IS the task. A handle is not a pointer, so the
    /// same read went through `Arena::resolve` — a bounds check, a generation
    /// compare and an `Option` — **every tick**, on the one handle the kernel
    /// itself maintains and cannot have wrong.
    ///
    /// Measured: that resolve was **43 of the tick's 56 instructions, 77 %**.
    /// Stubbing it to a constant took `increment_tick` from 56 to 13 against
    /// the C's 15 — from 3.73x against us to 0.87x. This field is that
    /// finding, done correctly.
    ///
    /// **It is written in exactly two places** — [`Self::set_current`] and
    /// [`Self::set_task_priority`] — so that a call site cannot forget it.
    /// `current_priority` debug-asserts it against the resolved value, so the
    /// kernel's own tests and the conformance corpus fail loudly if it ever
    /// drifts.
    current_priority: u8,
    /// `xSchedulerRunning`.
    running: bool,
    /// `xYieldPendings[0]`.
    yield_pending: bool,
    /// Which of the two delayed lists is `pxDelayedTaskList` right now.
    delayed_swapped: bool,
    /// Which of the two timer lists is `pxCurrentTimerList` right now.
    pub(crate) timers_swapped: bool,
    first_stall: Stall,

    pub(crate) port: P,
    pub(crate) trace: T,
    /// `vApplicationTickHook`, held by value so it can borrow the kernel.
    pub(crate) tick_hook: H,
    /// How many outermost critical-section exits a task owes the clock.
    ///
    /// This is `uxSavedCriticalNesting` in the Posix port's
    /// `prvSwitchThread` and then some. A thread stops at the switch; a
    /// stackless call does not, so the abandoned frame runs on — closing
    /// the sections it had open and, on some paths, opening one more. The
    /// port tallies every exit that frame makes instead of counting it as
    /// sim time, and the tally is paid here when the task runs again. That
    /// is what puts a sim tick where the C one lands.
    /// Five per-task booleans, one bit each, in ONE array.
    ///
    /// They were five `[bool; TASKS]`. Each one is a separate base address, so
    /// a function touching two of them — and the owed body touches three —
    /// computed two bases and did two loads to read two bits that now share a
    /// byte. Packed they cost one byte a task instead of five, and one base
    /// instead of five across the whole kernel.
    flags: [u8; TASKS],
    /// `started`, kept OUT of `flags` on purpose.
    ///
    /// It is the only one of the five that `switch_context` reads — through
    /// `hand_over`, on every switch — and packing it cost the selection row
    /// one instruction for the mask. The other four are never on that path,
    /// so they stay packed. Per-call-site, not per-idea.
    started: [bool; TASKS],
    /// `owes_anything`, kept OUT of `flags` for the reason `started` is: it is
    /// the single most-read byte in the kernel — `resume_pending` tests it
    /// before every step the runner takes — and packing it cost that row three
    /// instructions for the mask. Per call site, not per idea.
    owes_anything: [bool; TASKS],
    /// `delay_aborted`, kept OUT of `flags`: `check_for_timeout` reads it on
    /// every pass of every blocking call.
    delay_aborted: [bool; TASKS],
    pub(crate) lists: Lists<ITEMS, LISTS>,
    pub(crate) tcbs: Arena<TaskKind, Tcb, TASKS>,
    owed_exits: [u32; TASKS],
    /// A trace line a task owes from a call it was switched out of.
    ///
    /// `xQueueReceive`'s failure path is `taskEXIT_CRITICAL();
    /// traceQUEUE_RECEIVE_FAILED( pxQueue ); return errQUEUE_EMPTY;` — and
    /// if the tick that the exit released switches the task away, those two
    /// lines sit on a stack that is not running. The C emits the trace when
    /// the task resumes, so this kernel does too.
    owed_trace: [OwedTrace; TASKS],
    pub(crate) queues: Arena<QueueKind, Queue, QUEUES>,
    pub(crate) timers: Arena<TimerKind, Timer, TIMERS>,
    pub(crate) groups: Arena<EventGroupKind, EventGroup, GROUPS>,
    pub(crate) buffers: Arena<StreamKind, StreamBuffer, BUFFERS>,
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
    /// Extents of the SLOT arena that a deleted queue gave back, as
    /// `(base, length)`, coalesced the same way `free_blocks` is.
    ///
    /// The byte arena has had this since `MessageBufferDemo` needed it. The
    /// slot arena did not, so `queue_delete` returned the descriptor while
    /// the storage stayed spent and a create/delete loop exhausted `SLOTS`
    /// permanently — found through `AbortDelay` on 2026-09-21, after nine
    /// hypotheses and eight refutations, because the descriptor arena works
    /// perfectly and that is where everyone looked.
    ///
    /// At most `QUEUES` queues are alive, so at most `QUEUES` holes can sit
    /// between them and the list cannot overflow.
    pub(crate) free_slots: [(usize, usize); QUEUES],
    /// The ring of `DaemonTaskMessage_t`s the timer queue carries indices
    /// into. It is exactly as long as the queue, so a message can only be
    /// overwritten once the queue has already refused to hold its index.
    /// The timer daemon's command mailbox, sized by the configuration.
    ///
    /// It used to be `[Message; MAX_TIMER_COMMANDS]` — a hardcoded 32 — while
    /// `Config::TIMER_QUEUE_LENGTH` already said how many the configuration
    /// wanted and defaulted to ten. Because it scaled with **nothing**, it was
    /// a fixed 768 bytes at every geometry, which is **39 % of a two-task
    /// kernel's entire static footprint** and 31 % of a four-task one.
    ///
    /// `TIMER_CMDS` is that number, checked against the config in
    /// [`Self::with_tick_hook`] exactly as `ITEMS` and `LISTS` are.
    pub(crate) timer_messages: [Message; TIMER_CMDS],
    pub(crate) slots: [u64; SLOTS],
    pub(crate) bytes: [u8; BYTES],
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
    const TIMER_CMDS: usize,
> Kernel<C, P, T, H, TASKS, ITEMS, LISTS, QUEUES, SLOTS, BUFFERS, BYTES, TIMERS, GROUPS, TIMER_CMDS>
where
    H: TickHook<Self>,
{
    /// `portMAX_DELAY` at this configuration's tick width.
    pub const MAX_DELAY: u64 = <C::Tick as TickWidth>::MAX;

    // ------------------------------------------------------- footprint --

    /// The kernel's own static size, and the same number `size_of` gives.
    ///
    /// The consts below decompose it. They exist because a footprint you
    /// cannot decompose to the byte is a footprint you cannot optimise: the
    /// arena fields are `pub(crate)` and their types are private, so nothing
    /// outside this crate can measure them, and a decomposition assembled
    /// from per-dimension SLOPES is not one — `ITEMS` and `LISTS` are derived
    /// from `TASKS`, `QUEUES` and `GROUPS`, so the function is not linear and
    /// slopes do not sum to the total. That was tried; it produced 696 bytes
    /// of nonsense.
    ///
    /// Every one is `size_of` on whatever target this is compiled for, which
    /// is the only place the number means anything: a host `usize` is eight
    /// bytes and rv32's is four.
    ///
    /// [`FOOTPRINT_ACCOUNTED`](Self::FOOTPRINT_ACCOUNTED) sums them, and the
    /// difference from [`FOOTPRINT`](Self::FOOTPRINT) is padding plus the
    /// scalar tail — print both, never one. `bench/kernel-ram` does.
    pub const FOOTPRINT: usize = core::mem::size_of::<Self>();

    /// The task control blocks.
    pub const FOOTPRINT_TCBS: usize = core::mem::size_of::<Arena<TaskKind, Tcb, TASKS>>();
    /// The queue descriptors, and NOT their item storage.
    pub const FOOTPRINT_QUEUES: usize = core::mem::size_of::<Arena<QueueKind, Queue, QUEUES>>();
    /// Every intrusive list: ready, delayed, and the per-object waiters.
    pub const FOOTPRINT_LISTS: usize = core::mem::size_of::<Lists<ITEMS, LISTS>>();
    /// Queue item storage, as `u64` slots.
    pub const FOOTPRINT_SLOTS: usize = core::mem::size_of::<[u64; SLOTS]>();
    /// The stream-buffer descriptors, and NOT their bytes.
    pub const FOOTPRINT_BUFFERS: usize =
        core::mem::size_of::<Arena<StreamKind, StreamBuffer, BUFFERS>>();
    /// The software timers.
    pub const FOOTPRINT_TIMERS: usize = core::mem::size_of::<Arena<TimerKind, Timer, TIMERS>>();
    /// The event groups.
    pub const FOOTPRINT_GROUPS: usize =
        core::mem::size_of::<Arena<EventGroupKind, EventGroup, GROUPS>>();
    /// The timer daemon's command mailbox.
    ///
    /// It scales with `TIMER_CMDS` now, which the geometry check pins to
    /// `Config::TIMER_QUEUE_LENGTH`. It used to be a hardcoded 32 and so
    /// scaled with nothing — a fixed 768 bytes, 39 % of a two-task kernel.
    /// The line stays separate because that history is worth being able to
    /// read off a decomposition.
    pub const FOOTPRINT_TIMER_MESSAGES: usize = core::mem::size_of::<[Message; TIMER_CMDS]>();
    /// The byte arena, for stream-buffer contents.
    pub const FOOTPRINT_BYTES: usize = core::mem::size_of::<[u8; BYTES]>();
    /// The two free lists, over the byte arena and the slot arena.
    pub const FOOTPRINT_FREE_LISTS: usize = core::mem::size_of::<[(usize, usize); BUFFERS]>()
        + core::mem::size_of::<[(usize, usize); QUEUES]>();

    /// The seven per-task side arrays, together.
    ///
    /// Seven arrays indexed by the same task, five of them `[bool; TASKS]`.
    /// Whether that wants to be one array of a flags byte is a question this
    /// number is here to make askable.
    pub const FOOTPRINT_PER_TASK_SIDE: usize = core::mem::size_of::<[u8; TASKS]>()
        + core::mem::size_of::<[bool; TASKS]>() * 3
        + core::mem::size_of::<[OwedTrace; TASKS]>()
        + core::mem::size_of::<[u32; TASKS]>();

    /// What the lines above account for.
    ///
    /// `FOOTPRINT - FOOTPRINT_ACCOUNTED` is the unattributed remainder: the
    /// scalar tail (`tick`, `current`, the counters) plus whatever padding
    /// the layout inserts. It was 140 bytes of 6,784 when this was written. A
    /// remainder that grows is a field somebody added without adding a line
    /// here.
    pub const FOOTPRINT_ACCOUNTED: usize = Self::FOOTPRINT_TCBS
        + Self::FOOTPRINT_QUEUES
        + Self::FOOTPRINT_LISTS
        + Self::FOOTPRINT_SLOTS
        + Self::FOOTPRINT_BUFFERS
        + Self::FOOTPRINT_TIMERS
        + Self::FOOTPRINT_GROUPS
        + Self::FOOTPRINT_TIMER_MESSAGES
        + Self::FOOTPRINT_BYTES
        + Self::FOOTPRINT_FREE_LISTS
        + Self::FOOTPRINT_PER_TASK_SIDE;

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
        if ITEMS != list_slots_for(TASKS, TIMERS, LISTS)
            || LISTS != lists_for(C::MAX_PRIORITIES, QUEUES, GROUPS)
            || TASKS == 0
            || C::MAX_TASK_NAME_LEN > crate::NAME_CAPACITY
            // The mailbox is a declared dimension now, so a declaration that
            // disagrees with its own config is refused rather than silently
            // sized to whichever one the type happened to carry.
            || TIMER_CMDS != C::TIMER_QUEUE_LENGTH
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
            timer_messages: [Message::default(); TIMER_CMDS],
            timer_message_next: 0,
            timers_swapped: false,
            timer_last_time: 0,
            timer_queue: QueueHandle::NULL,
            bytes: [0; BYTES],
            bytes_used: 0,
            free_blocks: [(0, 0); BUFFERS],
            free_count: 0,
            free_slots: [(0, 0); QUEUES],
            free_slot_count: 0,
            current: TaskHandle::NULL,
            top_ready_priority: 0,
            current_priority: 0,
            tick: C::INITIAL_TICK_COUNT,
            pended_ticks: 0,
            suspended_depth: 0,
            running: false,
            next_unblock_time: Self::MAX_DELAY,
            yield_pending: false,
            task_count: 0,
            stalls: 0,
            first_stall: Stall::None,
            awaiting_reap: TaskHandle::NULL,
            delayed_swapped: false,
            overflows: 0,
            flags: [0; TASKS],
            started: [false; TASKS],
            owes_anything: [false; TASKS],
            delay_aborted: [false; TASKS],
            owed_exits: [0; TASKS],
            unwinding: TaskHandle::NULL,
            queue_resumes: 0,
            event_resumes: 0,
            tick_hook,
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
        self.overflows as u64
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
        task.index() as u16
    }

    /// A task's `xEventListItem`: `TASKS` above its state item, so the two
    /// never collide and either maps back to its task by arithmetic alone.
    pub(crate) fn event_item(task: TaskHandle) -> ItemId {
        Self::task_item_base().saturating_add(task.index() as u16)
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
        self.pended_ticks as u64
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
        // The cache is the whole point; this is the proof it is honest. It
        // costs nothing in release and fails the kernel's own tests, the
        // Kani proofs and the conformance corpus the moment a new write to
        // `current` or to a priority forgets its helper.
        debug_assert_eq!(
            self.current_priority,
            self.priority_of(None).unwrap_or(0),
            "the cached current priority went stale -- a write to `current` or              to a task's priority bypassed set_current/set_task_priority"
        );
        self.current_priority
    }

    /// Make `task` the running task, keeping [`Self::current_priority`] with
    /// it, where the caller already knows the priority.
    ///
    /// One of the two places that field is written; assigning `self.current`
    /// directly is what this exists to prevent.
    ///
    /// **The priority is a parameter and not resolved here**, because every
    /// caller already has it. `switch_context`'s search loop finds the highest
    /// non-empty ready list and takes `next` out of it, so `next`'s priority
    /// IS that list's index; task creation has it as an argument. Resolving
    /// the TCB again to learn what the caller just proved cost **9
    /// instructions on the switch row** — measured, by landing this without
    /// the parameter and watching `switch_select` go 79 → 88, then 88 → 82
    /// when it was threaded through.
    fn set_current_at(&mut self, task: TaskHandle, priority: u8) {
        self.current = task;
        self.current_priority = priority;
    }

    /// Write a task's scheduling priority, keeping [`Self::current_priority`]
    /// with it when that task is the running one.
    ///
    /// The other of the two. Note it is the SCHEDULING priority only:
    /// `base_priority` does not decide which ready list a task sits on, so it
    /// is written directly at its call sites.
    ///
    /// # Errors
    /// [`Error::InvalidHandle`] or [`Error::Gone`] for a handle the arena
    /// refuses, exactly as `resolve_mut` does.
    fn set_task_priority(&mut self, task: TaskHandle, priority: u8) -> Result<()> {
        self.tcbs.resolve_mut(task)?.priority = priority;
        if task == self.current {
            self.current_priority = priority;
        }
        Ok(())
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
    #[cold]
    #[inline(never)]
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
        // `tcbs` and `trace` are different fields, so the name can be read
        // where it lives rather than copied onto the stack to end a borrow
        // that never had to end.
        //
        // A sink that never reads the name pays for neither the lookup nor
        // the UTF-8 validation that turning one into a `&str` costs.
        let Self {
            tcbs,
            trace,
            port,
            tick,
            ..
        } = self;
        let name = if T::WANTS_NAMES {
            tcbs.get(task).map(|t| t.name.as_str()).unwrap_or("")
        } else {
            ""
        };
        let tick = *tick;
        trace.note_exits(port.exits());
        trace.event(tick, make(task, name));
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
    // Out of line: 12 cold create/delete sites, each otherwise inlining
    // suspend_all + resume_all. Worth -46 B, and the Ir cost is nil because
    // every caller is a create or a delete.
    #[inline(never)]
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
    ///
    /// NOT in line, though it is three statements called 118,640 times in
    /// a 20,000-tick `StreamBufferDemo`. `#[inline(always)]` was measured:
    /// it saves that scenario 210,292 and costs `BlockQ` 200,912 and
    /// `GenQTest` 280,540, for a net loss of 271,962 over six scenarios.
    /// Whatever the hot sites gain, the switch-heavy ones lose more.
    pub fn exit_critical(&mut self) {
        self.port.exit_critical();
        if self.port.take_pending_tick() {
            self.tick_on_exit();
        }
    }

    /// The tick as [`Kernel::exit_critical`] reaches it, deliberately out of
    /// line.
    ///
    /// `exit_critical` is inlined into every queue walker and every demo
    /// step, and the tick is its rare arm -- 8,000 of 48,000 sends in
    /// `khot-ir` raise one, and 38,013 ticks across 18 scenarios in
    /// `kernel-ir`. Left in line it put `increment_tick` and
    /// `switch_context` inside all of those frames.
    ///
    /// Taking it out is worth 233,207 on `kernel-ir` and costs the four
    /// kernel micro-instruments 9,650 between them, which is about what one
    /// extra call layer costs at their call counts.
    ///
    /// The other half was refuted: `#[inline(always)]` on
    /// [`Kernel::tick_from_isr`] itself costs `khot-ir` 178,245 while saving
    /// the three lighter instruments 82,642, and stacked on top of this
    /// outlining it still costs `khot-ir` 12,001, `ksched-ir` 7,555 and
    /// `kipc-ir` 2,000 for nothing `kernel-ir` did not already have. So the
    /// body keeps LLVM's own choice and only this call site is pinned.
    #[cold]
    #[inline(never)]
    fn tick_on_exit(&mut self) {
        self.tick_from_isr();
    }

    /// `vPortSystemTickHandler`: the tick, from interrupt context.
    ///
    /// A silicon port calls this from its timer interrupt. The sim reaches
    /// it from [`Kernel::exit_critical`] -- through [`Kernel::tick_on_exit`],
    /// which is where the reason for this attribute is written down -- and
    /// from the idle hook.
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

    // ---- tickless idle --------------------------------------------------

    /// `tskIDLE_PRIORITY`.
    const IDLE_PRIORITY: u8 = 0;

    /// `prvGetExpectedIdleTime`: how long nothing is due, in ticks.
    ///
    /// Zero whenever the answer would be unsafe to sleep on -- something
    /// above the idle priority is running, another idle-priority task is
    /// ready to share the slice, or a higher-priority task is ready. The
    /// last of those cannot happen with preemption on, and the C keeps the
    /// test anyway because a cooperative build reaches it.
    fn expected_idle_time(&self) -> u64 {
        // The C spells these as three arms so each carries its own reason;
        // they answer zero alike, so here they are one short-circuit chain
        // in the same order:
        //
        //  - something above the idle priority is running;
        //  - another idle-priority task is ready to share the slice, so the
        //    very next tick has to be processed;
        //  - a higher-priority task is ready, which only a cooperative
        //    build can reach and which the C tests for anyway.
        let due = self.current_priority() > Self::IDLE_PRIORITY
            || self.ready_len(Self::IDLE_PRIORITY).unwrap_or(0) > 1
            || self.top_ready_priority > Self::IDLE_PRIORITY;

        if due {
            0
        } else {
            self.next_unblock_time.saturating_sub(self.tick)
        }
    }

    /// `vTaskStepTick`: wind the tick count forward over a suppressed period.
    ///
    /// Note this does *not* call the tick hook for each stepped tick, which
    /// is the C's own note and the reason a tickless trace has no heartbeat
    /// in it.
    ///
    /// The C `configASSERT`s that the jump does not pass the next unblock
    /// time. This clamps instead: a port that overslept is a port bug, and
    /// winding the clock past a task's wake would lose the wake entirely,
    /// where losing the extra sleep is recoverable. `forbid(unsafe)` buys
    /// nothing if the kernel panics on a misbehaving port.
    ///
    /// Landing exactly on the next unblock time leaves the last tick pended
    /// rather than stepped, so `increment_tick` runs it when the scheduler
    /// resumes and the delayed task is woken by the same code that would
    /// have woken it.
    pub fn step_tick(&mut self, ticks_to_jump: u64) {
        let room = self.next_unblock_time.saturating_sub(self.tick);
        let mut jump = ticks_to_jump.min(room);

        if self.tick.saturating_add(jump) == self.next_unblock_time && jump > 0 {
            self.pended_ticks = self.pended_ticks.saturating_add(1);
            jump = jump.wrapping_sub(1);
        }

        self.tick = self.tick.wrapping_add(jump) & Self::MAX_DELAY;
    }

    /// The `configUSE_TICKLESS_IDLE` block of `prvIdleTask`.
    ///
    /// Compiled away entirely when the configuration leaves tickless idle
    /// off, which is the default and is why every existing scenario is
    /// byte-identical with this in the tree.
    ///
    /// The shape is the C's, including the part that looks redundant: the
    /// expected idle time is sampled once WITHOUT the scheduler suspended,
    /// and again with it. The first answer is not necessarily valid and the
    /// C takes it anyway, because suspending and resuming the scheduler on
    /// every pass of the idle task costs more than a sample that is
    /// sometimes wasted.
    pub fn idle_suppress_ticks(&mut self) {
        if !C::USE_TICKLESS_IDLE {
            return;
        }

        if self.expected_idle_time() < C::EXPECTED_IDLE_TIME_BEFORE_SLEEP {
            return;
        }

        self.suspend_all();
        let expected = self.expected_idle_time();
        if expected >= C::EXPECTED_IDLE_TIME_BEFORE_SLEEP {
            let tick = self.tick;
            self.note_exits();
            self.trace.event(tick, Event::LowPowerIdleBegin);

            let slept = self.port.suppress_ticks_and_sleep(expected);
            self.step_tick(slept);

            let tick = self.tick;
            self.note_exits();
            self.trace.event(tick, Event::LowPowerIdleEnd);
        }
        let _ = self.resume_all();
    }

    /// `portYIELD()` as the Posix port spells it: a critical section around
    /// the context switch, so a tick raised during it lands on the way out.
    /// `#[cold]` because the call is GUARDED and reached from many sites on
    /// one hot path, which is the shape that pays: it lets LLVM keep the
    /// caller's frame setup out of the likely route. Measured on
    /// `riscv32-qemu-tick-work`, one attribute at a time.
    /// Worth queue -7, group -3 on its own.
    #[cold]
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
    /// Like the C, **the new task becomes current if its priority is at or
    /// above the running one's**. On a hosted firmware that is the sharp
    /// edge: the CPU is still on the caller's stack, so creating a
    /// higher-priority task hands `current` to something that is not
    /// running. See [`Kernel::start_scheduler`], which creates the timer
    /// daemon at [`Config::TIMER_TASK_PRIORITY`] for exactly this reason and
    /// documents the failure it produces — a clean run reporting zero laps.
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
            queue_resume: QueueResume::No,
            stream_timed: false,
            stream_waited: false,
            stream_local: 0,
            _stride_pad: [0; STRIDE_PAD_WORDS],
        };
        let handle = match self.tcbs.try_insert(tcb) {
            Ok(h) => h,
            Err(_) => {
                let tick = self.tick;
                self.note_exits();
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
            if let Some(slot) = self.owed_trace.get_mut(caller.index() as usize) {
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
                self.set_current_at(task, priority);
            } else if !self.running {
                // `<=`, so the last-created task of the highest priority is
                // the one that runs first.
                if self.current_priority() <= priority {
                    self.set_current_at(task, priority);
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
    /// Returns `Result<()>`, and that is a measured decision, not an
    /// oversight.
    ///
    /// Its two hot callers -- the tick's wake loop and the pending-ready
    /// drain -- resolve the SAME TCB again on the next line for the SAME
    /// field, to ask whether the woken task outranks the running one. Having
    /// this hand the priority back instead costs **+3.69% on kdelay-ir and
    /// +3.10% on ksched-ir**.
    ///
    /// Wrapping it -- narrow signature for the eleven callers that ignore the
    /// value, a wide inner for the two that want it -- does NOT help: that
    /// measured bit-for-bit identical to the unwrapped version on all four
    /// arms, because LLVM inlines through the wrapper and propagates the wide
    /// return anyway.
    ///
    /// The likely mechanism is the tail call. At `Result<()>` this function
    /// ends by handing back `insert_end`'s own result; at `Result<u8>` it has
    /// to capture that result, test it, and build a new one, at every site.
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
    /// # ★ The timer daemon is created at [`Config::TIMER_TASK_PRIORITY`],
    /// and it will outrank a task you created below it
    ///
    /// This is the C's behaviour and it is deliberate, but on a hosted
    /// firmware it has a failure mode that looks like nothing at all.
    ///
    /// `Tmr Svc` is created here **unconditionally**, and
    /// [`Kernel::create_task`] makes the highest-priority task current. So a
    /// firmware whose own `main` runs below `TIMER_TASK_PRIORITY` leaves the
    /// kernel believing a **stackless** task is running: `current` names the
    /// daemon while the CPU is on `main`'s stack. Every
    /// [`Kernel::switch_context`] then declines as `from == to`, the workers
    /// never start, and the symptom is a clean run reporting **zero laps**
    /// rather than a fault.
    ///
    /// Nothing here can detect it — a stackless kernel cannot see whose stack
    /// the CPU is actually on — so it is a caller's invariant:
    ///
    /// > **Give the task that owns the CPU a priority at or above
    /// > [`Config::TIMER_TASK_PRIORITY`]**, or lower `TIMER_TASK_PRIORITY`
    /// > below it.
    ///
    /// Reported from the Janus side, where it cost a board run
    /// (`docs/plans/janus-rtos.md` §5b), and seen again in the radio cell.
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
        self.note_exits();
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
        //
        // ONE call per priority level, not two. The search used to ask
        // `is_empty` — a bounded read of the list's metadata — and then hand
        // the winning level to `next_round_robin`, which reads that same
        // metadata again to find the cursor. `next_round_robin` already
        // answers `None` for an empty list, and does so BEFORE it touches the
        // cursor, so it is its own emptiness probe and the level that loses
        // is left exactly as it was found.
        let mut top = self.top_ready_priority;
        // â˜… `Err` is folded into "that level is empty" below, and the fold IS
        // the win: with three arms the walk carried a packed
        // `Result<Option<ItemId>>` across the back edge -- a `setb`/`shl`/`or`
        // to build it and a `test`/`jne` to take it apart, on every one of the
        // 1.58 levels a call walks. Two arms took this function from 168.0 to
        // 157.7 Ir per call, -937,965 on `bench/kernel-ir`, all of it here.
        //
        // But an out-of-range list id is a different fault from an empty level,
        // and the fold alone would have reported it as `NoReadyTask`. So it is
        // proved HERE instead, once, where it can still name itself:
        // `ready_list` is the identity, so the only way the walk can index out
        // of range is a `top_ready_priority` above the configured ceiling.
        // That is the diagnostic bought back, and the cost of buying it is in
        // the commit message beside the fold's.
        if top >= C::MAX_PRIORITIES {
            self.note_stall(Stall::ListError);
            return;
        }
        let item = loop {
            match self.lists.next_round_robin(Self::ready_list(top)) {
                Ok(Some(item)) => break item,
                Ok(None) | Err(_) => {
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
            }
        };
        let Ok(next) = self.task_of_state_item(item) else {
            self.note_stall(Stall::UnknownTask);
            return;
        };
        self.top_ready_priority = top;
        if next != current {
            self.hand_over(current, next);
        }
        // `top` is `next`'s priority by construction: the loop above found
        // the highest non-empty ready list and `next` came out of it.
        self.set_current_at(next, top);
        self.trace_task(next, |task, name| Event::TaskSwitchedIn { task, name });
    }

    // ----------------------------------------------------------- the tick --

    /// Pay whatever the running task still owes from a call it was
    /// preempted inside. The runner calls this before stepping a body;
    /// `true` means work was done and the current task may have changed
    /// again, so nothing else should be assumed.
    #[inline(always)]
    pub fn resume_pending(&mut self) -> bool {
        // The fast path, in line at the runner's one call site.
        //
        // `Runner::step_once` asks this before every step -- 214,488 times
        // in a 20,000-tick `StreamBufferDemo` -- and the answer is "nothing
        // owed" almost every time. Two loads and two branches decide it,
        // and out of line they cost a call and a frame as well.
        //
        // The two tests are the entry conditions of the two things the
        // body does: `settle_unwind` returns at once unless `unwinding` is
        // set, and the rest returns at once unless this task owes
        // something. Neither is duplicated -- the cold half still runs
        // both in full.
        if self.unwinding.is_null() {
            // Nothing to settle, which is the shape of almost every call.
            // `settle_unwind` would test `unwinding` and return at once, and
            // the cold half would then re-read the very byte this line reads
            // — so on this shape both are skipped and the owed body is
            // entered directly.
            let index = self.current.index() as usize;
            if !self.owes_anything.get(index).copied().unwrap_or(true) {
                return false;
            }
            return self.resume_pending_owed(index);
        }
        self.resume_pending_cold()
    }

    /// Everything [`Kernel::resume_pending`] does when something IS owed.
    ///
    /// Out of line on purpose: it is reached on a small fraction of steps,
    /// and inlining it would put the whole `OwedTrace` match at the hot
    /// site. Same body/symbol split the trace sink's `num` measured.
    #[inline(never)]
    fn resume_pending_cold(&mut self) -> bool {
        self.settle_unwind();
        let index = self.current.index() as usize;

        // One load and one test for the overwhelmingly common case. The
        // combined check below is the fallback, and it is what clears the hint
        // -- so a hint left standing costs one slow path and then goes away.
        //
        // This read cannot be threaded in from `resume_pending`: settling an
        // unwind calls `owe` itself, so the answer here is not always the
        // answer there. That is exactly why the no-unwind shape gets its own
        // entry rather than this one being made cheaper.
        if !self.owes_anything.get(index).copied().unwrap_or(true) {
            return false;
        }
        self.resume_pending_owed(index)
    }

    /// Everything owed to the current task, once it is known that something
    /// is.
    ///
    /// Its own symbol so that both shapes above can reach it without either
    /// paying for the other's entry conditions.
    #[inline(never)]
    fn resume_pending_owed(&mut self, index: usize) -> bool {
        let did = self.resume_pending_owed_inner(index);
        if did {
            // Every branch below that did work returned with the hint still
            // standing, so the runner came straight back and spent a second
            // entry re-deriving "nothing owed". Clearing it here covers all
            // of them at one site.
            self.clear_owe_if_settled(index);
        }
        did
    }

    /// The body of [`Kernel::resume_pending_owed`].
    fn resume_pending_owed_inner(&mut self, index: usize) -> bool {
        // Almost every call answers "nothing owed", and the sequence below
        // reaches that answer in three stages -- a compare, a match over
        // `OwedTrace`, then another compare -- each reachable only after the
        // one before. The same three loads, but one branch instead of three.
        // No `.copied()` on `owed_trace`: that materialises an
        // `Option<OwedTrace>`, and `OwedTrace` is forty bytes wide because of
        // `TimerCommandSend`'s `Name`. Matching through the reference tests
        // the discriminant where the copy moved the whole variant to test it.
        if self.owed_exits.get(index).copied().unwrap_or(0) == 0
            && matches!(self.owed_trace.get(index), Some(OwedTrace::None) | None)
            && !self.flag(index, Self::F_YIELD, false)
        {
            if let Some(f) = self.owes_anything.get_mut(index) {
                *f = false;
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
                    *slot = crate::kernel::OwedTrace::None;
                }
                let tick = self.tick;
                self.note_exits();
                self.trace
                    .event(tick, Event::QueueSendFailed { queue, name: "" });
                return true;
            }
            Some(OwedTrace::ReceiveFailed(queue)) => {
                if let Some(slot) = self.owed_trace.get_mut(index) {
                    *slot = crate::kernel::OwedTrace::None;
                }
                let tick = self.tick;
                self.note_exits();
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
                    *slot = crate::kernel::OwedTrace::None;
                }
                let tick = self.tick;
                self.note_exits();
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
            Some(OwedTrace::StreamBufferCreate {
                buffer,
                is_message_buffer,
            }) => {
                if let Some(slot) = self.owed_trace.get_mut(index) {
                    *slot = crate::kernel::OwedTrace::None;
                }
                let tick = self.tick;
                self.note_exits();
                self.trace.event(
                    tick,
                    Event::StreamBufferCreate {
                        buffer,
                        is_message_buffer,
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
                    *slot = crate::kernel::OwedTrace::None;
                }
                let tick = self.tick;
                self.note_exits();
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
                    *slot = crate::kernel::OwedTrace::None;
                }
                let _ = self.add_new_task_to_ready_list(task, priority);
                return true;
            }
            Some(OwedTrace::None) | None => {}
        }
        if !self.flag(index, Self::F_YIELD, false) {
            return false;
        }
        self.set_flag(index, Self::F_YIELD, false);
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
        if self.unwinding.is_null() {
            return;
        }
        let task = core::mem::replace(&mut self.unwinding, TaskHandle::NULL);
        if task.is_null() {
            return;
        };
        let owed = self.port.end_unwind();
        if let Some(slot) = self.owed_exits.get_mut(task.index() as usize) {
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
    /// In line on purpose, at all seven call sites. It is reached 46,671
    /// times in kernel-ir and every one of them paid a call and a frame to
    /// choose between two events the caller had already decided between.
    #[inline(always)]
    pub(crate) fn trace_failure_or_owe(&mut self, caller: TaskHandle, owed: OwedTrace) {
        #[cfg(feature = "census")]
        CENSUS_OWE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // Unlike every other trace site, this one has a side effect a no-op
        // sink cannot delete: the miss path STORES an `OwedTrace` and raises
        // `owes_anything`, which sends the task down the owed body on its next
        // run to reproduce a line the sink will drop.
        if !T::EMITS {
            return;
        }
        if self.current == caller {
            let tick = self.tick;
            self.note_exits();
            match owed {
                OwedTrace::SendFailed(queue) => self
                    .trace
                    .event(tick, Event::QueueSendFailed { queue, name: "" }),
                OwedTrace::ReceiveFailed(queue) => self
                    .trace
                    .event(tick, Event::QueueReceiveFailed { queue, name: "" }),
                OwedTrace::StreamBufferCreate {
                    buffer,
                    is_message_buffer,
                } => self.trace.event(
                    tick,
                    Event::StreamBufferCreate {
                        buffer,
                        is_message_buffer,
                    },
                ),
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
        if let Some(slot) = self.owed_trace.get_mut(caller.index() as usize) {
            *slot = owed;
        }
        self.owe(caller);
    }

    /// Raise the hint that `task` owes something.
    ///
    /// Called by every writer of `owed_trace`, `owes_yield` and `owed_exits`.
    /// Missing one would let `resume_pending` skip work that was really owed,
    /// which the conformance differential would show as a missing trace line.
    /// `owes_yield`: a `portYIELD()` the task was preempted before reaching.
    const F_YIELD: u8 = 1 << 1;
    /// `wait_set`: a blocking call's `WaitFrame` is live in the TCB.
    const F_WAIT: u8 = 1 << 2;

    /// Read one per-task flag.
    ///
    /// `missing` is what an index past the end answers. Each caller chooses it
    /// so an impossible index takes the SLOW path rather than the fast one,
    /// which is what the five separate arrays did with `unwrap_or`.
    #[inline(always)]
    fn flag(&self, index: usize, bit: u8, missing: bool) -> bool {
        match self.flags.get(index) {
            Some(f) => f & bit != 0,
            None => missing,
        }
    }

    /// Write one per-task flag. An index past the end writes nothing, as
    /// `get_mut` did.
    #[inline(always)]
    fn set_flag(&mut self, index: usize, bit: u8, on: bool) {
        if let Some(f) = self.flags.get_mut(index) {
            if on {
                *f |= bit;
            } else {
                *f &= !bit;
            }
        }
    }

    /// Drop the hint if nothing is owed any more.
    ///
    /// Worth one whole entry to [`Kernel::resume_pending_owed`] per owed item.
    /// A census over three scenarios found every raise producing exactly TWO
    /// entries — one that did the work and returned `true`, and a second that
    /// re-derived "nothing owed" and cleared the hint on its way out (BlockQ:
    /// 43,110 raises, 86,207 entries, 43,103 of them finding nothing). Doing
    /// it at the end of the branch that did the work turns that second entry
    /// into a `resume_pending` fast path, which is a few instructions instead
    /// of forty-odd.
    ///
    /// This cannot clear a live hint. Everything that opens new debt raises
    /// the hint itself — a replay that gets switched out leaves its tail to
    /// `settle_unwind`, which raises it again when it collects the tally.
    fn clear_owe_if_settled(&mut self, index: usize) {
        if self.owed_exits.get(index).copied().unwrap_or(0) == 0
            && matches!(self.owed_trace.get(index), Some(OwedTrace::None) | None)
            && !self.flag(index, Self::F_YIELD, false)
        {
            if let Some(f) = self.owes_anything.get_mut(index) {
                *f = false;
            }
        }
    }

    fn owe(&mut self, task: TaskHandle) {
        if let Some(f) = self.owes_anything.get_mut(task.index() as usize) {
            *f = true;
        }
    }

    /// Record that `task` was preempted before the `portYIELD()` at the end
    /// of the call it is inside.
    pub(crate) fn owe_yield(&mut self, task: TaskHandle) {
        self.set_flag(task.index() as usize, Self::F_YIELD, true);
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
        if self.unwinding.is_null() {
            self.unwinding = outgoing;
            self.port.begin_unwind();
        }
        let index = incoming.index() as usize;
        // ONE access to the byte, not two: the test and the two bit writes all
        // work on the same `&mut`. STARTED goes on and YIELD goes off together,
        // which is the whole point of their sharing a byte.
        let first = match self.started.get_mut(index) {
            Some(f) if !*f => {
                *f = true;
                true
            }
            _ => false,
        };
        if first {
            self.set_flag(index, Self::F_YIELD, false);
            if let Some(slot) = self.owed_exits.get_mut(index) {
                *slot = 0;
            }
            if let Some(slot) = self.owed_trace.get_mut(index) {
                *slot = crate::kernel::OwedTrace::None;
            }
        }
    }

    /// Mark a task as having run, without a hand-over: the first task the
    /// scheduler starts has no predecessor.
    fn note_first_start(&mut self, task: TaskHandle) {
        if let Some(f) = self.started.get_mut(task.index() as usize) {
            *f = true;
        }
    }

    /// `xTaskIncrementTick`; `true` when a context switch is required.
    pub fn increment_tick(&mut self) -> bool {
        let tick = self.tick;
        self.note_exits();
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
                self.note_exits();
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
        self.note_exits();
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
                self.note_exits();
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
        self.note_exits();
        self.trace_task(caller, |task, name| Event::TaskNotifyTake {
            task,
            name,
            index,
        });
        // One resolve for the read and both writes. `notified_value_of`
        // resolved the TCB to read the value and this line resolved it again
        // to write it back -- the same trade `unlock_queue` makes, and a
        // `&mut self` call sits between this and the read at the top of the
        // function, so the two cannot be folded by the compiler.
        let mut value = 0;
        if let Ok(tcb) = self.tcbs.resolve_mut(caller) {
            value = tcb.notified.get(index).copied().unwrap_or(0);
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
                .is_ok_and(|tcb| tcb.name.matches(name))
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

    /// `xTaskGenericNotify` with its `pulPreviousNotificationValue`
    /// out-parameter filled: `xTaskNotifyAndQuery`.
    ///
    /// It exists because the answer has to come out of the SAME critical
    /// section as the notification. Reading [`Kernel::notify_value`] first
    /// and then notifying yields the same two numbers and costs a second
    /// critical section -- and on the sim a critical-section exit is a
    /// tick opportunity, so the extra one moves the tick and the trace
    /// stops matching the C. `TaskNotify.c` calls this eleven times.
    ///
    /// # Errors
    /// As [`Kernel::notify`].
    pub fn notify_and_query(
        &mut self,
        task: TaskHandle,
        index: usize,
        value: u32,
        action: NotifyAction,
    ) -> Result<(bool, u32)> {
        if index >= C::NOTIFICATION_ARRAY_ENTRIES {
            return Err(Error::InvalidArgument);
        }
        self.enter_critical();
        let previous = self
            .tcbs
            .resolve(task)
            .map(|tcb| tcb.notified.get(index).copied().unwrap_or(0));
        let result = self.notify_locked(task, index, value, action, false);
        self.exit_critical();
        Ok((result?.0, previous?))
    }

    /// `xTaskGenericNotifyAndQueryFromISR`.
    ///
    /// # Errors
    /// As [`Kernel::notify`].
    pub fn notify_and_query_from_isr(
        &mut self,
        task: TaskHandle,
        index: usize,
        value: u32,
        action: NotifyAction,
    ) -> Result<(bool, u32, Woken)> {
        if index >= C::NOTIFICATION_ARRAY_ENTRIES {
            return Err(Error::InvalidArgument);
        }
        let mask = self.port.enter_critical_from_isr();
        let previous = self
            .tcbs
            .resolve(task)
            .map(|tcb| tcb.notified.get(index).copied().unwrap_or(0));
        let result = self.notify_locked(task, index, value, action, true);
        self.port.exit_critical_from_isr(mask);
        let (ok, woken) = result?;
        Ok((ok, previous?, if woken { Woken::YES } else { Woken::NO }))
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
            self.note_exits();
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

    /// Whether `task` has a queue call to resume below its sampling exit.
    /// Taking it clears the marker.
    pub(crate) fn take_queue_resume(&mut self, task: TaskHandle) -> QueueResume {
        // Nobody is parked at a sampling exit, so there is nothing to read.
        // This is the answer on all but a handful of queue calls, and it costs
        // one field load where resolving the TCB costs an arena lookup.
        if self.queue_resumes == 0 {
            return QueueResume::No;
        }
        let Ok(tcb) = self.tcbs.resolve_mut(task) else {
            return QueueResume::No;
        };
        let resume = tcb.queue_resume;
        // Peek before clearing, the same trade `settle_unwind` makes: every
        // queue send and every queue receive passes through here, and on all
        // but the handful that were preempted at a sampling exit the slot is
        // already `No` — so the unconditional write was a store, on every
        // queue call, to clear something already clear.
        if resume != QueueResume::No {
            tcb.queue_resume = QueueResume::No;
            self.queue_resumes = self.queue_resumes.saturating_sub(1);
        }
        resume
    }

    /// Remember that a queue call was preempted at its sampling exit.
    ///
    /// No local travels with it, unlike [`Kernel::set_stream_resume`]: the C
    /// re-reads the queue under `prvLockQueue` on the other side of the
    /// exit, so there is nothing sampled to carry.
    pub(crate) fn set_queue_resume(&mut self, task: TaskHandle, at: QueueResume) {
        debug_assert_ne!(
            at,
            QueueResume::No,
            "set_queue_resume only ever parks a task; clearing is take's job, and the summary count depends on that"
        );
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            // Count the slot, not the call: re-parking a task already parked
            // replaces its slot and must not be counted twice.
            if tcb.queue_resume == QueueResume::No {
                self.queue_resumes = self.queue_resumes.wrapping_add(1);
            }
            tcb.queue_resume = at;
        }
    }

    /// Has this task's blocking send already paid for `vTaskSetTimeOutState`?
    ///
    /// A one-shot, like [`Kernel::take_stream_resume`], and for the same
    /// reason: `vTaskSetTimeOutState`'s own exit can release a tick and
    /// switch the caller away, and the C's thread then resumes **below** it,
    /// in the sampling loop. Running it again on re-entry would spend a
    /// second exit the C never spends -- which is a sixteenth of a tick, and
    /// `StreamBufferDemo`'s echo client hit it at tick 419.
    pub(crate) fn take_stream_timed(&mut self, task: TaskHandle) -> bool {
        let Ok(tcb) = self.tcbs.resolve_mut(task) else {
            return false;
        };
        let was = tcb.stream_timed;
        tcb.stream_timed = false;
        was
    }

    /// Remember that the timeout state is set and must not be set again.
    pub(crate) fn set_stream_timed(&mut self, task: TaskHandle) {
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            tcb.stream_timed = true;
        }
    }

    /// Has this task's stream-buffer call already finished its wait?
    ///
    /// The other half of [`Kernel::take_stream_resume`]. A call can be
    /// preempted at two different exits, and the two resume in different
    /// places: at the SAMPLING exit the C goes on to block, and at the
    /// WAIT's own exit it goes on to move the bytes and trace them. Without
    /// this the second case is indistinguishable from the first, and a call
    /// that had already waited would wait again.
    pub(crate) fn take_stream_waited(&mut self, task: TaskHandle) -> bool {
        let Ok(tcb) = self.tcbs.resolve_mut(task) else {
            return false;
        };
        let was = tcb.stream_waited;
        tcb.stream_waited = false;
        was
    }

    /// Remember that the wait is done and only the transfer is left.
    pub(crate) fn set_stream_waited(&mut self, task: TaskHandle) {
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            tcb.stream_waited = true;
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
        // Hoisted: see the note at the bottom of the loop.
        let running = if C::USE_PREEMPTION {
            self.current_priority()
        } else {
            0
        };
        loop {
            let delayed = self.delayed_list();
            if self.lists.is_empty_of(delayed) {
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
            // `!switch_required` FIRST, and the running priority read once
            // for the whole drain.
            //
            // Both of these cost a TCB resolve -- a generation check, a
            // bounds check and an `Option` -- and this loop runs once per
            // task the tick wakes. `self.current` cannot move while the
            // drain is running (nothing here switches), so the running
            // priority is a loop invariant that was being re-derived on
            // every lap. And once a switch is already required, asking again
            // can only set a `true` to `true`, which is not worth the
            // resolve it costs -- the same reasoning `increment_tick`
            // already applies one level up.
            if C::USE_PREEMPTION && !switch_required {
                let woken = self.tcbs.resolve(task).map(|t| t.priority).unwrap_or(0);
                if woken > running {
                    switch_required = true;
                }
            }
        }
    }

    /// `taskSWITCH_DELAYED_LISTS()`.
    // Out of line for the same reason: the overflow swap happens when the
    // tick count wraps, which is once in a very long while.
    // In line at its ONE caller (A3): win 26. It carried #[inline(never)] AND a
    // later #[inline] together, so the A3 change was dead from the day it was
    // written -- and rustc said so on every build.
    #[inline(always)]
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
                if let Some(f) = self.delay_aborted.get_mut(task.index() as usize) {
                    *f = true;
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
        if let Some(f) = self.delay_aborted.get_mut(current.index() as usize) {
            *f = false;
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
            // ---- append, when the wake time says we may ------------------
            //
            // `vListInsert` walks from the head until it finds a larger
            // value, so a delayed list `n` deep costs `n` steps -- and this
            // function is the single largest consumer of the blocking
            // workload, most of it that walk.
            //
            // The walk is almost always finding the same answer: the END.
            // `wake_at` is `now + ticks` and `now` only increases, so tasks
            // blocking with comparable timeouts produce ASCENDING wake times
            // and belong after everything already there.
            //
            // `insert_sorted` takes that shortcut, and it asks this caller
            // for exactly ONE thing: that the list is already sorted. The
            // delayed list is, because it is only ever built by sorted
            // inserts and by this append -- so its last item carries its
            // largest value.
            //
            // The other two promises this call site used to make are gone
            // into the list, where they are written once instead of at
            // every future call site: the `>=` tie rule, and the fact that
            // `insert_end` only appends while the cursor is at the marker.
            // Both were silent when wrong. See `ListsOf::insert_sorted`,
            // and `ListsOf::is_sorted` for checking the one that is left.
            self.lists.insert_sorted(list, item, wake_at)?;
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
            self.awaiting_reap = target;
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
        if let Some(f) = self.started.get_mut(task.index() as usize) {
            *f = false;
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
        let task = self.awaiting_reap;
        if task.is_null() {
            return;
        };
        // The C takes the section per reaped task, decrements both counts
        // inside it, and calls `prvDeleteTCB` outside.
        self.enter_critical();
        self.awaiting_reap = TaskHandle::NULL;
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
                    let tcb = self.tcbs.resolve(target)?;
                    let raise = tcb.base_priority == tcb.priority || new_priority > tcb.priority;
                    if raise {
                        self.set_task_priority(target, new_priority)?;
                    }
                    self.tcbs.resolve_mut(target)?.base_priority = new_priority;
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
    ///
    /// **The GUARD is inlined and only the WALK is out of line.** Outlining
    /// the whole thing — which is what this used to do — meant every call paid
    /// a call and a frame to discover there was nothing to do, on a path taken
    /// three times inside `queue_send_blocking` and three more inside
    /// `queue_take_blocking`. Asking "is the list empty" is one list read; the
    /// walk behind it is what wanted outlining.
    #[inline(always)]
    fn drain_pending_ready(&mut self) {
        if self.lists.is_empty_of(Self::pending_ready_list()) {
            // Empty, or a list error: either way the walk below moves nothing,
            // so `moved_any` would stay false and the reset would not happen.
            return;
        }
        self.drain_pending_ready_walk();
    }

    /// The walk itself. See [`Kernel::drain_pending_ready`].
    #[cold]
    #[inline(never)]
    fn drain_pending_ready_walk(&mut self) {
        let mut moved_any = false;
        while !self.lists.is_empty_of(Self::pending_ready_list()) {
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
    /// on essentially every call — and, as there, **only the loop is out of
    /// line now**. The guard is a field read, and it also skips the
    /// `pended_ticks = 0` store that the zero case was writing over a zero.
    #[inline(always)]
    fn unwind_pended_ticks(&mut self) {
        if self.pended_ticks == 0 {
            return;
        }
        self.unwind_pended_ticks_loop();
    }

    /// The loop itself. See [`Kernel::unwind_pended_ticks`].
    #[cold]
    #[inline(never)]
    fn unwind_pended_ticks_loop(&mut self) {
        let mut pended = self.pended_ticks;
        while pended > 0 {
            if self.increment_tick() {
                self.yield_pending = true;
            }
            pended = pended.saturating_sub(1);
        }
        self.pended_ticks = 0;
    }

    /// Hand the port's critical-section exit tally to the trace sink.
    ///
    /// Gated on `T::EMITS` because `Port::exits` reads an ATOMIC counter, and
    /// LLVM may not delete an atomic load even when the value it produces is
    /// unused. With `NoTrace` the body folds to nothing while the `lw` behind it
    /// would stay, so every site was paying a load of a counter no sink was
    /// going to read. The gate is a compile-time constant, so an emitting sink
    /// is unchanged and a silent one loses the load entirely.
    #[inline(always)]
    pub(crate) fn note_exits(&mut self) {
        if T::EMITS {
            self.trace.note_exits(self.port.exits());
        }
    }

    /// `xTaskResumeAll`; `true` when it yielded on the caller's behalf.
    ///
    /// Out of line for the cold callers; the body is [`Kernel::resume_all_inline`].
    #[inline(never)]
    pub fn resume_all(&mut self) -> bool {
        self.resume_all_inline()
    }

    /// The body of [`Kernel::resume_all`], in line for the hot callers.
    #[inline(always)]
    pub(crate) fn resume_all_inline(&mut self) -> bool {
        let mut already_yielded = false;
        self.enter_critical();
        {
            self.suspended_depth = self.suspended_depth.saturating_sub(1);
            if self.suspended_depth == 0 && self.task_count > 0 {
                if !self.lists.is_empty_of(Self::pending_ready_list()) {
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
    ///
    /// Answers the block time that is LEFT. Both call sites wanted it right
    /// afterwards and used to resolve the same TCB a second time to read a
    /// field this function is already holding a `&mut` to. The answer is exact
    /// rather than merely equal to `ticks`, because the frame may be an earlier
    /// pass's — in which case the remaining time is what the kernel has been
    /// counting down, not what the caller passed.
    ///
    /// `None` means no frame was written at all, so the caller can skip the
    /// `end_wait` that would tear one down.
    pub(crate) fn begin_wait(
        &mut self,
        task: TaskHandle,
        queue: QueueHandle,
        ticks: u64,
    ) -> Option<u64> {
        // The zero-block-time exit, taken before anything is read. `wait_set`
        // mirrors the TCB's `entry_set` in one byte, which is the same trade
        // `end_wait` documents and makes — so the frame can be ruled out
        // without an arena lookup, and without reading the clock either.
        // Out of range reads as SET, so an impossible index still goes the
        // long way round.
        if ticks == 0 && !self.flag(task.index() as usize, Self::F_WAIT, true) {
            // `None`, not `Some(0)`: no frame was written, so the caller can
            // skip the `end_wait` that would tear one down. It reads the same
            // `wait_set` byte to find nothing to do.
            return None;
        }
        let tick = self.tick;
        let overflows = self.overflows;
        let Ok(tcb) = self.tcbs.resolve_mut(task) else {
            // A queue call made before the scheduler started, from what the
            // C would call `main()`: there is no task to keep a frame for,
            // and such a call never blocks — `xSemaphoreGive` on a fresh
            // semaphore is the usual one. The old read answered 0 here
            // (`unwrap_or(0)` on the same failed resolve), so 0 is what this
            // must answer to keep the call sites reading the same.
            return Some(0);
        };
        if tcb.wait.queue != queue || !tcb.wait.entry_set {
            tcb.wait = WaitFrame {
                queue,
                ticks: crate::timer::Split64::new(ticks),
                entering: crate::timer::Split64::new(tick),
                overflows,
                entry_set: true,
                inherited: false,
            };
        }
        let remaining = tcb.wait.ticks.get();
        // The frame is set either way on this path -- either it was written
        // just now, or it was already this queue's.
        self.set_flag(task.index() as usize, Self::F_WAIT, true);
        Some(remaining)
    }

    /// The call finished, one way or the other.
    pub(crate) fn end_wait(&mut self, task: TaskHandle) {
        // Almost every call arrives with no frame to clear, and the mirror
        // says so in one byte where resolving the TCB to read `entry_set`
        // cost an arena lookup. Out of range reads as SET, so an impossible
        // index still takes the slow path below.
        let at = task.index() as usize;
        if !self.flag(at, Self::F_WAIT, true) {
            return;
        }
        self.set_flag(at, Self::F_WAIT, false);
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
    /// `None` is `pdTRUE` — the block time has run out. `Some(left)` is
    /// `pdFALSE`, and carries what is left of it.
    ///
    /// The block time is threaded out because both call sites asked for it
    /// immediately afterwards by resolving the TCB again, which
    /// the same TCB out of the arena again to read the very field the match
    /// below has just written.
    pub(crate) fn check_for_timeout(&mut self, task: TaskHandle) -> Option<u64> {
        self.enter_critical();
        // An aborted delay is not the same as a time out, but it has the
        // same result: stop waiting.
        if self
            .delay_aborted
            .get(task.index() as usize)
            .copied()
            .unwrap_or(false)
        {
            if let Some(f) = self.delay_aborted.get_mut(task.index() as usize) {
                *f = false;
            }
            self.exit_critical();
            return None;
        }
        let now = self.tick;
        let overflows = self.overflows;
        let result = match self.tcbs.resolve_mut(task) {
            Ok(tcb) => {
                let entering = tcb.wait.entering.get();
                let held = tcb.wait.ticks.get();
                let elapsed = now.wrapping_sub(entering) & Self::MAX_DELAY;
                if held == Self::MAX_DELAY {
                    // An indefinite block never times out.
                    Some(Self::MAX_DELAY)
                } else if overflows != tcb.wait.overflows && now >= entering {
                    tcb.wait.ticks = crate::timer::Split64::new(0);
                    None
                } else if elapsed < held {
                    let left = held.wrapping_sub(elapsed);
                    tcb.wait.ticks = crate::timer::Split64::new(left);
                    tcb.wait.entering = crate::timer::Split64::new(now);
                    tcb.wait.overflows = overflows;
                    Some(left)
                } else {
                    tcb.wait.ticks = crate::timer::Split64::new(0);
                    None
                }
            }
            Err(_) => None,
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
        // Nobody is parked in a wait, so there is nothing to read — one field
        // load in place of an arena lookup.
        if self.event_resumes == 0 {
            return false;
        }
        match self.tcbs.resolve_mut(task) {
            Ok(tcb) if tcb.event_blocked => {
                tcb.event_blocked = false;
                self.event_resumes = self.event_resumes.wrapping_sub(1);
                true
            }
            _ => false,
        }
    }

    /// Mark that `task` blocked inside an event-group wait, so the call it
    /// is inside resumes rather than restarts.
    pub(crate) fn set_event_resume(&mut self, task: TaskHandle) {
        if let Ok(tcb) = self.tcbs.resolve_mut(task) {
            // Count the slot, not the call.
            if !tcb.event_blocked {
                self.event_resumes = self.event_resumes.wrapping_add(1);
            }
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
    #[cold]
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
            self.set_task_priority(holder, waiter_priority)?;
            self.add_task_to_ready_list(holder)?;
        } else {
            self.set_task_priority(holder, waiter_priority)?;
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
    #[cold]
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
        self.set_task_priority(holder, base)?;
        let event_value = u64::from(C::MAX_PRIORITIES.saturating_sub(base));
        self.lists
            .set_value(Self::event_item(holder), event_value)?;
        self.add_task_to_ready_list(holder)?;
        Ok(true)
    }

    /// `vTaskPriorityDisinheritAfterTimeout`: a waiter gave up, so the
    /// holder keeps only what the tasks still waiting justify.
    #[cold]
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
        self.set_task_priority(holder, target)?;
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
    /// `#[cold]` because the call is GUARDED and reached from many sites on
    /// one hot path, which is the shape that pays: it lets LLVM keep the
    /// caller's frame setup out of the likely route. Measured on
    /// `riscv32-qemu-tick-work`, one attribute at a time.
    /// Worth queue -7 on its own.
    #[cold]
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

// ================================================================ tests ==

/// `kernel.rs` is corpus-oracled BY DESIGN (`docs/HOLES.md` H3), and these
/// do not change that. They exist for the mutants that survived BOTH
/// oracles — measured, 80 of 415 viable — because the corpus and the unit
/// suite each cannot reach them:
///
/// * the corpus runs `PosixDemoConfig` for 2,000 ticks, so the TICK
///   OVERFLOW arms of `delay_until` never execute. Nine survivors sat
///   there, and the only way to reach them is to choose a
///   `previous_wake` near the maximum rather than to simulate 2^32 ticks;
/// * the diagnostic accessors (`task_at`, `ready_items`, `ready_cursor`)
///   and the geometry guard in `with_tick_hook` are called by no scenario
///   at all — they are also on H2's list of APIs with no differential, so
///   two independent methods agree on the same surface.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use rusty_rtos_core::config::Config;
    use rusty_rtos_core::hooks::NoTickHook;

    use crate::system::tests::{NoTrace, TestConfig, TestPort};

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
        0,
        0,
        0,
        0,
        { <TestConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
    >;

    fn kernel() -> K {
        K::new(TestPort::default(), NoTrace).expect("the declared geometry adds up")
    }

    /// A kernel with one task of our own running, so the calls that act on
    /// "the current task" have one.
    fn running() -> K {
        let mut k = kernel();
        k.create_task("t", 1).expect("a task");
        let h = k.start_scheduler().expect("start");
        k.suspend(Some(h.timer)).expect("park the daemon");
        for _ in 0..50_u32 {
            if k.name_of(k.current()).expect("a name").as_str() == "t" {
                return k;
            }
            k.switch_context();
        }
        panic!("the task never reached the CPU");
    }

    /// `vTaskDelayUntil` with no overflow: a deadline still ahead delays,
    /// and one already passed does NOT.
    ///
    /// ```c
    /// else if( ( xTimeIncrement > ( xConstTickCount - *pxPreviousWakeTime ) ) ... )
    /// ```
    ///
    /// The second half is the one that matters and the one a naive test
    /// misses: a task that overran its period must NOT sleep a whole extra
    /// one, it must return immediately and say it did not delay.
    #[test]
    fn delay_until_skips_a_deadline_that_has_already_passed() {
        let mut k = running();

        // now is 0; the deadline is 20 ticks away, so it delays.
        let mut previous = 0_u64;
        assert_eq!(k.delay_until(&mut previous, 20), Ok(true));
        assert_eq!(previous, 20, "the wake time advances by the period");

        // Run the clock well past the next deadline.
        for _ in 0..60_u32 {
            k.tick_from_isr();
        }

        // now is 60 and the deadline was 40: already gone.
        let mut previous = 20_u64;
        assert_eq!(
            k.delay_until(&mut previous, 20),
            Ok(false),
            "the period was overrun, so it must not sleep another one"
        );
        assert_eq!(
            previous, 40,
            "and the wake time STILL advances, or the task never catches up"
        );
    }

    /// The tick-overflow arm, which the corpus cannot reach in 2,000 ticks.
    ///
    /// ```c
    /// if( xConstTickCount < *pxPreviousWakeTime ) {
    ///     if( ( xTimeToWake < *pxPreviousWakeTime ) && ( xTimeToWake > xConstTickCount ) )
    /// ```
    ///
    /// Both halves are needed: the wake time must have wrapped TOO, and it
    /// must still be ahead of the wrapped-around now. Reaching it needs a
    /// `previous_wake` near the maximum, not 2^32 ticks of simulation.
    #[test]
    fn delay_until_handles_a_wake_time_that_wrapped_with_the_tick() {
        let mut k = running();
        let max = K::MAX_DELAY;

        // A few ticks, so `now` is small and positive.
        for _ in 0..5_u32 {
            k.tick_from_isr();
        }

        // previous_wake sits just below the maximum; the period carries the
        // wake time over the top, to 9. now is 5, so 9 is still ahead.
        let mut previous = max.wrapping_sub(10);
        assert_eq!(
            k.delay_until(&mut previous, 20),
            Ok(true),
            "wrapped, and still in the future: it must delay"
        );
        assert_eq!(previous, 9, "the wake time wrapped with the tick");

        // Same wrap, but the wake time is already behind a larger now.
        let mut k = running();
        for _ in 0..40_u32 {
            k.tick_from_isr();
        }
        let mut previous = max.wrapping_sub(10);
        assert_eq!(
            k.delay_until(&mut previous, 20),
            Ok(false),
            "wrapped, and now is already past it: it must NOT delay"
        );
    }

    /// `task_at` walks the arena by SLOT, which is how a debugger or a
    /// health check enumerates tasks without holding handles.
    #[test]
    fn task_at_enumerates_live_slots_and_stops_at_the_end() {
        let mut k = kernel();
        let a = k.create_task("a", 1).expect("a task");

        assert_eq!(k.task_at(0), Some(a), "slot zero is the first task");
        assert_eq!(
            k.task_at(usize::from(u16::MAX)),
            None,
            "past the arena is None, not a handle"
        );
        assert_eq!(
            k.task_at(usize::from(u16::MAX) + 1),
            None,
            "and an index that does not even fit a u16 is None, not a panic"
        );
    }

    /// `ready_items` fills the caller's buffer and answers how many it
    /// wrote, stopping at the buffer's end rather than running past it.
    #[test]
    fn ready_items_fills_what_it_can_and_counts_what_it_filled() {
        let mut k = kernel();
        k.create_task("a", 1).expect("a");
        k.create_task("b", 1).expect("b");

        let mut out = [0_u16; 8];
        let n = k.ready_items(1, &mut out);
        assert_eq!(n, 2, "two tasks are ready at priority 1");

        // A buffer too small must be filled to its end and no further.
        let mut small = [0_u16; 1];
        assert_eq!(
            k.ready_items(1, &mut small),
            1,
            "it wrote one, because one is all there was room for"
        );

        // An empty priority writes nothing.
        let mut out = [0_u16; 8];
        assert_eq!(k.ready_items(0, &mut out), 0);
    }

    /// `ready_cursor` is the diagnostic that answers "did the round robin
    /// move", so it must report the list it was asked about.
    #[test]
    fn ready_cursor_reports_the_list_it_was_asked_about() {
        let mut k = kernel();
        k.create_task("a", 1).expect("a");
        k.create_task("b", 1).expect("b");

        let before = k.ready_cursor(1);
        k.switch_context();
        let after = k.ready_cursor(1);
        assert_ne!(after, before, "a switch moved the cursor at priority 1");
        assert_eq!(
            k.ready_cursor(0),
            k.ready_cursor(0),
            "and an untouched priority answers the same thing twice"
        );
    }

    /// `with_tick_hook`'s geometry guard, which is the only thing standing
    /// between a mis-declared kernel and a silently wrong one.
    ///
    /// `ITEMS` and `LISTS` are derived constants; if a caller writes them
    /// by hand and gets them wrong, every list index is off. The guard
    /// refuses, and these are the arms of it.
    #[test]
    fn the_geometry_guard_refuses_a_kernel_that_does_not_add_up() {
        // ITEMS too small for the declared tasks and timers.
        type WrongItems = crate::Kernel<
            TestConfig,
            TestPort,
            NoTrace,
            NoTickHook,
            4,
            // Legal as a geometry — a power of two above `LISTS` — but not
            // the derived value, which is 32. A grosser error (2 slots for
            // 12 lists) is now a COMPILE error in `SIZES_FIT`, so it cannot
            // be used to exercise the runtime guard any more. That the
            // check moved earlier is the improvement; this keeps the later
            // one honest.
            16,
            { crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 0) },
            1,
            1,
            0,
            0,
            0,
            0,
            { <TestConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
        >;
        assert!(
            WrongItems::new(TestPort::default(), NoTrace).is_err(),
            "ITEMS must be list_slots_for(TASKS, TIMERS, LISTS)"
        );

        // LISTS not derived from the priorities, queues and groups.
        type WrongLists = crate::Kernel<
            TestConfig,
            TestPort,
            NoTrace,
            NoTickHook,
            4,
            // Correct for the two lists declared below, so the refusal
            // below is attributable to `LISTS` and nothing else.
            { crate::list_slots_for(4, 0, 2) },
            2,
            1,
            1,
            0,
            0,
            0,
            0,
            { <TestConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
        >;
        assert!(
            WrongLists::new(TestPort::default(), NoTrace).is_err(),
            "LISTS must be lists_for(MAX_PRIORITIES, QUEUES, GROUPS)"
        );

        // A kernel with no room for a single task.
        type NoTasks = crate::Kernel<
            TestConfig,
            TestPort,
            NoTrace,
            NoTickHook,
            0,
            { crate::list_slots_for(0, 0, crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 0)) },
            { crate::lists_for(TestConfig::MAX_PRIORITIES, 1, 0) },
            1,
            1,
            0,
            0,
            0,
            0,
            { <TestConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
        >;
        assert!(
            NoTasks::new(TestPort::default(), NoTrace).is_err(),
            "TASKS == 0 cannot even hold the idle task"
        );

        // And the geometry that DOES add up is accepted, or the three
        // assertions above would pass for the wrong reason.
        assert!(K::new(TestPort::default(), NoTrace).is_ok());
    }
    /// `prvGetExpectedIdleTime`, arm one: something above the idle
    /// priority is running, so there is no window to sleep through.
    ///
    /// The three arms are a short-circuit chain and each is separately
    /// constructible, which is how they get pinned one at a time. A `>`
    /// turned `>=` in any of them would let the kernel sleep through work
    /// that is ready NOW.
    #[test]
    fn expected_idle_time_is_zero_while_anything_outranks_the_idle_task() {
        let mut k = kernel();
        k.create_task("hi", 1).expect("a task above idle");
        k.start_scheduler().expect("start");

        assert!(
            k.current_priority() > 0,
            "the task above idle is the one running"
        );
        assert_eq!(
            k.expected_idle_time(),
            0,
            "something outranks idle, so the window is zero"
        );
    }

    /// Arm two: another task shares the idle priority, so the very next
    /// tick has to be processed to give it its slice.
    #[test]
    fn expected_idle_time_is_zero_when_another_idle_task_wants_the_slice() {
        let mut k = kernel();
        k.create_task("a", 0).expect("a task AT the idle priority");
        k.start_scheduler().expect("start");

        assert!(
            k.ready_len(0).unwrap_or(0) > 1,
            "the idle task and ours are both ready at priority 0"
        );
        assert_eq!(
            k.expected_idle_time(),
            0,
            "a shared slice cannot be slept through"
        );
    }

    /// And with none of the three arms true, the window is the time to the
    /// next unblock -- which is the number the whole tickless feature
    /// rests on.
    #[test]
    fn expected_idle_time_is_the_window_to_the_next_unblock() {
        let mut k = running();
        k.delay(30).expect("park our task for thirty ticks");

        // Only the idle task is ready now, and nothing outranks it.
        assert_eq!(k.current_priority(), 0, "the idle task is running");
        assert_eq!(k.ready_len(0).unwrap_or(0), 1, "and it is alone at zero");
        assert_eq!(
            k.expected_idle_time(),
            30,
            "so the port may sleep right up to the next unblock"
        );
    }

    /// `ulTaskNotifyValueClear`'s reader half, and `xTaskNotifyAndQuery`'s:
    /// it answers the slot it was ASKED for, for the task it was asked
    /// about.
    ///
    /// Pinned with a value that is neither 0 nor 1, because those are what
    /// the surviving mutants replaced it with.
    #[test]
    fn notify_value_answers_the_slot_it_was_asked_for() {
        use crate::kernel::NotifyAction;

        let mut k = running();
        let me = k.current();
        k.notify(me, 0, 0xabcd_1234, NotifyAction::Overwrite)
            .expect("notify");

        assert_eq!(
            k.notify_value(Some(me), 0),
            Ok(0xabcd_1234),
            "the value that was set, not zero and not one"
        );
        assert_eq!(
            k.notify_value(None, 0),
            Ok(0xabcd_1234),
            "None means the CURRENT task"
        );
        assert!(
            k.notify_value(Some(me), TestConfig::NOTIFICATION_ARRAY_ENTRIES)
                .is_err(),
            "an index past the configured array is an error, not a zero"
        );
    }
    /// `delay_until`'s BOUNDARIES, which is where the four mutants that
    /// survived both oracles live.
    ///
    /// Each comparison in it has an edge the ordinary tests never touch,
    /// and each edge is reachable once you notice that `previous_wake` is
    /// a CALLER variable and the period may be zero:
    ///
    /// | line | comparison | the edge |
    /// |---|---|---|
    /// | 3228 | `wake_at < *previous_wake` | equal, via a period of 0 |
    /// | 3228 | `wake_at > now` | equal, by ticking to the wake time |
    /// | 3231 | `wake_at < *previous_wake` | equal, via a period of 0 |
    ///
    /// All three answers must be "do not delay". A `<=` or a `>=` in any of
    /// them turns a zero-length wait into a full period of sleep.
    #[test]
    fn delay_until_does_not_delay_on_any_of_its_boundaries() {
        let max = K::MAX_DELAY;

        // (1) OVERFLOW arm, period zero: the wake time equals the previous
        // one, so there is nothing to wait for.
        let mut k = running();
        for _ in 0..5_u32 {
            k.tick_from_isr();
        }
        let mut previous = max.wrapping_sub(10);
        assert_eq!(
            k.delay_until(&mut previous, 0),
            Ok(false),
            "a period of zero is not a period of MAX_DELAY"
        );

        // (2) OVERFLOW arm, wake time exactly equal to now: already due.
        // (max - 10) + 20 wraps to 9, so tick to 9.
        let mut k = running();
        for _ in 0..9_u32 {
            k.tick_from_isr();
        }
        let mut previous = max.wrapping_sub(10);
        assert_eq!(
            previous.wrapping_add(20) & max,
            9,
            "the wake time wraps to 9"
        );
        assert_eq!(
            k.delay_until(&mut previous, 20),
            Ok(false),
            "the wrapped wake time is NOW, so it is not in the future"
        );

        // (3) NON-overflow arm, period zero with now already at the wake
        // time: neither disjunct holds.
        let mut k = running();
        for _ in 0..10_u32 {
            k.tick_from_isr();
        }
        let mut previous = 10_u64;
        assert_eq!(
            k.delay_until(&mut previous, 0),
            Ok(false),
            "nothing to wait for, and no period to sleep"
        );
        assert_eq!(previous, 10, "and the wake time does not move");
    }
    /// The tick-wrap bookkeeping, which `xTaskIncrementTick` runs once
    /// every `MAX_DELAY + 1` ticks.
    ///
    /// Neither oracle can reach it: the corpus runs 2,000 ticks and the
    /// unit suite does not wrap either, so every mutation of it survived
    /// both. It is a private `fn` and this module is inside `kernel.rs`,
    /// so the test calls it directly rather than trying to simulate 2^32
    /// ticks — which is the right answer when the trigger is unreachable
    /// but the behaviour is not.
    #[test]
    fn switching_the_delayed_lists_flips_the_pair_and_counts_the_wrap() {
        let mut k = running();
        let swapped = k.delayed_swapped;
        let overflows = k.overflows;

        k.switch_delayed_lists();
        assert_ne!(
            k.delayed_swapped, swapped,
            "the delayed and overflow lists change places"
        );
        assert_eq!(
            k.overflows,
            overflows.wrapping_add(1),
            "and the wrap is counted, because `overflows` is how a trace \
             tells one epoch from the next"
        );

        k.switch_delayed_lists();
        assert_eq!(k.delayed_swapped, swapped, "flipping twice is the identity");
        assert_eq!(k.overflows, overflows.wrapping_add(2));
    }

    /// `prvResetNextTaskUnblockTime`, which the wrap calls: after the swap
    /// the next unblock time has to be recomputed from the list that is
    /// NOW the delayed one.
    ///
    /// That list is empty immediately after a wrap, so the answer must be
    /// `MAX_DELAY` — a stale value from the old list would make the
    /// kernel think a task is due and spin, or sleep through one that is.
    #[test]
    fn switching_the_delayed_lists_recomputes_the_next_unblock_time() {
        let mut k = running();
        let due = k.tick_count().wrapping_add(30);
        k.delay(30).expect("park our task on the delayed list");
        assert_eq!(k.next_unblock_time, due, "the parked task is what is next");

        k.switch_delayed_lists();
        assert_eq!(
            k.next_unblock_time,
            u64::MAX,
            "the list that is now 'delayed' is empty, so nothing is due"
        );
    }

    /// The sentinel for "nothing is due" is NOT the same value in the two
    /// places this kernel writes it, and this pins both.
    ///
    /// `new()` starts it at `MAX_DELAY` -- the tick mask, and what the C's
    /// `portMAX_DELAY` is. `reset_next_task_unblock_time` instead takes
    /// `head_value` of an empty list, which answers `ListsOf::MAX_VALUE`
    /// = `u64::MAX`, so the `unwrap_or(MAX_DELAY)` beside it never fires
    /// for emptiness.
    ///
    /// Neither is reachable as a tick, because the tick is masked to
    /// `MAX_DELAY`, so no scheduling decision can tell them apart and the
    /// corpus is green either way. It IS visible to the tickless path:
    /// `expected_idle_time` answers `next_unblock_time - tick`, which is
    /// the window the port is handed, and the two sentinels differ there
    /// by four billion.
    ///
    /// Recorded rather than changed: making them agree is an owner
    /// decision, because `expected_idle_time` feeds a port and any change
    /// to it moves what a tickless build asks for.
    #[test]
    fn the_nothing_is_due_sentinel_differs_between_init_and_reset() {
        let k = kernel();
        assert_eq!(
            k.next_unblock_time,
            K::MAX_DELAY,
            "a fresh kernel uses the tick mask, which is the C's portMAX_DELAY"
        );

        let mut k = running();
        k.reset_next_task_unblock_time();
        assert_eq!(
            k.next_unblock_time,
            u64::MAX,
            "and a reset over an empty list uses the LIST's maximum instead"
        );
        assert_ne!(
            K::MAX_DELAY,
            u64::MAX,
            "the two are genuinely different values, or this test is vacuous"
        );
    }

    /// The owe hint must not outlive the debt it stands for.
    ///
    /// A census over three scenarios found every raise producing exactly TWO
    /// entries to `resume_pending_owed` — one that did the work and returned
    /// `true`, and a second that re-derived "nothing owed" and cleared the
    /// hint on its way out. BlockQ: 43,110 raises, 86,207 entries. Clearing
    /// the hint at the end of the work made entries equal raises.
    ///
    /// Both directions are asserted. A `clear_owe_if_settled` that cleared
    /// unconditionally would pass the first half and lose a real owed item,
    /// which is a missing trace line in the differential — so the second half
    /// is the half that matters.
    #[test]
    fn the_owe_hint_does_not_outlive_the_debt() {
        let mut k = running();
        let me = k.current();
        let i = me.index() as usize;
        fn hint(k: &K, i: usize) -> bool {
            k.owes_anything.get(i).copied().unwrap_or(false)
        }

        k.owe(me);
        assert!(hint(&k, i), "the hint was raised");
        k.clear_owe_if_settled(i);
        assert!(
            !hint(&k, i),
            "nothing is owed, so the hint is dead and must go -- leaving it standing costs one whole entry to the owed body per owed item"
        );

        // The negative case, once per kind of debt. A `clear_owe_if_settled`
        // that cleared unconditionally would pass the half above and lose a
        // real owed item, which is a missing line in the differential.
        for kind in 0..3 {
            match kind {
                0 => *k.owed_exits.get_mut(i).expect("in range") = 1,
                1 => k.set_flag(i, K::F_YIELD, true),
                _ => {
                    *k.owed_trace.get_mut(i).expect("in range") =
                        crate::kernel::OwedTrace::AddNewTaskToReadyList {
                            task: me,
                            priority: 1,
                        }
                }
            }
            k.owe(me);
            k.clear_owe_if_settled(i);
            assert!(hint(&k, i), "kind {kind} is live debt; the hint must stay");
            *k.owed_exits.get_mut(i).expect("in range") = 0;
            k.set_flag(i, K::F_YIELD, false);
            *k.owed_trace.get_mut(i).expect("in range") = crate::kernel::OwedTrace::None;
        }
    }

    /// The stream-buffer resume flag is a ONE-SHOT: the take clears it, so
    /// a second take answers None. A `take` that did not clear would let a
    /// blocked reader wake twice on one send.
    #[test]
    fn a_stream_resume_is_taken_once_and_then_gone() {
        let mut k = running();
        let me = k.current();
        assert_eq!(k.take_stream_resume(me), None, "nothing set yet");

        k.set_stream_resume(me, 7);
        assert_eq!(k.take_stream_resume(me), Some(7), "the value that was set");
        assert_eq!(k.take_stream_resume(me), None, "and it is consumed");
    }

    /// `vTaskMissedYield`: a yield asked for while the scheduler is
    /// suspended has to be remembered, or it is simply lost.
    #[test]
    fn a_missed_yield_is_remembered_rather_than_dropped() {
        let mut k = running();
        k.yield_pending = false;
        k.missed_yield();
        assert!(
            k.yield_pending,
            "the yield survives until something can act on it"
        );
    }
}

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
use rusty_rtos_core::handle::{Queue as QueueKind, QueueHandle, Task as TaskKind, TaskHandle};
use rusty_rtos_core::list::{ItemId, ListId, Lists};
use rusty_rtos_core::port::Port;
use rusty_rtos_core::priority::Priority;
use rusty_rtos_core::tick::TickWidth;
use rusty_rtos_core::trace::{Event, Trace};

use crate::name::Name;
use crate::{OVERHEAD_LISTS, items_for, lists_for};

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
struct Tcb {
    name: Name,
    /// `uxPriority`, the possibly-inherited one the scheduler sorts by.
    priority: u8,
    /// `uxBasePriority`, what the task asked for.
    base_priority: u8,
}

/// One queue: the C `Queue_t` minus the byte pointers. Storage is a range
/// of the kernel's shared slot pool.
#[derive(Debug, Clone, Copy)]
struct Queue {
    base: usize,
    /// `uxLength`.
    length: usize,
    /// `uxMessagesWaiting`.
    waiting: usize,
    /// Index within `0..length` of the next item to read.
    read: usize,
    /// Index within `0..length` of the next slot to write.
    write: usize,
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
/// `TASKS`, `ITEMS`, `LISTS`, `QUEUES` and `SLOTS` are the geometry: see
/// the crate docs and [`items_for`] / [`lists_for`]. [`Kernel::new`]
/// refuses a geometry that does not add up.
pub struct Kernel<
    C: Config,
    P: Port,
    T: Trace,
    const TASKS: usize,
    const ITEMS: usize,
    const LISTS: usize,
    const QUEUES: usize,
    const SLOTS: usize,
> {
    port: P,
    trace: T,
    tcbs: Arena<TaskKind, Tcb, TASKS>,
    queues: Arena<QueueKind, Queue, QUEUES>,
    lists: Lists<ITEMS, LISTS>,
    slots: [u64; SLOTS],
    slots_used: usize,
    /// `pxCurrentTCB`.
    current: TaskHandle,
    /// `uxTopReadyPriority`.
    top_ready_priority: u8,
    /// `xTickCount`, masked to the configuration's tick width.
    tick: u64,
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
    /// How deep a task's critical nesting was when it was switched out —
    /// `uxSavedCriticalNesting` in the Posix port's `prvSwitchThread`. The
    /// exits that unwind it are owed too, and counted when they are paid,
    /// which is what puts a sim tick where the C one lands.
    owed_exits: [u32; TASKS],
    _config: PhantomData<C>,
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
    pub fn new(port: P, trace: T) -> Result<Self> {
        C::validate()?;
        if ITEMS != items_for(TASKS)
            || LISTS != lists_for(C::MAX_PRIORITIES, QUEUES)
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
            current: TaskHandle::NULL,
            top_ready_priority: 0,
            tick: C::INITIAL_TICK_COUNT,
            pended_ticks: 0,
            suspended_depth: 0,
            running: false,
            next_unblock_time: Self::MAX_DELAY,
            yield_pending: false,
            task_count: 0,
            delayed_swapped: false,
            overflows: 0,
            started: [false; TASKS],
            owes_yield: [false; TASKS],
            owed_exits: [0; TASKS],
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

    const fn ready_list(priority: u8) -> ListId {
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
    fn queue_send_list(queue: QueueHandle) -> ListId {
        let offset = u8::try_from(queue.index().saturating_mul(2)).unwrap_or(u8::MAX);
        C::MAX_PRIORITIES
            .saturating_add(u8::try_from(OVERHEAD_LISTS).unwrap_or(u8::MAX))
            .saturating_add(offset)
    }

    /// `xTasksWaitingToReceive` of a queue.
    fn queue_receive_list(queue: QueueHandle) -> ListId {
        Self::queue_send_list(queue).saturating_add(1)
    }

    /// A task's `xStateListItem`: the task's own arena index.
    const fn state_item(task: TaskHandle) -> ItemId {
        task.index()
    }

    /// A task's `xEventListItem`: `TASKS` above its state item, so the two
    /// never collide and either maps back to its task by arithmetic alone.
    fn event_item(task: TaskHandle) -> ItemId {
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

    fn current_priority(&self) -> u8 {
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
            let waiting_on_event = self.lists.container(Self::event_item(task))?.is_some();
            return Ok(if waiting_on_event {
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

    // ------------------------------------------------------------ tracing --

    fn trace_task<F>(&mut self, task: TaskHandle, make: F)
    where
        F: for<'a> FnOnce(TaskHandle, &'a str) -> Event<'a>,
    {
        let name = self.tcbs.get(task).map(|t| t.name).unwrap_or_default();
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace.event(tick, make(task, name.as_str()));
    }

    fn priority_value(raw: u8) -> Priority {
        Priority::new(raw, C::MAX_PRIORITIES).unwrap_or(Priority::IDLE)
    }

    // --------------------------------------------------- critical section --

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
    fn port_yield(&mut self) {
        self.enter_critical();
        self.port.count_yield();
        self.switch_context();
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
        // configASSERT( uxPriority < configMAX_PRIORITIES ), then C clamps.
        let priority = priority.min(C::MAX_PRIORITIES.saturating_sub(1));
        let tcb = Tcb {
            name: Name::new(name, C::MAX_TASK_NAME_LEN),
            priority,
            base_priority: priority,
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
        self.add_new_task_to_ready_list(handle, priority)?;
        Ok(handle)
    }

    /// `prvAddNewTaskToReadyList`.
    fn add_new_task_to_ready_list(&mut self, task: TaskHandle, priority: u8) -> Result<()> {
        self.enter_critical();
        {
            self.task_count = self.task_count.saturating_add(1);
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
    fn add_task_to_ready_list(&mut self, task: TaskHandle) -> Result<()> {
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
        let timer_queue = self.queue_create(C::TIMER_QUEUE_LENGTH)?;
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
                        // means it is gone; keep the current task rather
                        // than index out of range.
                        return;
                    }
                    top = top.saturating_sub(1);
                }
                Err(_) => return,
            }
        }
        let Ok(Some(item)) = self.lists.next_round_robin(Self::ready_list(top)) else {
            return;
        };
        let Ok(next) = self.task_of_state_item(item) else {
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
        let index = usize::from(self.current.index());
        // First the stack unwinds — the critical sections the task had open
        // when it was switched out — and only then does the statement after
        // the yield run. Reversing the two moves every tick.
        let owed = self.owed_exits.get(index).copied().unwrap_or(0);
        if owed > 0 {
            if let Some(slot) = self.owed_exits.get_mut(index) {
                *slot = 0;
            }
            self.port.set_nesting(owed);
            for _ in 0..owed {
                self.exit_critical();
            }
            return true;
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

    /// Record that `task` was preempted before the `portYIELD()` at the end
    /// of the call it is inside.
    fn owe_yield(&mut self, task: TaskHandle) {
        if let Some(flag) = self.owes_yield.get_mut(usize::from(task.index())) {
            *flag = true;
        }
    }

    /// Hand the CPU from `outgoing` to `incoming`: what `prvSwitchThread`
    /// does to `uxCriticalNesting`.
    ///
    /// The outgoing task's open sections go with it, to be unwound when it
    /// runs again; the calls that would have unwound them here are the tail
    /// of a frame that is no longer running, so the port ignores exactly
    /// that many. A task being switched in for the first time has a fresh
    /// stack and owes nothing.
    fn hand_over(&mut self, outgoing: TaskHandle, incoming: TaskHandle) {
        let pending = self.port.take_nesting();
        if let Some(slot) = self.owed_exits.get_mut(usize::from(outgoing.index())) {
            // A task cannot be switched out twice without running in
            // between, so this replaces rather than accumulates.
            *slot = pending;
        }
        self.port.swallow_exits(pending);
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
            if C::USE_PREEMPTION
                && C::USE_TIME_SLICING
                && self
                    .lists
                    .len(Self::ready_list(self.current_priority()))
                    .unwrap_or(0)
                    > 1
            {
                switch_required = true;
            }
            if C::USE_PREEMPTION && self.yield_pending {
                switch_required = true;
            }
        } else {
            self.pended_ticks = self.pended_ticks.wrapping_add(1);
        }
        switch_required
    }

    /// The wake loop inside `xTaskIncrementTick`.
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

    /// `prvAddCurrentTaskToDelayedList`.
    fn add_current_task_to_delayed_list(
        &mut self,
        ticks: u64,
        can_block_indefinitely: bool,
    ) -> Result<()> {
        let now = self.tick;
        let current = self.current;
        let item = Self::state_item(current);
        if self.lists.container(item)?.is_some() {
            let _ = self.lists.remove(item);
        }
        if ticks == Self::MAX_DELAY && can_block_indefinitely {
            self.lists.insert_end(Self::suspended_list(), item)?;
            return Ok(());
        }
        let wake_at = now.wrapping_add(ticks) & Self::MAX_DELAY;
        self.lists.set_value(item, wake_at)?;
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
            if self.lists.container(item)?.is_some() {
                let _ = self.lists.remove(item);
            }
            let event = Self::event_item(target);
            if self.lists.container(event)?.is_some() {
                let _ = self.lists.remove(event);
            }
            self.lists.insert_end(Self::suspended_list(), item)?;
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

    /// `vTaskPrioritySet`. `None` is the calling task.
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
        self.suspended_depth = self.suspended_depth.saturating_add(1);
    }

    /// `xTaskResumeAll`; `true` when it yielded on the caller's behalf.
    pub fn resume_all(&mut self) -> bool {
        let mut already_yielded = false;
        self.enter_critical();
        {
            self.suspended_depth = self.suspended_depth.saturating_sub(1);
            if self.suspended_depth == 0 && self.task_count > 0 {
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
                if self.pended_ticks > 0 {
                    let mut pended = self.pended_ticks;
                    while pended > 0 {
                        if self.increment_tick() {
                            self.yield_pending = true;
                        }
                        pended = pended.saturating_sub(1);
                    }
                    self.pended_ticks = 0;
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

    // ------------------------------------------------------------ queues --

    /// `xQueueCreate`, for a queue of `u64` items.
    ///
    /// The C kernel copies bytes; K1's corpus moves 32-bit values, so the
    /// slot is a `u64` and a wider payload waits for the scenario that
    /// needs it rather than a generic nobody exercises.
    ///
    /// # Errors
    /// [`Error::Full`] when the queue arena or the shared slot pool is
    /// exhausted; [`Error::InvalidArgument`] for a zero length.
    pub fn queue_create(&mut self, length: usize) -> Result<QueueHandle> {
        if length == 0 {
            return Err(Error::InvalidArgument);
        }
        let base = self.slots_used;
        let end = base.checked_add(length).ok_or(Error::Full)?;
        if end > SLOTS {
            return Err(Error::Full);
        }
        let handle = self
            .queues
            .try_insert(Queue {
                base,
                length,
                waiting: 0,
                read: 0,
                write: 0,
            })
            .map_err(|_| Error::Full)?;
        self.slots_used = end;
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

    /// `xQueueSend` to the back, with a block time in ticks.
    ///
    /// K1's corpus only ever sends with a zero block time, so a full queue
    /// returns [`Error::Full`] rather than blocking; blocking sends arrive
    /// with the scenario that needs them.
    ///
    /// # Errors
    /// [`Error::Full`] when the queue is full; [`Error::Gone`] for a stale
    /// handle; [`Error::Unsupported`] for a non-zero block time.
    pub fn queue_send(&mut self, queue: QueueHandle, value: u64, ticks: u64) -> Result<()> {
        if ticks != 0 {
            return Err(Error::Unsupported);
        }
        self.enter_critical();
        let snapshot = match self.queues.resolve(queue) {
            Ok(q) => *q,
            Err(e) => {
                self.exit_critical();
                return Err(e);
            }
        };
        if snapshot.waiting < snapshot.length {
            let tick = self.tick;
            self.trace.note_exits(self.port.exits());
            self.trace.event(tick, Event::QueueSend { queue, name: "" });
            if let Some(cell) = self
                .slots
                .get_mut(snapshot.base.saturating_add(snapshot.write))
            {
                *cell = value;
            }
            if let Ok(q) = self.queues.resolve_mut(queue) {
                q.write = q.write.saturating_add(1);
                if q.write >= q.length {
                    q.write = 0;
                }
                q.waiting = q.waiting.saturating_add(1);
            }
            let receivers = Self::queue_receive_list(queue);
            if self.lists.is_empty(receivers) == Ok(false)
                && self.remove_from_event_list(receivers)?
            {
                // queueYIELD_IF_USING_PREEMPTION(), inside the section.
                self.port_yield();
            }
            self.exit_critical();
            return Ok(());
        }
        // The C exits the critical section *before* the failure trace.
        self.exit_critical();
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace
            .event(tick, Event::QueueSendFailed { queue, name: "" });
        Err(Error::Full)
    }

    /// `xQueueReceive` with a block time in ticks.
    ///
    /// As [`Kernel::queue_send`], K1's corpus only receives with a zero
    /// block time.
    ///
    /// # Errors
    /// [`Error::Empty`] when the queue is empty; [`Error::Gone`] for a
    /// stale handle; [`Error::Unsupported`] for a non-zero block time.
    pub fn queue_receive(&mut self, queue: QueueHandle, ticks: u64) -> Result<u64> {
        if ticks != 0 {
            return Err(Error::Unsupported);
        }
        self.enter_critical();
        let snapshot = match self.queues.resolve(queue) {
            Ok(q) => *q,
            Err(e) => {
                self.exit_critical();
                return Err(e);
            }
        };
        if snapshot.waiting > 0 {
            // C copies the data out, then fires the trace.
            let value = self
                .slots
                .get(snapshot.base.saturating_add(snapshot.read))
                .copied()
                .unwrap_or(0);
            if let Ok(q) = self.queues.resolve_mut(queue) {
                q.read = q.read.saturating_add(1);
                if q.read >= q.length {
                    q.read = 0;
                }
                q.waiting = q.waiting.saturating_sub(1);
            }
            let tick = self.tick;
            self.trace.note_exits(self.port.exits());
            self.trace
                .event(tick, Event::QueueReceive { queue, name: "" });
            let senders = Self::queue_send_list(queue);
            if self.lists.is_empty(senders) == Ok(false) && self.remove_from_event_list(senders)? {
                // queueYIELD_IF_USING_PREEMPTION(), inside the section.
                self.port_yield();
            }
            self.exit_critical();
            return Ok(value);
        }
        self.exit_critical();
        let tick = self.tick;
        self.trace.note_exits(self.port.exits());
        self.trace
            .event(tick, Event::QueueReceiveFailed { queue, name: "" });
        Err(Error::Empty)
    }

    /// `vQueueWaitForMessageRestricted`: what the timer service task does
    /// when it has no timer to run — block on the timer queue without
    /// suspending the scheduler.
    ///
    /// `wait_indefinitely` is the C third argument: when set, the wait
    /// becomes `portMAX_DELAY` and the task goes to the suspended list
    /// rather than a delayed one. The two queue locks around it are C's
    /// `prvLockQueue` and `prvUnlockQueue`, which are three critical
    /// sections in total — and therefore three ticks' worth of sim time
    /// every sixteen, which is why they are here rather than elided.
    ///
    /// # Errors
    /// A list error surfaces as itself.
    pub fn wait_for_message_restricted(
        &mut self,
        queue: QueueHandle,
        ticks: u64,
        wait_indefinitely: bool,
    ) -> Result<()> {
        // prvLockQueue
        self.enter_critical();
        let empty = self
            .queues
            .resolve(queue)
            .map(|q| q.waiting == 0)
            .unwrap_or(false);
        self.exit_critical();
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
        // prvUnlockQueue: one critical section per lock counter.
        self.enter_critical();
        self.exit_critical();
        self.enter_critical();
        self.exit_critical();
        Ok(())
    }

    /// `xTaskRemoveFromEventList`; `true` when the woken task outranks the
    /// running one and a yield is therefore required.
    fn remove_from_event_list(&mut self, list: ListId) -> Result<bool> {
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

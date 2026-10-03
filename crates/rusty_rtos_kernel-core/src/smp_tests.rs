//! SMP: the scheduler with `configNUMBER_OF_CORES == 2`.
//!
//! Each test is a scenario whose expected answer was read off the pinned
//! `tasks.c` (FreeRTOS-Kernel V11.3.1: `prvSelectHighestPriorityTask`,
//! `prvYieldForTask`, `prvYieldCore`, `xTaskIncrementTick`), with
//! `configRUN_MULTIPLE_PRIORITIES == 1` and no core affinity. The port here
//! lets a test say which core is calling, and COMMITS its switches -- a
//! stacked port, as on silicon -- so a test decides exactly when each core
//! runs `switch_context`.
//!
//! These are the first SMP evidence, not the last: the C oracle with two
//! cores (the conformance method's answer) is the next slice, and these
//! cases are the ones it must agree with.

use core::cell::Cell;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::tick::Bits32;

use crate::system::tests::NoTrace;

struct SmpConfig;

impl Config for SmpConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 100;
    const MAX_PRIORITIES: u8 = 5;
    const MINIMAL_STACK_SIZE: usize = 1;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 1;
    const TIMER_TASK_STACK_DEPTH: usize = 1;
    const TIMER_QUEUE_LENGTH: usize = 1;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
    const NUMBER_OF_CORES: u8 = 2;
}

/// The same, with time slicing on.
struct SlicingConfig;

impl Config for SlicingConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 100;
    const MAX_PRIORITIES: u8 = 5;
    const MINIMAL_STACK_SIZE: usize = 1;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 1;
    const TIMER_TASK_STACK_DEPTH: usize = 1;
    const TIMER_QUEUE_LENGTH: usize = 1;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
    const NUMBER_OF_CORES: u8 = 2;
    const USE_TIME_SLICING: bool = true;
}

/// A port with a settable core id that commits its own switches.
#[derive(Default)]
struct SmpPort {
    core: Cell<u8>,
    nesting: Cell<u32>,
    yields: Cell<u32>,
}

impl SmpPort {
    fn on(&self, core: u8) {
        self.core.set(core);
    }
}

impl Port for SmpPort {
    const COMMITS_SWITCH: bool = true;
    fn count_yield(&self) {
        self.yields.set(self.yields.get() + 1);
    }
    fn yield_now(&self) {}
    fn yield_from_isr(&self, _woken: Woken) {}
    fn enter_critical(&self) {
        self.nesting.set(self.nesting.get() + 1);
    }
    fn exit_critical(&self) {
        self.nesting.set(self.nesting.get().saturating_sub(1));
    }
    fn set_interrupt_mask_from_isr(&self) -> u32 {
        0
    }
    fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
    fn in_isr(&self) -> bool {
        false
    }
    fn core_id(&self) -> u8 {
        self.core.get()
    }
    fn set_in_tick_entry(&self, _yes: bool) {}
}

const TASKS: usize = 8;
const QUEUES: usize = 2;

type K<Cfg> = crate::Kernel<
    Cfg,
    SmpPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { crate::list_slots_for(TASKS, 0, crate::lists_for(5, QUEUES, 0)) },
    { crate::lists_for(5, QUEUES, 0) },
    QUEUES,
    4,
    0,
    0,
    0,
    0,
    1,
>;

fn kernel<Cfg: Config>() -> K<Cfg> {
    K::<Cfg>::new(SmpPort::default(), NoTrace).expect("the declared geometry adds up")
}

/// Switch `core`, the way its switching interrupt would.
fn switch<Cfg: Config>(k: &mut K<Cfg>, core: u8) {
    k.port().on(core);
    k.switch_context();
}

#[test]
fn every_core_starts_on_its_own_idle_task() {
    let mut k = kernel::<SmpConfig>();
    let a = k.create_task("a", 3).expect("a");
    let started = k.start_scheduler().expect("start");
    assert!(!started.passive_idle.is_null(), "two cores, two idle tasks");
    assert_eq!(k.current_on(0), started.idle);
    assert_eq!(k.current_on(1), started.passive_idle);
    // Created before the start, `a` holds no core until a core switches.
    assert_ne!(k.current_on(0), a);
    assert_ne!(k.current_on(1), a);
}

#[test]
fn two_cores_run_the_two_highest_tasks_and_never_the_same_one() {
    let mut k = kernel::<SmpConfig>();
    let a = k.create_task("a", 3).expect("a");
    let b = k.create_task("b", 2).expect("b");
    k.start_scheduler().expect("start");
    switch(&mut k, 0);
    switch(&mut k, 1);
    assert_eq!(k.current_on(0), a, "core 0 takes the highest");
    assert_eq!(
        k.current_on(1),
        b,
        "core 1 cannot take `a`: core 0 holds it"
    );
    // Switching again changes nothing: each core keeps its own task.
    switch(&mut k, 0);
    switch(&mut k, 1);
    assert_eq!((k.current_on(0), k.current_on(1)), (a, b));
}

#[test]
fn a_newly_ready_task_preempts_the_core_running_the_lowest_priority() {
    let mut k = kernel::<SmpConfig>();
    let a = k.create_task("a", 3).expect("a");
    let b = k.create_task("b", 2).expect("b");
    k.start_scheduler().expect("start");
    switch(&mut k, 0);
    switch(&mut k, 1);
    let _ = k.take_core_yields();

    // Core 0 creates `c` at 3. It outranks `b` (core 1) and not `a`, so
    // `prvYieldForTask` asks CORE 1 -- not the caller -- to yield.
    k.port().on(0);
    let c = k.create_task("c", 3).expect("c");
    assert_eq!(k.take_core_yields(), 0b10, "core 1 is interrupted");
    assert_eq!(k.take_core_yields(), 0, "the read clears it");
    assert_eq!(k.current_on(0), a, "the caller keeps running");

    switch(&mut k, 1);
    assert_eq!(
        k.current_on(1),
        c,
        "core 1 skips `a` (held by core 0) and takes `c`"
    );
    let _ = b;
}

#[test]
fn a_second_request_does_not_interrupt_a_core_twice() {
    let mut k = kernel::<SmpConfig>();
    k.create_task("a", 3).expect("a");
    k.create_task("b", 2).expect("b");
    k.start_scheduler().expect("start");
    switch(&mut k, 0);
    switch(&mut k, 1);
    let _ = k.take_core_yields();
    k.port().on(0);
    k.create_task("c", 3).expect("c");
    k.create_task("d", 3).expect("d");
    // `taskTASK_SCHEDULED_TO_YIELD`: core 1 was asked once, and a core
    // already asked is not a candidate for the second task.
    assert_eq!(k.take_core_yields(), 0b10);
}

#[test]
fn a_running_idle_task_ranks_below_a_real_priority_zero_task() {
    let mut k = kernel::<SmpConfig>();
    let a = k.create_task("a", 3).expect("a");
    let started = k.start_scheduler().expect("start");
    switch(&mut k, 0);
    // Core 1: the timer daemon is the only other ready task; it blocks at
    // once in a real system, so take it out of the picture by switching
    // core 1 to it and then parking it.
    switch(&mut k, 1);
    let _ = k.take_core_yields();
    assert_eq!(k.current_on(0), a);
    // Make core 1 run its idle task: delay the daemon from core 1.
    k.port().on(1);
    k.delay(100).expect("the daemon blocks");
    switch(&mut k, 1);
    // AN idle task, and not necessarily core 1's own: with no core affinity
    // the C lets idle tasks migrate. Level 0 is walked from its head, where
    // `IDLE0` sits unheld (core 0 runs `a`), so core 1 takes `IDLE0`. This
    // test first expected `IDLE1` and the kernel was right.
    let idle_on_1 = k.current_on(1);
    assert!(idle_on_1 == started.idle || idle_on_1 == started.passive_idle);
    assert_eq!(idle_on_1, started.idle, "the head of level 0");
    let _ = k.take_core_yields();

    // A priority-0 task readied from core 0: equal to idle's number, but an
    // idle task counts as one below it, so core 1 is asked.
    k.port().on(0);
    let z = k.create_task("z", 0).expect("z");
    assert_eq!(k.take_core_yields(), 0b10);
    // Being asked to yield is not the same as handing the core to `z`.
    // Selection walks level 0 from its HEAD, and the idle tasks are ordinary
    // members of that list: `IDLE0` moves to the end, `IDLE1` is at the head
    // and unheld, so core 1 takes `IDLE1`. The C does the same, and gets `z`
    // running through `configIDLE_SHOULD_YIELD` -- the idle task yields
    // whenever another task shares its priority. (This test first expected
    // `z` here; the kernel was right a second time.)
    switch(&mut k, 1);
    assert_eq!(k.current_on(1), started.passive_idle);
    // The idle task's yield: it goes behind `z`, and `z` runs.
    switch(&mut k, 1);
    assert_eq!(k.current_on(1), z);
}

#[test]
fn the_tick_yields_every_core_whose_priority_has_a_second_ready_task() {
    let mut k = kernel::<SlicingConfig>();
    let a = k.create_task("a", 3).expect("a");
    let b = k.create_task("b", 3).expect("b");
    k.create_task("c", 3).expect("c");
    k.start_scheduler().expect("start");
    switch(&mut k, 0);
    switch(&mut k, 1);
    assert_eq!((k.current_on(0), k.current_on(1)), (a, b));
    let _ = k.take_core_yields();

    // The tick runs on core 0. Level 3 holds three ready tasks, so BOTH
    // cores owe a time-slice yield: core 0 switches on the way out, core 1
    // is interrupted.
    k.port().on(0);
    let switch_here = k.increment_tick();
    assert!(switch_here, "core 0 slices");
    assert_eq!(k.take_core_yields(), 0b10, "core 1 slices too");
}

/// Two cores with time slicing OFF.
struct NoSlicingConfig;

impl Config for NoSlicingConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 100;
    const MAX_PRIORITIES: u8 = 5;
    const MINIMAL_STACK_SIZE: usize = 1;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 1;
    const TIMER_TASK_STACK_DEPTH: usize = 1;
    const TIMER_QUEUE_LENGTH: usize = 1;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
    const NUMBER_OF_CORES: u8 = 2;
    const USE_TIME_SLICING: bool = false;
}

/// The same three tasks with time slicing off: the tick slices nothing
/// (plan P5: `configUSE_PREEMPTION && configUSE_TIME_SLICING` survived as
/// `||`, because every two-core oracle runs with both on).
#[test]
fn without_time_slicing_the_tick_yields_no_core() {
    let mut k = kernel::<NoSlicingConfig>();
    let a = k.create_task("a", 3).expect("a");
    let b = k.create_task("b", 3).expect("b");
    k.create_task("c", 3).expect("c");
    k.start_scheduler().expect("start");
    switch(&mut k, 0);
    switch(&mut k, 1);
    assert_eq!((k.current_on(0), k.current_on(1)), (a, b));
    let _ = k.take_core_yields();
    k.port().on(0);
    assert!(!k.increment_tick(), "core 0 does not slice");
    assert_eq!(k.take_core_yields(), 0, "nor does core 1");
}

#[test]
fn a_yielding_task_goes_behind_the_tasks_that_waited() {
    let mut k = kernel::<SmpConfig>();
    let a = k.create_task("a", 3).expect("a");
    let b = k.create_task("b", 3).expect("b");
    let c = k.create_task("c", 3).expect("c");
    k.start_scheduler().expect("start");
    switch(&mut k, 0);
    switch(&mut k, 1);
    assert_eq!((k.current_on(0), k.current_on(1)), (a, b));
    // Core 0 yields: `a` moves to the END of level 3, the walk from the head
    // finds `b` (held by core 1) and then `c`.
    switch(&mut k, 0);
    assert_eq!(k.current_on(0), c);
    // And `a` is next for whichever core switches.
    switch(&mut k, 1);
    assert_eq!(k.current_on(1), a);
}

#[test]
fn a_semaphore_give_wakes_its_waiter_on_the_lowest_core() {
    let mut k = kernel::<SmpConfig>();
    let hi = k.create_task("hi", 4).expect("hi");
    let mid = k.create_task("mid", 3).expect("mid");
    let low = k.create_task("low", 2).expect("low");
    let sem = k.semaphore_create_binary().expect("a semaphore");
    k.start_scheduler().expect("start");
    switch(&mut k, 0);
    switch(&mut k, 1);
    assert_eq!((k.current_on(0), k.current_on(1)), (hi, mid));
    let _ = k.take_core_yields();

    // `hi` blocks on the semaphore from core 0; core 0 takes `low`.
    k.port().on(0);
    let wait = k.semaphore_take(sem, 100).expect("take");
    assert!(matches!(wait, crate::queue::Wait::Blocked));
    switch(&mut k, 0);
    assert_eq!(k.current_on(0), low);
    let _ = k.take_core_yields();

    // `mid` (core 1) gives it. `hi` outranks both running tasks; the lowest
    // is `low` on core 0, so core 0 -- NOT the giver's core -- is asked.
    k.port().on(1);
    let _ = k.semaphore_give(sem).expect("give");
    assert_eq!(k.take_core_yields(), 0b01);
    switch(&mut k, 0);
    assert_eq!(k.current_on(0), hi);
    assert_eq!(k.current_on(1), mid, "the giver keeps running");
}

#[test]
fn one_core_builds_never_ask_for_a_cross_core_yield() {
    // The one-core kernel is the conformance-proven one; this pins that the
    // SMP bookkeeping is inert there.
    use crate::system::tests::{TestConfig, TestPort};
    type One = crate::Kernel<
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
        { <TestConfig as Config>::TIMER_QUEUE_LENGTH },
    >;
    let mut k = One::new(TestPort::default(), NoTrace).expect("geometry");
    k.create_task("a", 1).expect("a");
    let started = k.start_scheduler().expect("start");
    assert!(started.passive_idle.is_null());
    k.create_task("b", 2).expect("b");
    assert_eq!(k.take_core_yields(), 0);
}

// ------------------------------------------------- S1b: the OTHER core --
//
// `vTaskDelete`, `vTaskSuspend`, `vTaskPrioritySet`, `vTaskResume` and a
// notification, each acting on a task a DIFFERENT core is running or
// should run.

/// Two cores running `a` (core 0) and `b` (core 1), both at priority 3,
/// with a third task `c` ready at 2 for whichever core frees up.
fn two_running() -> (
    K<SmpConfig>,
    crate::StartHandles,
    [rusty_rtos_core::handle::TaskHandle; 3],
) {
    let mut k = kernel::<SmpConfig>();
    let a = k.create_task("a", 3).expect("a");
    let b = k.create_task("b", 3).expect("b");
    let c = k.create_task("c", 2).expect("c");
    let started = k.start_scheduler().expect("start");
    switch(&mut k, 0);
    switch(&mut k, 1);
    assert_eq!((k.current_on(0), k.current_on(1)), (a, b));
    let _ = k.take_core_yields();
    (k, started, [a, b, c])
}

#[test]
fn deleting_a_task_another_core_is_running_waits_for_that_core() {
    let (mut k, _, [_a, b, c]) = two_running();
    k.port().on(0);
    k.task_delete(Some(b)).expect("delete b from core 0");
    assert_eq!(k.take_core_yields(), 0b10, "core 1 is told to switch away");
    // Not freed yet: core 1 is still executing it.
    k.check_tasks_waiting_termination();
    assert!(
        k.priority_of(Some(b)).is_ok(),
        "the TCB outlives its core's switch"
    );
    // Core 1 switches; `b` is in no ready list, so it takes `c`.
    switch(&mut k, 1);
    assert_eq!(k.current_on(1), c);
    // Now nothing holds `b`, and the reaper frees it.
    k.check_tasks_waiting_termination();
    assert!(
        k.priority_of(Some(b)).is_err(),
        "reaped once no core holds it"
    );
}

#[test]
fn two_deferred_deletions_are_both_reaped() {
    let (mut k, _, [a, b, _c]) = two_running();
    k.port().on(0);
    k.task_delete(Some(b)).expect("b");
    k.task_delete(None).expect("a deletes itself");
    switch(&mut k, 0);
    switch(&mut k, 1);
    k.check_tasks_waiting_termination();
    assert!(k.priority_of(Some(a)).is_err());
    assert!(k.priority_of(Some(b)).is_err());
}

/// The second reap slot is reaped on its own: core 1 lets go of `b` first,
/// which empties the FIRST slot while `a` still waits in the second for
/// core 0 (plan P5: the second slot's own test survived every oracle).
#[test]
fn the_second_reap_slot_is_reaped_after_the_first_empties() {
    let (mut k, _, [a, b, _c]) = two_running();
    k.port().on(0);
    k.task_delete(Some(b)).expect("b");
    k.task_delete(None).expect("a deletes itself");
    switch(&mut k, 1);
    k.check_tasks_waiting_termination();
    assert!(
        k.priority_of(Some(b)).is_err(),
        "b, in the first slot, reaped"
    );
    assert!(k.priority_of(Some(a)).is_ok(), "a is still core 0's");
    switch(&mut k, 0);
    k.check_tasks_waiting_termination();
    assert!(
        k.priority_of(Some(a)).is_err(),
        "a, alone in the second slot, reaped"
    );
}

#[test]
fn suspending_a_task_another_core_is_running_yields_that_core() {
    let (mut k, _, [_a, b, c]) = two_running();
    k.port().on(0);
    k.suspend(Some(b)).expect("suspend b");
    assert_eq!(k.take_core_yields(), 0b10);
    switch(&mut k, 1);
    assert_eq!(k.current_on(1), c, "a suspended task is not selected");
}

#[test]
fn lowering_a_running_task_on_another_core_yields_that_core() {
    let (mut k, _, [_a, b, c]) = two_running();
    let d = {
        k.port().on(0);
        k.create_task("d", 3).expect("d")
    };
    // `d` at 3 was readied with both cores at 3: nobody is preempted.
    assert_eq!(k.take_core_yields(), 0);
    // Lower `b` (running on core 1) to 1: `taskYIELD_TASK_CORE` -- core 1.
    k.set_priority(Some(b), 1).expect("lower b");
    assert_eq!(k.take_core_yields(), 0b10);
    switch(&mut k, 1);
    assert_eq!(
        k.current_on(1),
        d,
        "core 1 takes the waiting priority-3 task"
    );
    let _ = c;
}

#[test]
fn raising_a_ready_task_preempts_the_lowest_core() {
    let mut k = kernel::<SmpConfig>();
    let a = k.create_task("a", 3).expect("a");
    let b = k.create_task("b", 2).expect("b");
    let c = k.create_task("c", 1).expect("c");
    k.start_scheduler().expect("start");
    switch(&mut k, 0);
    switch(&mut k, 1);
    assert_eq!((k.current_on(0), k.current_on(1)), (a, b));
    let _ = k.take_core_yields();
    // Raise the ready `c` above `b`: `taskYIELD_ANY_CORE` picks core 1.
    k.port().on(0);
    k.set_priority(Some(c), 4).expect("raise c");
    assert_eq!(k.take_core_yields(), 0b10);
    switch(&mut k, 1);
    assert_eq!(k.current_on(1), c);
}

#[test]
fn resuming_a_task_wakes_the_lowest_core() {
    let (mut k, started, [_a, b, _c]) = two_running();
    let hi = {
        k.port().on(0);
        let hi = k.create_task("hi", 4).expect("hi");
        // `hi` outranks both; core 1 (the higher-numbered of two equal
        // candidates) is asked.
        assert_eq!(k.take_core_yields(), 0b10);
        switch(&mut k, 1);
        assert_eq!(k.current_on(1), hi);
        // Park it, and let core 1 go back to `b`.
        k.port().on(1);
        k.suspend(None).expect("hi suspends itself");
        switch(&mut k, 1);
        hi
    };
    assert_eq!(k.current_on(1), b);
    let _ = k.take_core_yields();
    k.port().on(0);
    k.resume(hi).expect("resume hi");
    assert_eq!(
        k.take_core_yields(),
        0b10,
        "the resumed task preempts core 1"
    );
    let _ = started;
}

#[test]
fn a_notification_wakes_its_waiter_on_another_core() {
    let (mut k, _, [a, b, c]) = two_running();
    // `b` waits for a notification on core 1, which then runs `c`.
    k.port().on(1);
    let w = k.notify_take(0, true, 100).expect("take");
    assert!(matches!(w, crate::queue::Wait::Blocked));
    switch(&mut k, 1);
    assert_eq!(k.current_on(1), c);
    let _ = k.take_core_yields();
    // Core 0 notifies `b`: it outranks `c` (core 1), not `a` (core 0).
    k.port().on(0);
    k.notify(b, 0, 1, crate::kernel::NotifyAction::Increment)
        .expect("notify");
    assert_eq!(k.take_core_yields(), 0b10);
    switch(&mut k, 1);
    assert_eq!(k.current_on(1), b);
    assert_eq!(k.current_on(0), a, "the notifier keeps running");
}

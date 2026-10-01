//! # `xiao-s3-realtime` — every Kairos kernel feature at once, measured in real time
//!
//! One firmware running a realistic workload on a XIAO ESP32-S3: twelve
//! application tasks plus idle and the timer daemon, on the SHIPPED Xtensa
//! port — real stacks, a `SYSTIMER` tick interrupt, `Software0` committing
//! every switch — with each feature carrying its own real-time measurement.
//!
//! | feature | how it is exercised | measured |
//! |---|---|---|
//! | notify / queue / semaphore `_from_isr` | `SYSTIMER` alarm 1, 997 us, wakes three priority-5 tasks in turn | interrupt -> task latency, per mechanism |
//! | `delay_until`, preemption | a 2 ms control loop at priority 4 doing fixed work | release jitter, deadline misses |
//! | software timers + daemon | a 5 ms auto-reload timer, callback through `TickHook::timer` | callback jitter |
//! | mutex + priority inheritance | high / medium hog / low holding the mutex, on co-prime periods | high's worst blocking, inheritance observed |
//! | event groups | three tasks rendezvous through `event_group_sync` every 20 ms | release skew |
//! | message buffers | sequence-numbered, timestamped 16-byte messages every 1 ms | send -> receive latency, loss, corruption |
//! | idle | the core halts (`waiti`) between events | CPU load |
//!
//! ## The clock
//!
//! Xtensa `ccount`: one cycle of resolution at 240 MHz (4.17 ns), 32 bits, so
//! it wraps every 17.9 s — every delta here is a `wrapping_sub` over an
//! interval far shorter than that. `esp_hal::time::Instant` has 1 us, 240
//! cycles, larger than most numbers this firmware reports.
//!
//! ## What a latency here does and does not include
//!
//! Interrupt -> task is stamped at the FIRST statement of the alarm-1 handler
//! and again in the woken task. It therefore includes the kernel's `_from_isr`
//! call, the handler's return, `Software0`'s entry, the switch and the task's
//! resume — everything Kairos and its port contribute — and EXCLUDES the
//! hardware's own vector entry before that first statement, which no software
//! clock on this part can see.
//!
//! ## The warm-up
//!
//! Nothing is recorded for the first `WARMUP` control-loop releases: the
//! first round of every task pays one-off costs (first switch into a fresh
//! stack, cold instruction cache) that are not the steady state being claimed.


#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use esp_backtrace as _;
use esp_hal::time::Duration;
use esp_hal::timer::Timer;
use esp_hal::timer::systimer::{Alarm, SystemTimer};
use esp_println::println;
use rusty_rtos_core::port::Port;
use rusty_rtos_port_xtensa::{
    Context, XtensaPort, clear_switch_request, enable_switching, new_task_context, switch_context,
    yield_now,
};
use xtensa_lx::timer::get_cycle_count;

esp_bootloader_esp_idf::esp_app_desc!();

// The tasks, the timer daemon, the hook, the instruments and the report. The
// same file runs on OS threads in `host/`, as a functional check before a flash.
include!("workload.rs");

// ------------------------------------------------- what the workload needs --

/// Cycles per microsecond at the S3's 240 MHz.
const CYC_PER_US: u32 = 240;
const RUN_MS: u32 = if cfg!(feature = "long") { 600_000 } else { 20_000 };
/// A cycle counter on silicon: every timing check means what it says.
const REALTIME: bool = true;

fn cycles() -> u32 {
    get_cycle_count()
}

/// `SYSTIMER`'s 64-bit microsecond count, which does not wrap in a run --
/// `ccount` does, every 17.9 s.
fn wall_us() -> u64 {
    esp_hal::time::Instant::now().duration_since_epoch().as_micros()
}

/// The port's idle: `waiti`, a stateless halt until the next interrupt.
fn idle_wait() {
    IDLE_PORT.idle();
}

fn report_done(_passed: bool) -> ! {
    loop {
        delay(1_000_000);
    }
}

const CONTEXTS: usize = TASKS + 1;
const MAIN: usize = TASKS;

type K = Kernel<
    CellConfig,
    XtensaPort,
    NoTrace,
    RtHook,
    TASKS,
    ITEMS,
    LISTS,
    QUEUES,
    SLOTS,
    BUFFERS,
    BYTES,
    TIMERS,
    GROUPS,
    TIMER_CMDS,
>;

struct KernelCell(UnsafeCell<Option<K>>);
// SAFETY: every access goes through `with_kernel`, which masks interrupts, or
// through an interrupt handler; the three handlers here share one interrupt
// level, so none can preempt another, and there is one core.
#[allow(unsafe_code)]
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(UnsafeCell::new(None));

/// Borrow the kernel with interrupts masked.
fn with_kernel<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    critical_section::with(|_| {
        // SAFETY: interrupts are masked, so no handler holds this, and there is
        // no second core.
        #[allow(unsafe_code)]
        let slot = unsafe { &mut *KERNEL.0.get() };
        slot.as_mut().map(f)
    })
}

/// As `with_kernel`, from inside an interrupt, which already has exclusivity.
fn with_kernel_in_isr<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    // SAFETY: a handler cannot overlap a critical section on one core, and the
    // handlers here share one level, so none preempts another.
    #[allow(unsafe_code)]
    let slot = unsafe { &mut *KERNEL.0.get() };
    slot.as_mut().map(f)
}

// ------------------------------------------------------- stacks and switching --

#[repr(align(16))]
struct Stack(#[allow(dead_code)] [u8; 8192]);

static mut STACKS: [Stack; TASKS] = [const { Stack([0; 8192]) }; TASKS];
static mut CONTEXTS_STORE: [Context; CONTEXTS] =
    [const { unsafe { core::mem::zeroed() } }; CONTEXTS];
/// Which context is on the CPU. The switch handler is its only writer.
static CURRENT: AtomicU32 = AtomicU32::new(MAIN as u32);

/// The port's idle. A second instance is fine: `idle` is stateless.
static IDLE_PORT: XtensaPort = XtensaPort::new();

struct AlarmCell(UnsafeCell<Option<Alarm<'static>>>);
// SAFETY: written once by `main` before its interrupt is enabled; afterwards
// only that alarm's own handler touches it.
#[allow(unsafe_code)]
unsafe impl Sync for AlarmCell {}
static TICK_ALARM: AlarmCell = AlarmCell(UnsafeCell::new(None));
static IRQ_ALARM: AlarmCell = AlarmCell(UnsafeCell::new(None));

fn clear_alarm(cell: &AlarmCell) {
    // SAFETY: see `AlarmCell`.
    #[allow(unsafe_code)]
    let slot = unsafe { &mut *cell.0.get() };
    if let Some(alarm) = slot.as_mut() {
        alarm.clear_interrupt();
    }
}

/// `Software0`: the switch. The port raises it; the kernel decides.
#[esp_hal::ram]
#[unsafe(export_name = "Software0")]
fn switching_interrupt(trap_frame: &mut Context) {
    clear_switch_request();
    let from = CURRENT.load(Ordering::Acquire) as usize;
    let to = with_kernel_in_isr(|k| {
        k.switch_context();
        k.current().index() as usize
    })
    .unwrap_or(from);
    if from == to || to >= CONTEXTS {
        return;
    }
    note_switch(from, to);
    CURRENT.store(to as u32, Ordering::Release);
    // SAFETY: single core; this handler is the only reader or writer of the
    // store while a switch is in progress, and both indices are < CONTEXTS.
    #[allow(unsafe_code)]
    unsafe {
        let base = (&raw mut CONTEXTS_STORE).cast::<Context>();
        switch_context(Some(base.add(from)), base.add(to), trap_frame);
    }
}

/// `SYSTIMER` alarm 0: the 1 kHz tick.
#[esp_hal::handler]
fn tick_interrupt() {
    clear_alarm(&TICK_ALARM);
    if with_kernel_in_isr(Kernel::increment_tick).unwrap_or(false) {
        yield_now();
    }
}

/// `SYSTIMER` alarm 1: the interrupt source, every `IRQ_PERIOD_US`. The stamp
/// is the handler's first statement; everything after it is in the latency.
#[esp_hal::handler]
fn irq_source() {
    let stamp = get_cycle_count();
    clear_alarm(&IRQ_ALARM);
    if irq_fire(stamp) {
        yield_now();
    }
}

extern "C" fn task_entry(task_fn: usize, param: usize) {
    // SAFETY: `task_fn` is always one of the workload's task bodies, whose
    // signature this matches.
    #[allow(unsafe_code)]
    let entry: TaskFn = unsafe { core::mem::transmute(task_fn) };
    entry(param);
}

// ----------------------------------------------------------------- startup --

fn arm_task(handle: TaskHandle, body: TaskFn, param: usize) -> bool {
    let i = handle.index() as usize;
    if i >= TASKS {
        return false;
    }
    // SAFETY: called from `main` before any task runs and before any interrupt
    // is enabled; the stack is a `static` that outlives every task, and
    // `i < TASKS < CONTEXTS`.
    #[allow(unsafe_code)]
    unsafe {
        let stack = (&raw mut STACKS).cast::<Stack>().add(i);
        let top = stack.cast::<u8>().add(size_of::<Stack>());
        let store = (&raw mut CONTEXTS_STORE).cast::<Context>();
        store.add(i).write(new_task_context(task_entry, body as *const () as usize, param, top));
    }
    true
}

fn halt(why: &str) -> ! {
    println!("RESULT: FAIL -- {why}");
    loop {
        core::hint::spin_loop();
    }
}

fn start_alarm(alarm: &Alarm<'static>, period_us: u64, handler: esp_hal::interrupt::InterruptHandler) -> bool {
    alarm.set_interrupt_handler(handler);
    alarm.enable_auto_reload(true);
    if alarm.load_value(Duration::from_micros(period_us)).is_err() {
        return false;
    }
    alarm.enable_interrupt(true);
    alarm.start();
    true
}

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    println!();
    println!("=== xiao-s3-realtime: every Kairos kernel feature, measured on silicon ===");
    println!("run {RUN_MS} ms after warm-up; 12 application tasks + idle + timer daemon");

    let mut kernel = match K::new(XtensaPort::new(), NoTrace) {
        Ok(k) => k,
        Err(e) => {
            println!("kernel refused the geometry: {e:?}");
            halt("kernel geometry");
        }
    };

    let tasks = match build_workload(&mut kernel) {
        Ok(t) => t,
        Err(e) => {
            println!("creating the workload failed: {e:?}");
            halt("workload creation");
        }
    };
    let started = match kernel.start_scheduler() {
        Ok(s) => s,
        Err(e) => {
            println!("start_scheduler failed: {e:?}");
            halt("scheduler start");
        }
    };

    H_TIMER_Q.store(started.timer_queue.to_raw(), Ordering::Relaxed);
    IDLE_SLOT.store(started.idle.index(), Ordering::Relaxed);

    // EVERY task needs a stack, including the two the kernel creates itself:
    // the switch would otherwise load a zeroed context.
    let mut armed = arm_task(started.idle, task_idle, 0)
        && arm_task(started.timer, task_timer, 0);
    for (handle, body) in tasks {
        armed &= arm_task(handle, body, 0);
    }
    if !armed {
        halt("a task handle fell outside the context table");
    }

    // SAFETY: nothing else holds the kernel yet, and no interrupt that could
    // reach it is enabled until the lines below.
    #[allow(unsafe_code)]
    unsafe {
        *KERNEL.0.get() = Some(kernel);
    }

    let systimer = SystemTimer::new(peripherals.SYSTIMER);
    // Each alarm goes into the cell its handler clears BEFORE it is started. A
    // handler that ran first would find the cell empty, skip the clear, and --
    // the interrupt being level-held until cleared -- re-enter for ever.
    // SAFETY: neither alarm's interrupt is enabled yet, so nothing else can be
    // reading either cell.
    #[allow(unsafe_code)]
    let (tick, irq) = unsafe {
        *TICK_ALARM.0.get() = Some(systimer.alarm0);
        *IRQ_ALARM.0.get() = Some(systimer.alarm1);
        ((*TICK_ALARM.0.get()).as_ref(), (*IRQ_ALARM.0.get()).as_ref())
    };
    let started_ok = match (tick, irq) {
        (Some(tick), Some(irq)) => {
            start_alarm(tick, TICK_US, tick_interrupt) && start_alarm(irq, IRQ_PERIOD_US, irq_source)
        }
        _ => false,
    };
    if !started_ok {
        halt("a SYSTIMER alarm refused its period");
    }

    enable_switching();
    println!("entering the scheduler...");
    // Into the scheduler: `main`'s own context is saved into slot `MAIN`.
    yield_now();
    halt("the first switch never left main");
}

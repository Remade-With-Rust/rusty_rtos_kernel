//! # `xiao-s3-realtime-host` — the real-time workload, run for FUNCTION on OS threads
//!
//! `xiao-s3-realtime` puts every Kairos kernel feature on a XIAO ESP32-S3 at
//! once and times it. This cell runs **the same workload file, unchanged**
//! (`include!`d from that cell's `src/workload.rs`) on `rusty_rtos_port-host`:
//! one OS thread per task, a single run permit, a tick thread that freezes the
//! running task to preempt it.
//!
//! What a pass here establishes, before anything is flashed:
//!
//! * every task, the timer daemon and the interrupt source start and run;
//! * every feature produces measurements — nothing is silently dead;
//! * no kernel stall, no failed kernel call, no refused `_from_isr` call;
//! * every interrupt reaches its task; no message is lost or corrupted;
//! * the mutex is really contended and the holder is seen at an inherited
//!   priority.
//!
//! What it does NOT establish: anything about time. The clock is the
//! operating system's, the tick thread is at its mercy, and a "latency" here
//! is mostly the OS scheduler. The two timing checks print `skip`, and the
//! histograms are in microseconds of wall time — read them as "it ran", not
//! as a measurement.
//!
//! The interrupt source fires from the tick thread, every tick, as the
//! port's only interrupt context; on silicon it is its own `SYSTIMER` alarm.

use std::cell::UnsafeCell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rusty_rtos_core::port::Port as _;
use rusty_rtos_port_host::{init_task, pend_switch, set_scheduler, start_first_task, HostPort, Ticker, CURRENT};

include!("../../xiao-s3-realtime/src/workload.rs");

// ------------------------------------------------- what the workload needs --

/// The host clock is `Instant`, read in microseconds: one unit per us.
const CYC_PER_US: u32 = 1;
/// Ten seconds: long enough for every period to come round hundreds of times.
const RUN_MS: u32 = 10_000;
/// The OS owns this clock, so the timing checks are skipped, not passed.
const REALTIME: bool = false;

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn cycles() -> u32 {
    wall_us() as u32
}

fn wall_us() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// There is no core to halt: hand the permit on, and come back when nothing
/// else is ready.
fn idle_wait() {
    pend_switch();
}

/// The latency decomposition is an S3 instrument; here it is switched off.
const DECOMPOSE: bool = false;

fn mark(_: Mark) {}

fn mark_done(_: usize, _: u32, _: u32, _: u32) {}

fn report_done(passed: bool) -> ! {
    std::process::exit(if passed { 0 } else { 1 })
}

type K = Kernel<
    CellConfig,
    HostPort,
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

/// The kernel, reached from every task thread and from the tick thread.
///
/// An `UnsafeCell` rather than a `Mutex` on purpose: the port's critical
/// section is the lock, exactly as on a chip.
struct KernelCell(UnsafeCell<Option<K>>);
// SAFETY: every access goes through `with_kernel`, which takes the port's
// critical section, or through `with_kernel_in_isr`, whose callers (the tick
// thread and the scheduler hook) already hold it. One task runs at a time by
// construction of the run permit.
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(UnsafeCell::new(None));

static PORT: HostPort = HostPort::new();

fn with_kernel<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    PORT.enter_critical();
    // SAFETY: the critical section is held; see `KernelCell`.
    let slot = unsafe { &mut *KERNEL.0.get() };
    let out = slot.as_mut().map(f);
    PORT.exit_critical();
    out
}

/// For a caller that already holds the critical section: the tick thread and
/// the scheduler hook, this port's interrupt context.
fn with_kernel_in_isr<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    // SAFETY: the caller holds the port's critical section; see `KernelCell`.
    let slot = unsafe { &mut *KERNEL.0.get() };
    slot.as_mut().map(f)
}

// ------------------------------------------------------------- the joints --

/// The port asks who is next; the kernel answers.
extern "C" fn pick_next() {
    let next = with_kernel_in_isr(|k| {
        k.switch_context();
        k.current()
    });
    if let Some(handle) = next {
        let to = handle.index() as usize;
        let from = CURRENT.swap(to, std::sync::atomic::Ordering::SeqCst);
        note_switch(from, to);
    }
}

/// One tick, from the tick thread with the critical section held and the
/// running task frozen. The interrupt source fires here too.
extern "C" fn on_tick() -> bool {
    let want = with_kernel_in_isr(|k| k.increment_tick()).unwrap_or(false);
    let woke = irq_fire(cycles());
    want | woke
}

fn fail(why: &str) -> ! {
    println!("RESULT: FAIL -- {why}");
    std::process::exit(1)
}

fn main() {
    let _ = cycles();
    println!();
    println!("=== xiao-s3-realtime-host: the S3 workload on OS threads, checked for function ===");
    println!(
        "run {RUN_MS} ms after warm-up; 12 application tasks + idle + timer daemon; \
         interrupt source every tick (on silicon: every {IRQ_PERIOD_US} us)"
    );

    let mut kernel = match K::new(HostPort::new(), NoTrace) {
        Ok(k) => k,
        Err(e) => fail(&format!("kernel refused the geometry: {e:?}")),
    };
    let tasks = match build_workload(&mut kernel) {
        Ok(t) => t,
        Err(e) => fail(&format!("creating the workload failed: {e:?}")),
    };
    let started = match kernel.start_scheduler() {
        Ok(s) => s,
        Err(e) => fail(&format!("start_scheduler failed: {e:?}")),
    };
    H_TIMER_Q.store(started.timer_queue.to_raw(), Ordering::Relaxed);
    IDLE_SLOT.store(started.idle.index(), Ordering::Relaxed);
    let first = kernel.current().index() as usize;

    // SAFETY: no task thread or tick thread exists yet.
    unsafe {
        *KERNEL.0.get() = Some(kernel);
    }

    init_task(started.idle.index() as usize, task_idle);
    init_task(started.timer.index() as usize, task_timer);
    for (handle, body) in tasks {
        init_task(handle.index() as usize, body);
    }

    set_scheduler(pick_next);
    Ticker::new(Duration::from_micros(TICK_US), on_tick).spawn();
    start_first_task(first);
}

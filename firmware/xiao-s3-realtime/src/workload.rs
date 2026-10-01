// The workload: every Kairos kernel feature at once, with its instruments.
//
// This file is `include!`d by TWO programs, so the tasks, the timer daemon,
// the hook, the instruments and the report exist exactly once:
//
//   src/main.rs   the XIAO ESP32-S3 firmware -- the real-time measurement
//   host/main.rs  the same workload on OS threads (rusty_rtos_port-host) --
//                 a FUNCTIONAL check only: every feature works, together,
//                 with nothing hung, lost or stalled. Its clock is a Windows
//                 or Linux scheduler, so its timing checks are skipped.
//
// The including program supplies, at the same module level:
//
//   type K                            the kernel, over CellConfig and RtHook
//   fn with_kernel / with_kernel_in_isr
//   fn cycles() -> u32, const CYC_PER_US: u32
//   fn wall_us() -> u64               a wall clock that does not wrap in a run
//   fn idle_wait()                    what the idle task does between events
//
// and its scheduler hook calls `note_switch(from, to)` on every switch, after
// publishing the idle task's slot in `IDLE_SLOT`.
//
// For the latency decomposition (`--features decompose` on the S3) it also
// supplies `const DECOMPOSE: bool`, `fn mark(Mark)` and
// `fn mark_done(slot, a, j, k)`; elsewhere they are no-ops.
//   const REALTIME: bool              whether timing checks are meaningful
//   const RUN_MS: u32
//   fn report_done(passed: bool) -> !
//   println!

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rusty_rtos_core::config::Config;
use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{EventGroupHandle, QueueHandle, StreamBufferHandle, TaskHandle, TimerHandle};
use rusty_rtos_core::hooks::TickHook;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::NoTrace;
use rusty_rtos_kernel_core::kernel::NotifyAction;
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::{Kernel, Stall, list_slots_for, lists_for};

// ------------------------------------------------------------- the workload --

/// One kernel tick, 1 kHz.
const TICK_US: u64 = 1_000;
/// Control-loop releases ignored before anything is recorded: the first round
/// of every task pays one-off costs that are not the steady state.
const WARMUP: u32 = 250;

/// The control loop: period, and the fixed work it does each release.
const CTRL_PERIOD: u64 = 2;
const CTRL_WORK_US: u32 = 200;
/// The interrupt source's period on silicon: 997 us, deliberately not a
/// multiple of the tick, so its phase against the tick sweeps every value.
const IRQ_PERIOD_US: u64 = 997;
/// The software timer.
const TIMER_PERIOD: u64 = 5;
/// Priority inheritance, STAGED rather than left to chance. Every `PI_PERIOD`
/// ticks: `lo` takes the mutex on tick T and holds it for `LO_HOLD_US`; `hi`
/// and `mid` both wake on T+1. `hi` blocks on the mutex, so `lo` must inherit
/// its priority -- or `mid`'s burst runs ahead of `lo`, and `hi` waits for it.
///
/// The first version used co-prime periods, 7 / 11 / 3 ticks, and a 500 us
/// hold, and at 240 MHz it never contended once in 2,858 tries: every task
/// wakes on a tick, `hi` outranks `lo` when they wake together, and a hold
/// shorter than a tick almost never spans the next one. It had only
/// contended at 80 MHz, where every spin took three times as long.
const PI_PERIOD: u64 = 12;
/// Where `hi` and `mid` sit in the round, ticks after `lo`.
const PI_OFFSET: u64 = 1;
const HI_HOLD_US: u32 = 50;
const MID_BURST_US: u32 = 3_000;
/// Over two ticks, so `lo` still holds the mutex when `hi` arrives on T+1
/// even on the host, where a tick runs long.
const LO_HOLD_US: u32 = 2_500;
/// Event-group rendezvous period.
const EV_PERIOD: u64 = 20;
/// Message-buffer send period, and one message's size.
const MB_PERIOD: u64 = 1;
const MSG: usize = 16;

/// A blocking wait with no timeout. The kernel masks it to its tick width.
const FOREVER: u64 = u64::MAX;

// Priorities, 0..=5.
const P_IRQ: u8 = 5;
const P_CTRL: u8 = 4;
const P_HI: u8 = 3;
const P_EV: u8 = 3;
const P_MID: u8 = 2;
const P_MB: u8 = 2;
const P_LO: u8 = 1;

// ------------------------------------------------------------ the kernel --

pub struct CellConfig;

impl Config for CellConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 6;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 4;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 4;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
}

/// Twelve application tasks, the idle task and the timer daemon.
const TASKS: usize = 14;
/// The timer daemon's command queue, the ISR queue, the ISR semaphore, the mutex.
const QUEUES: usize = 6;
const SLOTS: usize = 16;
const BUFFERS: usize = 1;
const BYTES: usize = 256;
const TIMERS: usize = 1;
const GROUPS: usize = 1;
const LISTS: usize = lists_for(CellConfig::MAX_PRIORITIES, QUEUES, GROUPS);
const ITEMS: usize = list_slots_for(TASKS, TIMERS, LISTS);
const TIMER_CMDS: usize = <CellConfig as Config>::TIMER_QUEUE_LENGTH;

/// The software timer's callback id, which `RtHook::timer` switches on.
const CB_PERIODIC: u16 = 1;

/// A task body, as both ports start one.
type TaskFn = extern "C" fn(usize) -> !;

/// Timer callbacks arrive here, on the daemon task, inside its kernel call.
#[derive(Debug, Clone, Copy, Default)]
pub struct RtHook;

impl TickHook<K> for RtHook {
    fn tick(self, _kernel: &mut K) -> Self {
        self
    }

    fn timer(_kernel: &mut K, _timer: TimerHandle, callback: u16, _id: u64) {
        if callback != CB_PERIODIC {
            return;
        }
        let now = cycles();
        let prev = TIMER_PREV.swap(now, Ordering::Relaxed);
        TIMER_FIRES.fetch_add(1, Ordering::Relaxed);
        if GO.load(Ordering::Relaxed) && prev != 0 {
            JIT_TIMER.record(deviation(now.wrapping_sub(prev), period_cycles(TIMER_PERIOD)));
        }
    }
}

/// A blocking kernel call, as a stacked task makes one.
///
/// The kernel answers `Blocked` when it parks the caller -- and it has
/// ALREADY yielded, as the C's `portYIELD_WITHIN_API` does: on a port that
/// commits the switch, the switch request is raised inside the call and taken
/// the instant `with_kernel` unmasks. So by the time `Blocked` is seen here
/// the task has been off the CPU and has been woken, and the right move is to
/// make THE SAME CALL AGAIN at once, which the kernel answers with the result.
///
/// No `yield_now()` between the two. An extra one is a second switch
/// interrupt on every wake: it lands inside every latency this firmware
/// measures, and between equal priorities it rotates the round robin a step
/// the C never takes.
fn block_on<T>(mut f: impl FnMut(&mut K) -> Result<Wait<T>>) -> Option<T> {
    loop {
        match with_kernel(|k| f(k))? {
            Ok(Wait::Ready(value)) => return Some(value),
            Ok(Wait::Blocked) => {}
            Err(_) => {
                KERNEL_ERRORS.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
    }
}

/// `vTaskDelay`. The kernel yields inside the call; see `block_on`.
fn delay(ticks: u64) {
    if !matches!(with_kernel(|k| k.delay(ticks)), Some(Ok(()))) {
        KERNEL_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `xTaskDelayUntil`. `false` when the deadline had already passed.
fn delay_until(last: &mut u64, period: u64) -> bool {
    let on_time = match with_kernel(|k| k.delay_until(last, period)) {
        Some(Ok(d)) => d,
        _ => {
            KERNEL_ERRORS.fetch_add(1, Ordering::Relaxed);
            true
        }
    };
    on_time
}

fn now_tick() -> u64 {
    with_kernel(|k| k.tick_count()).unwrap_or(0)
}

fn spin_us(us: u32) {
    let start = cycles();
    let n = us.saturating_mul(CYC_PER_US);
    while cycles().wrapping_sub(start) < n {
        core::hint::spin_loop();
    }
}

const fn period_cycles(ticks: u64) -> u32 {
    (ticks * TICK_US) as u32 * CYC_PER_US
}

fn deviation(actual: u32, nominal: u32) -> u32 {
    actual.abs_diff(nominal)
}

// ---------------------------------------------------------- the instrument --

/// A latency histogram in clock units, exact min and max kept beside it. Each
/// one has a single writer, so plain atomics suffice.
///
/// Two tiers: 1,024 fine bins of 32 units (133 ns on the S3) up to 32,768
/// units, then 1,024 coarse bins of 2,048 units (8.5 us) up to 2,129,920 units
/// (8.9 ms). The first version had the fine tier alone, 136 us in all, and
/// reported the overflow bin's EDGE as a percentile -- a p50 of 136.53 us
/// printed beside a min of 1,299.97. Anything that still overflows is
/// reported as the max, never as an edge.
const FINE: usize = 1024;
const BIN: u32 = 32;
const COARSE: usize = 1024;
const CBIN: u32 = 2048;
const NB: usize = FINE + COARSE;
const FINE_TOP: u32 = FINE as u32 * BIN;

const fn bin_of(units: u32) -> usize {
    if units < FINE_TOP {
        (units / BIN) as usize
    } else {
        let i = FINE + ((units - FINE_TOP) / CBIN) as usize;
        if i < NB { i } else { NB - 1 }
    }
}

const fn upper_edge(i: usize) -> u32 {
    if i < FINE {
        (i as u32 + 1) * BIN
    } else {
        FINE_TOP + (i - FINE) as u32 * CBIN + CBIN
    }
}

struct Hist {
    bins: [AtomicU32; NB],
    n: AtomicU32,
    min: AtomicU32,
    max: AtomicU32,
}

impl Hist {
    const fn new() -> Self {
        Self {
            bins: [const { AtomicU32::new(0) }; NB],
            n: AtomicU32::new(0),
            min: AtomicU32::new(u32::MAX),
            max: AtomicU32::new(0),
        }
    }

    fn record(&self, units: u32) {
        if let Some(bin) = self.bins.get(bin_of(units)) {
            bin.fetch_add(1, Ordering::Relaxed);
        }
        self.n.fetch_add(1, Ordering::Relaxed);
        self.min.fetch_min(units, Ordering::Relaxed);
        self.max.fetch_max(units, Ordering::Relaxed);
    }

    fn count(&self) -> u32 {
        self.n.load(Ordering::Relaxed)
    }

    /// The upper edge of the bin holding the `per_10k`-th fraction, clamped
    /// into `[min, max]`; the overflow bin answers with the max itself.
    fn percentile(&self, per_10k: u64) -> u32 {
        let n = u64::from(self.count());
        if n == 0 {
            return 0;
        }
        let target = (n * per_10k).div_ceil(10_000).max(1);
        let mut seen = 0u64;
        for (i, bin) in self.bins.iter().enumerate() {
            seen += u64::from(bin.load(Ordering::Relaxed));
            if seen >= target {
                let (lo, hi) = (self.min.load(Ordering::Relaxed), self.max.load(Ordering::Relaxed));
                return if i == NB - 1 { hi } else { upper_edge(i).clamp(lo, hi) };
            }
        }
        self.max.load(Ordering::Relaxed)
    }
}

static LAT_NOTIFY: Hist = Hist::new();
static LAT_QUEUE: Hist = Hist::new();
static LAT_SEM: Hist = Hist::new();
static JIT_CTRL: Hist = Hist::new();
static JIT_TIMER: Hist = Hist::new();
static PI_BLOCK: Hist = Hist::new();
static EV_SKEW: Hist = Hist::new();
static LAT_MB: Hist = Hist::new();

static GO: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

static IRQ_FIRED: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
static IRQ_GOT: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
static IRQ_REFUSED: AtomicU32 = AtomicU32::new(0);
static IRQ_SEM_STAMP: AtomicU32 = AtomicU32::new(0);

static CTRL_MISSES: AtomicU32 = AtomicU32::new(0);
static TIMER_PREV: AtomicU32 = AtomicU32::new(0);
static TIMER_FIRES: AtomicU32 = AtomicU32::new(0);
static PI_CONTENDED: AtomicU32 = AtomicU32::new(0);
static INHERIT_SEEN: AtomicU32 = AtomicU32::new(0);
static MID_ROUNDS: AtomicU32 = AtomicU32::new(0);
static LO_ROUNDS: AtomicU32 = AtomicU32::new(0);
static EV_STAMP: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
static MB_SENT: AtomicU32 = AtomicU32::new(0);
static MB_BAD: AtomicU32 = AtomicU32::new(0);
static KERNEL_ERRORS: AtomicU32 = AtomicU32::new(0);
static IDLE_US: AtomicU32 = AtomicU32::new(0);
/// Idle clock units not yet a whole microsecond, carried between switches.
static IDLE_REM: AtomicU32 = AtomicU32::new(0);
/// When the idle task last went onto the CPU, and whether it is there now.
static IDLE_SINCE: AtomicU32 = AtomicU32::new(0);
static IDLE_ON: AtomicBool = AtomicBool::new(false);
/// The idle task's slot, published by `main` before the first switch.
static IDLE_SLOT: AtomicU32 = AtomicU32::new(u32::MAX);
static IDLE_US_AT_GO: AtomicU32 = AtomicU32::new(0);
static TICK_AT_GO: AtomicU32 = AtomicU32::new(0);
static WALL_MS_AT_GO: AtomicU32 = AtomicU32::new(0);

// Handles, published as raw words: written once before the first switch, read
// by tasks and the interrupt source with no `unsafe` needed to reach them.
static H_IRQ_N: AtomicU32 = AtomicU32::new(0);
static H_IRQ_Q: AtomicU32 = AtomicU32::new(0);
static H_IRQ_S: AtomicU32 = AtomicU32::new(0);
static H_MUTEX: AtomicU32 = AtomicU32::new(0);
static H_GROUP: AtomicU32 = AtomicU32::new(0);
static H_MB: AtomicU32 = AtomicU32::new(0);
static H_TIMER: AtomicU32 = AtomicU32::new(0);
static H_TIMER_Q: AtomicU32 = AtomicU32::new(0);

fn task_h(a: &AtomicU32) -> TaskHandle {
    TaskHandle::from_raw(a.load(Ordering::Relaxed))
}
fn queue_h(a: &AtomicU32) -> QueueHandle {
    QueueHandle::from_raw(a.load(Ordering::Relaxed))
}

/// After `STOP`, a task stops working and sleeps for good.
fn park_if_stopped() {
    if STOP.load(Ordering::Relaxed) {
        loop {
            delay(1_000_000);
        }
    }
}

// ------------------------------------------------------ the interrupt source --

/// The points along interrupt -> task that `--features decompose` stamps, in
/// path order; see `src/decompose.rs`. The workload marks C and D; the S3
/// handlers mark the rest.
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum Mark {
    Cleared = 1,
    KernelIn = 2,
    KernelOut = 3,
    Raised = 4,
    SwIn = 5,
    SwKernelOut = 6,
    SwNoted = 7,
    SwOut = 8,
}

/// One firing of the interrupt source, called in interrupt context with the
/// stamp taken at its first statement. Wakes one of three priority-5 tasks,
/// rotating notify -> queue -> semaphore. `true` when a switch is owed.
fn irq_fire(stamp: u32) -> bool {
    if STOP.load(Ordering::Relaxed) || !GO.load(Ordering::Relaxed) {
        return false;
    }
    let n = IRQ_FIRED.iter().map(|c| c.load(Ordering::Relaxed)).sum::<u32>();
    let which = (n % 3) as usize;
    if let Some(c) = IRQ_FIRED.get(which) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    mark(Mark::KernelIn);
    let woken = with_kernel_in_isr(|k| match which {
        0 => k
            .notify_from_isr(task_h(&H_IRQ_N), 0, stamp, NotifyAction::Overwrite)
            .map(|(_, w)| w),
        1 => k.queue_send_from_isr(queue_h(&H_IRQ_Q), u64::from(stamp)),
        _ => {
            IRQ_SEM_STAMP.store(stamp, Ordering::Relaxed);
            k.semaphore_give_from_isr(queue_h(&H_IRQ_S))
        }
    });
    mark(Mark::KernelOut);
    match woken {
        Some(Ok(w)) => w.needed(),
        _ => {
            IRQ_REFUSED.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

// ---------------------------------------------------------------- the tasks --

/// `block_on` for the woken interrupt tasks: also the moment the task came
/// back from the call that blocked it (point J of the decomposition).
fn block_on_resumed<T>(mut f: impl FnMut(&mut K) -> Result<Wait<T>>) -> Option<(T, u32)> {
    let mut resumed = 0;
    loop {
        match with_kernel(|k| f(k))? {
            Ok(Wait::Ready(value)) => return Some((value, resumed)),
            Ok(Wait::Blocked) => {
                if DECOMPOSE {
                    resumed = cycles();
                }
            }
            Err(_) => {
                KERNEL_ERRORS.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
    }
}

fn record_irq(slot: usize, hist: &Hist, stamp: u32, resumed: u32) {
    let now = cycles();
    if GO.load(Ordering::Relaxed) {
        hist.record(now.wrapping_sub(stamp));
        if DECOMPOSE && resumed != 0 {
            mark_done(slot, stamp, resumed, now);
        }
        if let Some(c) = IRQ_GOT.get(slot) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }
}

extern "C" fn task_irq_notify(_: usize) -> ! {
    loop {
        if let Some((stamp, resumed)) = block_on_resumed(|k| k.notify_take(0, true, FOREVER)) {
            record_irq(0, &LAT_NOTIFY, stamp, resumed);
        }
        park_if_stopped();
    }
}

extern "C" fn task_irq_queue(_: usize) -> ! {
    loop {
        if let Some((stamp, resumed)) = block_on_resumed(|k| k.queue_receive(queue_h(&H_IRQ_Q), FOREVER)) {
            record_irq(1, &LAT_QUEUE, stamp as u32, resumed);
        }
        park_if_stopped();
    }
}

extern "C" fn task_irq_sem(_: usize) -> ! {
    loop {
        if let Some(((), resumed)) = block_on_resumed(|k| k.semaphore_take(queue_h(&H_IRQ_S), FOREVER)) {
            record_irq(2, &LAT_SEM, IRQ_SEM_STAMP.load(Ordering::Relaxed), resumed);
        }
        park_if_stopped();
    }
}

/// The control loop: a 2 ms `delay_until` doing fixed work. It also starts the
/// software timer, opens the measurement window, and ends the run.
extern "C" fn task_ctrl(_: usize) -> ! {
    let timer = TimerHandle::from_raw(H_TIMER.load(Ordering::Relaxed));
    if block_on(|k| k.timer_start(timer, FOREVER)).is_none() {
        println!("the software timer would not start");
    }
    let mut last = now_tick();
    let mut prev = cycles();
    let run_releases = RUN_MS / CTRL_PERIOD as u32;
    let mut releases: u32 = 0;
    loop {
        let on_time = delay_until(&mut last, CTRL_PERIOD);
        let now = cycles();
        releases = releases.saturating_add(1);
        if releases == WARMUP {
            TICK_AT_GO.store(now_tick() as u32, Ordering::Relaxed);
            WALL_MS_AT_GO.store((wall_us() / 1_000) as u32, Ordering::Relaxed);
            IDLE_US_AT_GO.store(IDLE_US.load(Ordering::Relaxed), Ordering::Relaxed);
            GO.store(true, Ordering::Release);
        } else if releases > WARMUP {
            JIT_CTRL.record(deviation(now.wrapping_sub(prev), period_cycles(CTRL_PERIOD)));
            if !on_time {
                CTRL_MISSES.fetch_add(1, Ordering::Relaxed);
            }
        }
        prev = now;
        spin_us(CTRL_WORK_US);
        if releases >= WARMUP.saturating_add(run_releases) {
            finish();
        }
    }
}

/// The `delay_until` anchor for a task `offset` ticks into the inheritance
/// round: the latest tick AT OR BEFORE now on that schedule.
///
/// Never later than now. `xTaskDelayUntil` reads a previous wake time ahead of
/// the tick count as a tick-count overflow, does not delay, and moves the
/// anchor on a period -- so an anchor set in the future never blocks again.
/// The first staged version started `hi` at `last = 1` on tick 0 and starved
/// every task below priority 3; the host twin caught it.
fn pi_anchor(offset: u64) -> u64 {
    // Make sure `now >= offset`, so the subtraction below cannot wrap.
    delay(PI_PERIOD);
    let now = now_tick();
    now - (now - offset) % PI_PERIOD
}

/// Priority inheritance, high: on T+1, take the mutex `lo` holds, hold it
/// briefly, give it.
extern "C" fn task_pi_hi(_: usize) -> ! {
    let mutex = queue_h(&H_MUTEX);
    let mut last = pi_anchor(PI_OFFSET);
    loop {
        delay_until(&mut last, PI_PERIOD);
        let t0 = cycles();
        if block_on(|k| k.semaphore_take(mutex, FOREVER)).is_some() {
            let waited = cycles().wrapping_sub(t0);
            if GO.load(Ordering::Relaxed) {
                PI_BLOCK.record(waited);
                // Anything over 20 us means the mutex was actually held.
                if waited > 20 * CYC_PER_US {
                    PI_CONTENDED.fetch_add(1, Ordering::Relaxed);
                }
            }
            spin_us(HI_HOLD_US);
            let _ = block_on(|k| k.semaphore_give(mutex));
        }
        park_if_stopped();
    }
}

/// Priority inheritance, medium: a CPU hog that would starve `lo` -- and with
/// it the mutex `hi` waits for -- if inheritance did not lift `lo` above it.
extern "C" fn task_pi_mid(_: usize) -> ! {
    let mut last = pi_anchor(PI_OFFSET);
    loop {
        delay_until(&mut last, PI_PERIOD);
        spin_us(MID_BURST_US);
        MID_ROUNDS.fetch_add(1, Ordering::Relaxed);
        park_if_stopped();
    }
}

/// Priority inheritance, low: on tick T, hold the mutex for a fixed amount of
/// work, and look at its own priority while it does -- above `P_LO` means it
/// inherited.
extern "C" fn task_pi_lo(_: usize) -> ! {
    let mutex = queue_h(&H_MUTEX);
    let mut last = pi_anchor(0);
    loop {
        delay_until(&mut last, PI_PERIOD);
        if block_on(|k| k.semaphore_take(mutex, FOREVER)).is_some() {
            let mut lifted = false;
            for _ in 0..10 {
                spin_us(LO_HOLD_US / 10);
                let p = with_kernel(|k| k.task_priority_get(None)).and_then(|r| r.ok());
                lifted |= p.is_some_and(|p| p > P_LO);
            }
            if lifted && GO.load(Ordering::Relaxed) {
                INHERIT_SEEN.fetch_add(1, Ordering::Relaxed);
            }
            let _ = block_on(|k| k.semaphore_give(mutex));
            LO_ROUNDS.fetch_add(1, Ordering::Relaxed);
        }
        park_if_stopped();
    }
}

/// Event groups: three tasks rendezvous every 20 ms; task 0 measures the skew
/// between the moments all three were released.
fn ev_body(me: usize) -> ! {
    let group = EventGroupHandle::from_raw(H_GROUP.load(Ordering::Relaxed));
    let mine = 1u32 << me;
    let mut last = now_tick();
    loop {
        delay_until(&mut last, EV_PERIOD);
        if block_on(|k| k.event_group_sync(group, mine, 0b111, FOREVER)).is_some() {
            if let Some(slot) = EV_STAMP.get(me) {
                slot.store(cycles(), Ordering::Relaxed);
            }
            if me == 0 {
                delay(2);
                let s: [u32; 3] =
                    core::array::from_fn(|i| EV_STAMP.get(i).map_or(0, |a| a.load(Ordering::Relaxed)));
                let rel = s.map(|v| v.wrapping_sub(s[0]) as i32);
                let lo = rel.iter().copied().min().unwrap_or(0);
                let hi = rel.iter().copied().max().unwrap_or(0);
                if GO.load(Ordering::Relaxed) {
                    EV_SKEW.record(hi.wrapping_sub(lo) as u32);
                }
            }
        }
        park_if_stopped();
    }
}
extern "C" fn task_ev_a(_: usize) -> ! {
    ev_body(0)
}
extern "C" fn task_ev_b(_: usize) -> ! {
    ev_body(1)
}
extern "C" fn task_ev_c(_: usize) -> ! {
    ev_body(2)
}

/// Message buffer, sender: a sequence number and a timestamp every 1 ms.
extern "C" fn task_mb_tx(_: usize) -> ! {
    let mb = StreamBufferHandle::from_raw(H_MB.load(Ordering::Relaxed));
    let mut seq: u32 = 0;
    loop {
        delay(MB_PERIOD);
        seq = seq.wrapping_add(1);
        let mut msg = [0xA5u8; MSG];
        msg[0..4].copy_from_slice(&seq.to_le_bytes());
        msg[4..8].copy_from_slice(&cycles().to_le_bytes());
        if block_on(|k| k.stream_buffer_send(mb, &msg, FOREVER)) == Some(MSG) {
            MB_SENT.fetch_add(1, Ordering::Relaxed);
        }
        park_if_stopped();
    }
}

/// Message buffer, receiver: latency, and every message checked for loss
/// (a sequence gap) and corruption (wrong length or a damaged pad).
extern "C" fn task_mb_rx(_: usize) -> ! {
    let mb = StreamBufferHandle::from_raw(H_MB.load(Ordering::Relaxed));
    let mut expect: u32 = 1;
    loop {
        let mut out = [0u8; MSG];
        let got = block_on(|k| k.stream_buffer_receive(mb, &mut out, FOREVER));
        let now = cycles();
        let seq = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
        let stamp = u32::from_le_bytes([out[4], out[5], out[6], out[7]]);
        let intact = got == Some(MSG) && out[8..MSG].iter().all(|b| *b == 0xA5) && seq == expect;
        if GO.load(Ordering::Relaxed) {
            if intact {
                LAT_MB.record(now.wrapping_sub(stamp));
            } else {
                MB_BAD.fetch_add(1, Ordering::Relaxed);
            }
        }
        expect = seq.wrapping_add(1);
        park_if_stopped();
    }
}

/// Idle: wait for the next event. Its time is counted by `note_switch`.
extern "C" fn task_idle(_: usize) -> ! {
    loop {
        idle_wait();
    }
}

/// Idle-time accounting, called by the scheduler hook on every switch: the
/// shape of FreeRTOS's run-time stats, which count at `traceTASK_SWITCHED_IN`.
///
/// It cannot be done from inside the idle task. Idle halts, the interrupt
/// that ends the halt switches straight to the woken task, and idle's next
/// statement runs only once every other task is done -- so timing the halt
/// from idle counts the whole busy period as idle time. The first version
/// did exactly that and reported a CPU load of 0.0% under a 10% control loop.
///
/// Its cost lands inside every switch, and so inside every interrupt -> task
/// latency: a handful of instructions, the price of knowing the load.
fn note_switch(from: usize, to: usize) {
    if from == to {
        return;
    }
    let idle = IDLE_SLOT.load(Ordering::Relaxed) as usize;
    if from == idle && IDLE_ON.load(Ordering::Relaxed) {
        let units = IDLE_REM.load(Ordering::Relaxed)
            .wrapping_add(cycles().wrapping_sub(IDLE_SINCE.load(Ordering::Relaxed)));
        IDLE_US.fetch_add(units / CYC_PER_US, Ordering::Relaxed);
        IDLE_REM.store(units % CYC_PER_US, Ordering::Relaxed);
        IDLE_ON.store(false, Ordering::Relaxed);
    }
    if to == idle {
        IDLE_SINCE.store(cycles(), Ordering::Relaxed);
        IDLE_ON.store(true, Ordering::Relaxed);
    }
}

/// The timer daemon, `prvTimerTask`, as a straight loop on a stacked task.
/// Its callbacks reach `RtHook::timer` from inside `process_expired_timer`.
extern "C" fn task_timer(_: usize) -> ! {
    let queue = queue_h(&H_TIMER_Q);
    loop {
        let (next, mut empty) = with_kernel(|k| k.timer_next_expire()).unwrap_or((0, true));
        let expired = with_kernel(|k| {
            k.suspend_all();
            let (now, switched) = k.timer_sample_time_now().unwrap_or((0, false));
            if switched {
                let _ = k.resume_all();
                None
            } else if !empty && next <= now {
                let _ = k.resume_all();
                Some(now)
            } else {
                if empty {
                    empty = k.overflow_timer_list_is_empty();
                }
                let _ = k.wait_for_message_restricted(queue, next.wrapping_sub(now), empty);
                if !k.resume_all() {
                    k.task_yield();
                }
                None
            }
        })
        .flatten();
        if let Some(now) = expired {
            let _ = with_kernel(|k| k.process_expired_timer(next, now));
        }
        while matches!(with_kernel(|k| k.process_one_timer_command(0)), Some(Ok(Wait::Ready(true)))) {}
    }
}

// ------------------------------------------------------------ construction --

/// Create every object and the twelve application tasks, and publish the
/// handles. The caller starts the scheduler and gives each task its stack.
fn build_workload(kernel: &mut K) -> Result<[(TaskHandle, TaskFn); 12]> {
    let irq_q = kernel.queue_create(4)?;
    let irq_s = kernel.semaphore_create_binary()?;
    // `no-inherit` is the negative control: a binary semaphore, given once,
    // locks exactly like the mutex and has no priority inheritance. Both
    // inheritance checks must then FAIL -- which is what shows they can.
    let mutex = if cfg!(feature = "no-inherit") {
        let s = kernel.semaphore_create_binary()?;
        kernel.semaphore_give(s)?;
        s
    } else {
        kernel.mutex_create()?
    };
    let group = kernel.event_group_create()?;
    let mb = kernel.message_buffer_create(128)?;
    let timer = kernel.timer_create("rt5ms", TIMER_PERIOD, true, 0, CB_PERIODIC)?;
    H_IRQ_Q.store(irq_q.to_raw(), Ordering::Relaxed);
    H_IRQ_S.store(irq_s.to_raw(), Ordering::Relaxed);
    H_MUTEX.store(mutex.to_raw(), Ordering::Relaxed);
    H_GROUP.store(group.to_raw(), Ordering::Relaxed);
    H_MB.store(mb.to_raw(), Ordering::Relaxed);
    H_TIMER.store(timer.to_raw(), Ordering::Relaxed);

    let irq_n = kernel.create_task("irq_n", P_IRQ)?;
    H_IRQ_N.store(irq_n.to_raw(), Ordering::Relaxed);
    Ok([
        (irq_n, task_irq_notify as TaskFn),
        (kernel.create_task("irq_q", P_IRQ)?, task_irq_queue),
        (kernel.create_task("irq_s", P_IRQ)?, task_irq_sem),
        (kernel.create_task("ctrl", P_CTRL)?, task_ctrl),
        (kernel.create_task("pi_hi", P_HI)?, task_pi_hi),
        (kernel.create_task("ev_a", P_EV)?, task_ev_a),
        (kernel.create_task("ev_b", P_EV)?, task_ev_b),
        (kernel.create_task("ev_c", P_EV)?, task_ev_c),
        (kernel.create_task("pi_mid", P_MID)?, task_pi_mid),
        (kernel.create_task("mb_tx", P_MB)?, task_mb_tx),
        (kernel.create_task("mb_rx", P_MB)?, task_mb_rx),
        (kernel.create_task("pi_lo", P_LO)?, task_pi_lo),
    ])
}

// --------------------------------------------------------------- the report --

fn us(units: u32) -> (u32, u32) {
    let hundredths = u64::from(units) * 100 / u64::from(CYC_PER_US);
    ((hundredths / 100) as u32, (hundredths % 100) as u32)
}

fn row(name: &str, h: &Hist) {
    let n = h.count();
    if n == 0 {
        println!("  {name:<32} n=0  -- NO SAMPLES");
        return;
    }
    let (a, b) = us(h.min.load(Ordering::Relaxed));
    let (c, d) = us(h.percentile(5_000));
    let (e, f) = us(h.percentile(9_900));
    let (g, i) = us(h.percentile(9_990));
    let (j, l) = us(h.max.load(Ordering::Relaxed));
    println!(
        "  {name:<32} n={n:<7} min {a:>4}.{b:02}  p50 {c:>4}.{d:02}  p99 {e:>4}.{f:02}  p99.9 {g:>4}.{i:02}  max {j:>5}.{l:02} us"
    );
}

fn finish() -> ! {
    STOP.store(true, Ordering::Release);
    // Let every task reach its parking spot before reading anything.
    delay(50);

    // The window is measured on the WALL clock, because idle time is: a load
    // computed as wall-clock idle over a window counted in ticks is only right
    // where a tick is exactly a millisecond. On the host it is not, and the
    // first version read a load of 1.3% there.
    let window_ticks = (now_tick() as u32).wrapping_sub(TICK_AT_GO.load(Ordering::Relaxed));
    let window_ms = ((wall_us() / 1_000) as u32).wrapping_sub(WALL_MS_AT_GO.load(Ordering::Relaxed));
    let idle_ms = IDLE_US.load(Ordering::Relaxed).wrapping_sub(IDLE_US_AT_GO.load(Ordering::Relaxed)) / 1_000;
    let load_permille = 1_000u32.saturating_sub(idle_ms.saturating_mul(1_000) / window_ms.max(1));
    // One tick per millisecond, to within the two reads' skew. A tick handler
    // held off past the next alarm loses a tick, and this is where it shows.
    let ticks_kept = window_ticks.abs_diff(window_ms) <= 2 + window_ms / 10_000;
    let (stalls, why) = with_kernel(|k| (k.stalls(), k.first_stall())).unwrap_or((0, Stall::None));

    println!();
    println!("=== Kairos, every kernel feature at once ===");
    if cfg!(feature = "no-inherit") {
        println!("NEGATIVE CONTROL: the mutex is a binary semaphore; both inheritance checks must FAIL");
    }
    println!(
        "window  {window_ms} ms wall, {window_ticks} ticks, after {WARMUP} warm-up releases; CPU load {}.{}%",
        load_permille / 10,
        load_permille % 10
    );
    println!();
    println!("interrupt -> task (first handler statement to the woken task running):");
    row("notify_from_isr", &LAT_NOTIFY);
    row("queue_send_from_isr", &LAT_QUEUE);
    row("semaphore_give_from_isr", &LAT_SEM);
    println!("periodic timing, |actual interval - nominal|:");
    row("2 ms control loop (delay_until)", &JIT_CTRL);
    row("5 ms software timer callback", &JIT_TIMER);
    println!("synchronisation:");
    row("mutex wait, high priority", &PI_BLOCK);
    row("event_group_sync release skew", &EV_SKEW);
    row("message buffer send -> receive", &LAT_MB);
    println!();

    let fired: [u32; 3] = core::array::from_fn(|i| IRQ_FIRED.get(i).map_or(0, |a| a.load(Ordering::Relaxed)));
    let got: [u32; 3] = core::array::from_fn(|i| IRQ_GOT.get(i).map_or(0, |a| a.load(Ordering::Relaxed)));
    let misses = CTRL_MISSES.load(Ordering::Relaxed);
    let contended = PI_CONTENDED.load(Ordering::Relaxed);
    let inherited = INHERIT_SEEN.load(Ordering::Relaxed);
    let pi_max = PI_BLOCK.max.load(Ordering::Relaxed);
    // What `hi` may wait for with inheritance working: at most `lo`'s whole
    // critical section, plus up to two control-loop releases that outrank
    // both, plus 150 us of switches and margin -- 3,050 us. Without
    // inheritance `mid`'s 3 ms burst runs before the ~1.5 ms `lo` has left,
    // so `hi` waits at least 4,500 us and fails this.
    let pi_bound = (LO_HOLD_US + 2 * CTRL_WORK_US + 150) * CYC_PER_US;
    let errors = KERNEL_ERRORS.load(Ordering::Relaxed);
    let refused = IRQ_REFUSED.load(Ordering::Relaxed);
    let mb_bad = MB_BAD.load(Ordering::Relaxed);

    println!("counts:");
    println!(
        "  interrupts fired / handled   notify {}/{}  queue {}/{}  semaphore {}/{}",
        fired[0], got[0], fired[1], got[1], fired[2], got[2]
    );
    println!("  control loop deadline misses {misses}");
    println!("  software timer firings       {}", TIMER_FIRES.load(Ordering::Relaxed));
    println!(
        "  mutex contended / inherited  {contended} / {inherited}   (mid rounds {}, lo rounds {})",
        MID_ROUNDS.load(Ordering::Relaxed),
        LO_ROUNDS.load(Ordering::Relaxed)
    );
    println!("  messages sent / bad          {} / {mb_bad}", MB_SENT.load(Ordering::Relaxed));
    println!("  kernel stalls {stalls} (first {why:?}), call errors {errors}, ISR calls refused {refused}");
    println!();

    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        println!("      {}  {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failed += 1;
        }
    };
    let mut timing = |ok: bool, what: &str| {
        if REALTIME {
            check(ok, what);
        } else {
            println!("      skip  {what} (this clock is not real time)");
        }
    };
    let all = [&LAT_NOTIFY, &LAT_QUEUE, &LAT_SEM, &JIT_CTRL, &JIT_TIMER, &PI_BLOCK, &EV_SKEW, &LAT_MB];
    timing(ticks_kept, "the 1 kHz tick kept wall time: no tick lost");
    timing(misses == 0, "the 2 ms control loop never missed a deadline");
    timing(pi_max <= pi_bound, "priority inheritance bounded the high task's wait (no unbounded inversion)");
    check(all.iter().all(|h| h.count() > 0), "every feature produced measurements");
    check(stalls == 0 && errors == 0, "no kernel stalls and no failed kernel calls");
    check(refused == 0 && (0..3).all(|i| got[i] + 1 >= fired[i]), "every interrupt was delivered to its task");
    check(contended > 0, "the mutex really was contended (otherwise the bound proves nothing)");
    check(inherited > 0, "the low-priority holder was seen running at an inherited priority");
    check(mb_bad == 0, "no message lost, reordered or corrupted");

    println!();
    if failed == 0 {
        println!("RESULT: PASS -- every kernel feature ran at once and met its checks");
    } else {
        println!("RESULT: FAIL -- {failed} check(s) failed");
    }
    report_done(failed == 0)
}

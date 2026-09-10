//! K2's no-panic gate: every public call, with whatever arguments.
//!
//! The kernel denies `unwrap`, `expect` and `panic` on every path, and
//! forbids `unsafe`. That makes a panic reachable only through arithmetic
//! that overflows, an index that is out of range, or a slice that is
//! shorter than something assumed — and the lints catch the shapes, not the
//! reachability. This test goes after the reachability.
//!
//! It drives the whole public surface with a stream of arguments that a
//! caller would never produce: handles from other arenas, handles from
//! nothing at all, stale handles whose object has been deleted, indices
//! past the configured end, tick counts at both extremes, lengths larger
//! than the arenas. Nothing here checks that the kernel does the *right*
//! thing with them — that is what the conformance corpus is for. This
//! checks that it comes back at all.
//!
//! The generator is a xorshift with a fixed seed rather than a property
//! testing crate: it needs no dependency, the house doctrine prefers that,
//! and a failing run is reproducible from the seed printed in the message.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::fmt;

use rusty_rtos_core::config::{Config, PosixDemoConfig};
use rusty_rtos_core::handle::{QueueHandle, StreamBufferHandle, TaskHandle, TimerHandle};
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_kernel_core::kernel::NotifyAction;
use rusty_rtos_kernel_core::{Kernel, items_for, lists_for};

const TASKS: usize = 8;
const QUEUES: usize = 8;
const SLOTS: usize = 32;
const BUFFERS: usize = 4;
const BYTES: usize = 256;
const TIMERS: usize = 4;

/// The smallest port that satisfies the seam: it counts nesting so the
/// kernel's own bookkeeping has something real to talk to, and does nothing
/// else. The sim port is deliberately *not* used — this test is about the
/// kernel, and a second port implementation is one more thing that would
/// have to be wrong in the same way to hide a panic.
#[derive(Debug, Default)]
struct TestPort {
    nesting: core::cell::Cell<u32>,
    in_isr: core::cell::Cell<bool>,
}

impl Port for TestPort {
    fn yield_now(&self) {}
    fn yield_from_isr(&self, _woken: Woken) {}
    fn enter_critical(&self) {
        self.nesting.set(self.nesting.get().saturating_add(1));
    }
    fn exit_critical(&self) {
        self.nesting.set(self.nesting.get().saturating_sub(1));
    }
    fn set_interrupt_mask_from_isr(&self) -> u32 {
        0
    }
    fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
    fn in_isr(&self) -> bool {
        self.in_isr.get()
    }
    fn set_in_tick_entry(&self, yes: bool) {
        self.in_isr.set(yes);
    }
}

/// A trace sink that counts and keeps nothing, so a million calls cost
/// nothing but time.
#[derive(Default)]
struct Counting(u64);

impl Trace for Counting {
    fn event(&mut self, _tick: u64, _event: Event<'_>) {
        self.0 = self.0.wrapping_add(1);
    }
}

impl fmt::Debug for Counting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Counting({})", self.0)
    }
}

type K = Kernel<
    PosixDemoConfig,
    TestPort,
    Counting,
    NoTickHook,
    TASKS,
    { items_for(TASKS, TIMERS) },
    { lists_for(<PosixDemoConfig as Config>::MAX_PRIORITIES, QUEUES) },
    QUEUES,
    SLOTS,
    BUFFERS,
    BYTES,
    TIMERS,
>;

/// A fixed-size list of handles that are (probably) still live, so the
/// generator can aim at real objects as well as invented ones.
struct Live<T: Copy, const N: usize> {
    items: [T; N],
    len: usize,
}

impl<T: Copy, const N: usize> Live<T, N> {
    fn new(empty: T) -> Self {
        Self {
            items: [empty; N],
            len: 0,
        }
    }

    fn push(&mut self, item: T) {
        if let Some(slot) = self.items.get_mut(self.len) {
            *slot = item;
            self.len = self.len.saturating_add(1);
        }
    }

    fn all(&self) -> &[T] {
        self.items.get(..self.len).unwrap_or(&[])
    }
}

/// The generator: xorshift64*, seeded once and printed on failure.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next().checked_rem(n).unwrap_or(0)
        }
    }

    /// A tick count drawn from the values that break things: zero, one,
    /// the maximum, either side of the maximum, and a spread in between.
    fn ticks(&mut self) -> u64 {
        match self.below(8) {
            0 => 0,
            1 => 1,
            2 => u64::MAX,
            3 => u64::MAX - 1,
            4 => u64::from(u32::MAX),
            5 => 1 << 63,
            _ => self.next(),
        }
    }

    /// A handle that is as likely to be wrong as right: a live one, a
    /// plausible-looking index, the null handle, or pure noise.
    fn task(&mut self, live: &[TaskHandle]) -> TaskHandle {
        match self.below(4) {
            0 if !live.is_empty() => {
                let i = self.below(live.len() as u64) as usize;
                live.get(i).copied().unwrap_or(TaskHandle::NULL)
            }
            1 => TaskHandle::NULL,
            2 => TaskHandle::from_raw(self.next() as u32),
            _ => TaskHandle::from_raw((self.below(16) as u32) | ((self.below(4) as u32) << 16)),
        }
    }

    fn queue(&mut self, live: &[QueueHandle]) -> QueueHandle {
        match self.below(4) {
            0 if !live.is_empty() => {
                let i = self.below(live.len() as u64) as usize;
                live.get(i).copied().unwrap_or(QueueHandle::NULL)
            }
            1 => QueueHandle::NULL,
            2 => QueueHandle::from_raw(self.next() as u32),
            _ => QueueHandle::from_raw((self.below(16) as u32) | ((self.below(4) as u32) << 16)),
        }
    }

    fn timer(&mut self, live: &[TimerHandle]) -> TimerHandle {
        match self.below(4) {
            0 if !live.is_empty() => {
                let i = self.below(live.len() as u64) as usize;
                live.get(i).copied().unwrap_or(TimerHandle::NULL)
            }
            1 => TimerHandle::NULL,
            2 => TimerHandle::from_raw(self.next() as u32),
            _ => TimerHandle::from_raw((self.below(16) as u32) | ((self.below(4) as u32) << 16)),
        }
    }

    fn buffer(&mut self, live: &[StreamBufferHandle]) -> StreamBufferHandle {
        match self.below(4) {
            0 if !live.is_empty() => {
                let i = self.below(live.len() as u64) as usize;
                live.get(i).copied().unwrap_or(StreamBufferHandle::NULL)
            }
            1 => StreamBufferHandle::NULL,
            2 => StreamBufferHandle::from_raw(self.next() as u32),
            _ => StreamBufferHandle::from_raw(
                (self.below(16) as u32) | ((self.below(4) as u32) << 16),
            ),
        }
    }
}

/// What one pass actually managed to do, so a run that bounced off
/// invalid handles from start to finish cannot pass quietly.
#[derive(Debug, Default, Clone, Copy)]
struct Worked {
    /// Trace lines the kernel emitted.
    traced: u64,
    /// Ticks it counted.
    ticks: u64,
}

/// One pass: build a kernel, start it, then make `calls` arbitrary calls.
fn hammer(seed: u64, calls: u32) -> Worked {
    let mut rng = Rng(seed | 1);
    let mut k = K::new(TestPort::default(), Counting::default()).expect("the geometry adds up");

    let mut tasks = Live::<TaskHandle, TASKS>::new(TaskHandle::NULL);
    let mut queues = Live::<QueueHandle, QUEUES>::new(QueueHandle::NULL);
    let mut buffers = Live::<StreamBufferHandle, BUFFERS>::new(StreamBufferHandle::NULL);
    let mut timers = Live::<TimerHandle, TIMERS>::new(TimerHandle::NULL);

    for i in 0..3 {
        if let Ok(t) = k.create_task("h", (i % 5) as u8) {
            tasks.push(t);
        }
    }
    let started = k.start_scheduler().expect("the scheduler starts");
    tasks.push(started.idle);
    tasks.push(started.timer);
    queues.push(started.timer_queue);

    let mut data = [0_u8; 64];
    let mut out = [0_u8; 64];

    for _ in 0..calls {
        let ticks = rng.ticks();
        let task = rng.task(tasks.all());
        let queue = rng.queue(queues.all());
        let buffer = rng.buffer(buffers.all());
        let timer = rng.timer(timers.all());
        let index = rng.below(8) as usize;
        let value = rng.next();
        let length = rng.below(80) as usize;

        match rng.below(45) {
            0 => {
                if let Ok(t) = k.create_task("h", rng.below(300) as u8) {
                    tasks.push(t);
                }
            }
            1 => {
                if let Ok(q) = k.queue_create(rng.below(40) as usize) {
                    queues.push(q);
                }
            }
            2 => {
                if let Ok(q) =
                    k.semaphore_create_counting(rng.below(40) as usize, rng.below(40) as usize)
                {
                    queues.push(q);
                }
            }
            3 => {
                if let Ok(q) = k.mutex_create() {
                    queues.push(q);
                }
            }
            4 => {
                if let Ok(q) = k.mutex_create_recursive() {
                    queues.push(q);
                }
            }
            5 => {
                if let Ok(q) = k.queue_create_set(rng.below(40) as usize) {
                    queues.push(q);
                }
            }
            6 => {
                if let Ok(b) = k.stream_buffer_create(length, rng.below(80) as usize) {
                    buffers.push(b);
                }
            }
            7 => {
                if let Ok(b) = k.message_buffer_create(length) {
                    buffers.push(b);
                }
            }
            8 => {
                let _ = k.queue_send(queue, value, ticks);
            }
            9 => {
                let _ = k.queue_send_to_front(queue, value, ticks);
            }
            10 => {
                let _ = k.queue_overwrite(queue, value);
            }
            11 => {
                let _ = k.queue_receive(queue, ticks);
            }
            12 => {
                let _ = k.queue_peek(queue, ticks);
            }
            13 => {
                let _ = k.semaphore_take(queue, ticks);
            }
            14 => {
                let _ = k.semaphore_give(queue);
            }
            15 => {
                let _ = k.mutex_take_recursive(queue, ticks);
            }
            16 => {
                let _ = k.mutex_give_recursive(queue);
            }
            17 => {
                let _ = k.queue_messages_waiting(queue);
            }
            18 => {
                let _ = k.queue_spaces_available(queue);
            }
            19 => {
                let _ = k.queue_reset(queue);
            }
            20 => {
                let _ = k.queue_add_to_set(queue, rng.queue(queues.all()));
            }
            21 => {
                let _ = k.queue_remove_from_set(queue, rng.queue(queues.all()));
            }
            22 => {
                let _ = k.queue_select_from_set(queue, ticks);
            }
            23 => {
                let _ = k.queue_send_from_isr(queue, value);
            }
            24 => {
                let _ = k.queue_receive_from_isr(queue);
            }
            25 => {
                let _ = k.queue_peek_from_isr(queue);
            }
            26 => {
                let _ = k.delay(ticks);
            }
            27 => {
                let _ = k.suspend(if rng.below(2) == 0 { None } else { Some(task) });
            }
            28 => {
                let _ = k.resume(task);
            }
            29 => {
                let _ = k.set_priority(
                    if rng.below(2) == 0 { None } else { Some(task) },
                    rng.below(300) as u8,
                );
            }
            30 => {
                let _ = k.abort_delay(task);
            }
            31 => {
                let _ = k.notify(task, index, value as u32, action(rng.below(5)));
            }
            32 => {
                let n = length.min(data.len());
                for byte in data.get_mut(..n).unwrap_or(&mut []).iter_mut() {
                    *byte = value as u8;
                }
                let slice = data.get(..n).unwrap_or(&[]);
                let _ = k.stream_buffer_send(buffer, slice, ticks);
            }
            33 => {
                let n = length.min(out.len());
                let slice = out.get_mut(..n).unwrap_or(&mut []);
                let _ = k.stream_buffer_receive(buffer, slice, ticks);
            }
            34 => {
                if let Ok(t) =
                    k.timer_create("t", rng.below(40).max(1), rng.below(2) == 0, value, 0)
                {
                    timers.push(t);
                }
            }
            35 => {
                let _ = k.timer_start(timer, ticks);
            }
            36 => {
                let _ = k.timer_stop(timer, ticks);
            }
            37 => {
                let _ = k.timer_reset(timer, ticks);
            }
            38 => {
                let _ = k.timer_change_period(timer, rng.below(40), ticks);
            }
            39 => {
                let _ = k.timer_delete(timer, ticks);
            }
            40 => {
                // The four wrappers, not the raw command: the C's public
                // from-ISR API fills the value in itself, and a value from
                // anywhere else is a command time the daemon believes.
                match rng.below(4) {
                    0 => drop(k.timer_start_from_isr(timer)),
                    1 => drop(k.timer_stop_from_isr(timer)),
                    2 => drop(k.timer_reset_from_isr(timer)),
                    _ => drop(k.timer_change_period_from_isr(timer, rng.below(40).max(1))),
                }
            }
            41 => {
                let _ = k.timer_pend_function_call(0, value, value, ticks);
            }
            42 => {
                let _ = k.timer_is_active(timer);
            }
            43 => {
                let _ = k.timer_expiry_time(timer);
            }
            _ => {
                let _ = k.process_one_timer_command();
            }
        }

        // A handful of calls that take no arguments worth randomising, plus
        // the tick itself, so the kernel keeps moving rather than sitting
        // in whatever state the last call left it.
        match rng.below(10) {
            0 => k.tick_from_isr(),
            1 => {
                let _ = k.resume_pending();
            }
            2 => k.task_yield(),
            3 => {
                k.enter_critical();
                k.exit_critical();
            }
            4 => k.idle_hook_tick(),
            5 => {
                let _ = k.notify_wait(index, value as u32, (value >> 32) as u32, ticks);
            }
            6 => {
                let _ = k.notify_state_clear(None, index);
            }
            7 => {
                let _ = k.task_priority_get(None);
            }
            8 => {
                let _ = k.state_of(task);
            }
            _ => {
                let _ = k.stream_buffer_delete(buffer);
            }
        }
    }
    Worked {
        traced: k.trace().0,
        ticks: k.tick_count(),
    }
}

fn action(n: u64) -> NotifyAction {
    match n {
        0 => NotifyAction::None,
        1 => NotifyAction::SetBits,
        2 => NotifyAction::Increment,
        3 => NotifyAction::Overwrite,
        _ => NotifyAction::NoOverwrite,
    }
}

#[test]
fn no_call_panics_however_it_is_called() {
    // Sixty-four independent kernels, each taking four thousand arbitrary
    // calls: a quarter of a million calls over the whole public surface.
    // A failure names the seed, and a single seed reproduces it exactly.
    let mut total = Worked::default();
    for seed in 1..=64_u64 {
        let seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let worked = hammer(seed, 4_000);
        // Surviving is what this test is for. *Doing* something is what
        // the test itself could fail at: a run whose every call bounced
        // off an invalid handle would survive and prove nothing.
        assert!(
            worked.traced > 100,
            "seed {seed:#x}: only {} trace lines, so the calls bounced off              invalid handles instead of reaching the kernel",
            worked.traced
        );
        assert!(worked.ticks > 0, "seed {seed:#x}: time never moved");
        total.traced = total.traced.saturating_add(worked.traced);
        total.ticks = total.ticks.saturating_add(worked.ticks);
    }
    assert!(
        total.traced > 100_000,
        "the whole run traced only {} lines",
        total.traced
    );
}

#[test]
fn a_stale_handle_is_refused_rather_than_followed() {
    let mut k = K::new(TestPort::default(), Counting::default()).unwrap();
    let queue = k.queue_create(4).unwrap();
    let buffer = k.stream_buffer_create(16, 1).unwrap();
    let _ = k.start_scheduler().unwrap();

    k.stream_buffer_delete(buffer).unwrap();
    // The generation half of the handle has moved on, so the same index
    // does not name the same object.
    assert!(k.stream_buffer_bytes_available(buffer).is_err());
    assert!(k.stream_buffer_receive(buffer, &mut [0; 4], 0).is_err());

    // A queue handle is not a buffer handle even when the indices match.
    let raw = queue.to_raw();
    let as_buffer = StreamBufferHandle::from_raw(raw);
    assert!(k.stream_buffer_bytes_available(as_buffer).is_err());
}

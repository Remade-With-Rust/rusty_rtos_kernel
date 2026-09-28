#![no_std]
//! The Kairos arm of the flash comparison: the same kernel operations the C
//! arm's `root.c` pins, so a linker can be asked the same question of both.
//!
//! # The root set is the corpus, and that is the whole argument
//!
//! `docs/API-MAP.md` lists 341 FreeRTOS entry points, nearly all still
//! `planned` here. Linking the C kernel against all of them and Kairos
//! against the forty it implements would price a feature gap, not a kernel.
//! The operations below are the ones the conformance corpus exercises — the
//! one place the two kernels are known to do the SAME WORK, eighteen
//! scenarios byte-identical on four architectures. That is the only root set
//! a footprint comparison can honestly use, and it is the set `root.c`
//! names.
//!
//! # One exported function per operation, and why that matters
//!
//! The first version of this probe called every operation from a single
//! function. That is a bad instrument for two reasons, and both showed up in
//! the map: the optimiser inlined most of the kernel into that one function,
//! so **13,314 bytes were attributed to "the probe"** and the per-operation
//! split became meaningless; and one call site is not how a kernel is
//! actually used, so whatever inlining it produced was an artefact of the
//! harness rather than a property of the kernel.
//!
//! So each operation gets its own `extern "C"` entry point, exactly as the C
//! arm has one exported symbol per operation. The linker is then asked the
//! same question of both — keep what these entry points need, discard the
//! rest — and every byte lands in a section named after the operation that
//! caused it.
//!
//! The kernel arrives by pointer rather than being built here, so no entry
//! point is charged for a construction the C arm does not pay for either.

use rusty_rtos_core::config::{Config, PosixDemoConfig};

/// The config this probe links, matched to the C header the C arm compiles
/// against -- `bench/kernel-ram/c/FreeRTOSConfig.h`, which `run.sh` passes to
/// clang with `-include`.
///
/// It exists because the two arms were NOT configured the same way. The Rust
/// arm linked `PosixDemoConfig`, whose values are a hosted demo's, and the
/// differences gate CODE: that header sets `configUSE_QUEUE_SETS 0`, so the C
/// kernel compiles no `prvNotifyQueueSetContainer` at all, while our arm
/// carried the function and a `set_container` test on every send. The rv32
/// instrument already had a config matched to this same header and says why:
/// *"A comparison at two different geometries is not a comparison."*
///
/// Every field below names the line of that header it answers. Fields left to
/// `PosixDemoConfig` are ones where the two already agreed.
#[derive(Debug, Clone, Copy, Default)]
pub struct MatchedConfig;

impl Config for MatchedConfig {
    type Tick = <PosixDemoConfig as Config>::Tick;
    const TICK_RATE_HZ: u32 = <PosixDemoConfig as Config>::TICK_RATE_HZ;
    const DYNAMIC_ALLOCATION: bool = true;
    const PORT_STACK_INIT_CRITICAL: bool = true;
    /// `configMAX_PRIORITIES 5` -- :112. Worth +10 bytes to our arm, not a saving.
    const MAX_PRIORITIES: u8 = 5;
    const MINIMAL_STACK_SIZE: usize = 128;
    /// `configMAX_TASK_NAME_LEN 16` -- :122. Flash-neutral.
    const MAX_TASK_NAME_LEN: usize = 16;
    /// `configQUEUE_REGISTRY_SIZE 0` -- :156. Flash-neutral.
    const QUEUE_REGISTRY_SIZE: usize = 0;
    /// `configTIMER_TASK_PRIORITY ( configMAX_PRIORITIES - 1 )` -- :230.
    const TIMER_TASK_PRIORITY: u8 = 4;
    /// `configTIMER_QUEUE_LENGTH 10` -- :243. Flash-neutral.
    const TIMER_QUEUE_LENGTH: usize = 10;
    /// `configTIMER_TASK_STACK_DEPTH configMINIMAL_STACK_SIZE` -- :237.
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    /// `configCHECK_FOR_STACK_OVERFLOW 2` -- :365, and flash-NEUTRAL here,
    /// which is itself the finding: our kernel does not implement stack
    /// overflow checking, so the C arm compiles a check at level 2 that we
    /// have no code for. That is a FEATURE GAP in our favour and no config
    /// can close it -- unlike the three flags above, it is not a defect in
    /// the instrument but a difference the reader is owed.
    const CHECK_FOR_STACK_OVERFLOW: u8 = 2;
    /// `configUSE_TICK_HOOK 0` -- :341. Flash-neutral.
    const USE_TICK_HOOK: bool = false;
    /// `configTASK_NOTIFICATION_ARRAY_ENTRIES` defaults to 1. Worth -62 bytes.
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
    const MESSAGE_LENGTH_BYTES: usize = <PosixDemoConfig as Config>::MESSAGE_LENGTH_BYTES;
    const TOTAL_HEAP_SIZE: usize = <PosixDemoConfig as Config>::TOTAL_HEAP_SIZE;
    const MAX_TASKS: usize = <PosixDemoConfig as Config>::MAX_TASKS;
    const MAX_QUEUES: usize = <PosixDemoConfig as Config>::MAX_QUEUES;
    const MAX_TIMERS: usize = <PosixDemoConfig as Config>::MAX_TIMERS;
    const MAX_EVENT_GROUPS: usize = <PosixDemoConfig as Config>::MAX_EVENT_GROUPS;
    const MAX_STREAM_BUFFERS: usize = <PosixDemoConfig as Config>::MAX_STREAM_BUFFERS;
    /// `configUSE_QUEUE_SETS 0` -- FreeRTOSConfig.h:649, set explicitly.
    const USE_QUEUE_SETS: bool = false;
    /// `configUSE_TIME_SLICING 0` -- :92. Worth -34 bytes.
    const USE_TIME_SLICING: bool = false;
}
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::trace::NoTrace;
use rusty_rtos_core::handle::{
    EventGroupHandle, QueueHandle, StreamBufferHandle, TaskHandle, TimerHandle,
};
use rusty_rtos_kernel_core::kernel::{Kernel, NotifyAction};
use rusty_rtos_kernel_core::{list_slots_for, lists_for};
use rusty_rtos_port_riscv::RiscvPort;

const PRIOS: u8 = 5;
const TASKS: usize = 8;
const QUEUES: usize = 8;
const SLOTS: usize = 64;
const BUFFERS: usize = 4;
const BYTES: usize = 1024;
const TIMERS: usize = 16;
const GROUPS: usize = 2;

/// The real RISC-V port, not the sim one, because the C arm links `port.c`
/// and `portASM.S`. A kernel over a real port against a kernel over a
/// simulation would not be a comparison.
pub type K = Kernel<
    MatchedConfig,
    RiscvPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { list_slots_for(TASKS, TIMERS, lists_for(PRIOS, QUEUES, GROUPS)) },
    { lists_for(PRIOS, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    BUFFERS,
    BYTES,
    TIMERS,
    GROUPS,
    { <MatchedConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
>;

/// One entry point per operation.
///
/// `$name` becomes the exported symbol, so the map attributes every byte to
/// the operation that pulled it in. `k` is a pointer the caller owns; these
/// are measured, never run.
macro_rules! op {
    ($name:ident, |$k:ident| $body:expr) => {
        op!($name, |$k, _h, _t| $body);
    };
    ($name:ident, |$k:ident, $h:ident| $body:expr) => {
        op!($name, |$k, $h, _t, _v| $body);
    };
    ($name:ident, |$k:ident, $h:ident, $t:ident| $body:expr) => {
        op!($name, |$k, $h, $t, _v| $body);
    };
    ($name:ident, |$k:ident, $h:ident, $t:ident, $v:ident| $body:expr) => {
        /// # Safety
        /// `k` must point to a live kernel. Nothing here is ever executed.
        #[no_mangle]
        #[inline(never)]
        pub unsafe extern "C" fn $name(k: *mut K, $h: u32, $t: u64, $v: u32) {
            let $k: &mut K = &mut *k;
            let _ = $body;
        }
    };
}

// ------------------------------------------------------------------ tasks --
op!(kairos_task_create, |k| k.create_task("t", 1));
op!(kairos_task_delete, |k| k.task_delete(None));
op!(kairos_task_delay, |k, _h, t| k.delay(t));
op!(kairos_task_delay_until, |k| {
    let mut previous = 0u64;
    k.delay_until(&mut previous, 5)
});
op!(kairos_task_suspend, |k| k.suspend(None));
op!(kairos_task_resume, |k, h| k.resume(TaskHandle::from_raw(h)));
op!(kairos_task_priority_set, |k| k.set_priority(None, 2));
op!(kairos_task_priority_get, |k| k.task_priority_get(None));
op!(kairos_task_state_get, |k, h| k.task_state_get(TaskHandle::from_raw(h)));
op!(kairos_task_get_handle, |k| k.task_get_handle("t"));
op!(kairos_task_abort_delay, |k, h| k.abort_delay(TaskHandle::from_raw(h)));

// -------------------------------------------------------------- scheduler --
op!(kairos_start_scheduler, |k| k.start_scheduler());
op!(kairos_suspend_all, |k| k.suspend_all());
op!(kairos_resume_all, |k| k.resume_all());
op!(kairos_increment_tick, |k| k.increment_tick());
op!(kairos_switch_context, |k| k.switch_context());
op!(kairos_tick_count, |k| k.tick_count());
op!(kairos_check_terminated, |k| k.check_tasks_waiting_termination());

// ----------------------------------------------------- queues, semaphores --
op!(kairos_queue_create, |k| k.queue_create(4));
op!(kairos_queue_send, |k, h, t| k.queue_send(QueueHandle::from_raw(h), 1, t));
op!(kairos_queue_receive, |k, h, t| k.queue_receive(QueueHandle::from_raw(h), t));
op!(kairos_queue_peek, |k, h, t| k.queue_peek(QueueHandle::from_raw(h), t));
op!(kairos_queue_messages_waiting, |k, h| k
    .queue_messages_waiting(QueueHandle::from_raw(h)));
op!(kairos_queue_overwrite, |k, h| k.queue_overwrite(QueueHandle::from_raw(h), 2));
op!(kairos_semaphore_take, |k, h, t| k.semaphore_take(QueueHandle::from_raw(h), t));
op!(kairos_semaphore_give, |k, h| k.semaphore_give(QueueHandle::from_raw(h)));
op!(kairos_mutex_create, |k| k.mutex_create());
op!(kairos_mutex_create_recursive, |k| k.mutex_create_recursive());
op!(kairos_semaphore_create_counting, |k| k.semaphore_create_counting(4, 0));
op!(kairos_semaphore_create_binary, |k| k.semaphore_create_binary());
op!(kairos_mutex_take_recursive, |k, h, t| k
    .mutex_take_recursive(QueueHandle::from_raw(h), t));
op!(kairos_mutex_give_recursive, |k, h| k.mutex_give_recursive(QueueHandle::from_raw(h)));
op!(kairos_queue_send_from_isr, |k, h| k.queue_send_from_isr(QueueHandle::from_raw(h), 3));
op!(kairos_queue_receive_from_isr, |k, h| k
    .queue_receive_from_isr(QueueHandle::from_raw(h)));

// ---------------------------------------------------------- notifications --
op!(kairos_notify, |k, h, _t, v| k.notify(
    TaskHandle::from_raw(h),
    v as usize,
    v,
    // The action gates a five-arm match in both kernels. A constant here let
    // LLVM fold four of them away while `-u xTaskGenericNotify` pulls in all
    // five on the C side.
    match v & 3 {
        0 => NotifyAction::SetBits,
        1 => NotifyAction::Increment,
        2 => NotifyAction::Overwrite,
        _ => NotifyAction::NoOverwrite,
    }
));
op!(kairos_notify_wait, |k, _h, t, v| k.notify_wait(v as usize, v, v, t));
op!(kairos_notify_take, |k, _h, t, v| k.notify_take(v as usize, v & 1 != 0, t));
op!(kairos_notify_state_clear, |k, _h, _t, v| k.notify_state_clear(None, v as usize));
op!(kairos_notify_from_isr, |k| k.notify_from_isr(
    TaskHandle::default(),
    0,
    1,
    NotifyAction::SetBits
));

// ----------------------------------------------------------------- timers --
op!(kairos_timer_create, |k| k.timer_create("x", 10, true, 0, 0));
op!(kairos_timer_start, |k, h, t| k.timer_start(TimerHandle::from_raw(h), t));
op!(kairos_timer_stop, |k, h, t| k.timer_stop(TimerHandle::from_raw(h), t));
op!(kairos_timer_reset, |k, h, t| k.timer_reset(TimerHandle::from_raw(h), t));
op!(kairos_timer_change_period, |k, h, t| k
    .timer_change_period(TimerHandle::from_raw(h), 20, t));
op!(kairos_timer_is_active, |k, h| k.timer_is_active(TimerHandle::from_raw(h)));

// ----------------------------------------------------------- event groups --
op!(kairos_event_group_create, |k| k.event_group_create());
op!(kairos_event_group_set_bits, |k, h| k
    .event_group_set_bits(EventGroupHandle::from_raw(h), 1));
op!(kairos_event_group_wait_bits, |k, h, t, v| k.event_group_wait_bits(
    EventGroupHandle::from_raw(h),
    v,
    // Both bools gate branches the C arm compiles unconditionally.
    v & 1 != 0,
    v & 2 != 0,
    t
));
op!(kairos_event_group_clear_bits, |k, h| k
    .event_group_clear_bits(EventGroupHandle::from_raw(h), 1));
op!(kairos_event_group_sync, |k, h, t| k
    .event_group_sync(EventGroupHandle::from_raw(h), 1, 1, t));

// --------------------------------------------------------- stream buffers --
op!(kairos_stream_buffer_create, |k| k.stream_buffer_create(64, 1));
op!(kairos_stream_buffer_send, |k, h| {
    let data = [0u8; 4];
    k.stream_buffer_send(StreamBufferHandle::from_raw(h), &data, 0)
});
op!(kairos_stream_buffer_receive, |k, h| {
    let mut out = [0u8; 4];
    k.stream_buffer_receive(StreamBufferHandle::from_raw(h), &mut out, 0)
});
op!(kairos_stream_buffer_send_from_isr, |k, h| {
    let data = [0u8; 4];
    k.stream_buffer_send_from_isr(StreamBufferHandle::from_raw(h), &data)
});

/// A staticlib needs one even though nothing here is run.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

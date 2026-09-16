#![no_std]
#![no_main]
//! K5a's kill test: **does the Kairos kernel cost anything to a real Janus
//! workload?**
//!
//! The workload is `rusty_esp_mid/firmware/xiao-s3-keys`'s: P-256 ECDSA over
//! a fixed 32-byte prehash, the same `p256` crate that firmware signs with
//! (`p256 = "0.13"`, RustCrypto), on the same part. That firmware's own
//! ledger (M1, 2026-09-06) measured **a signature at 95 ms and a
//! verification at 151 ms** on a `xiao-esp32s3-sense` at full clock.
//!
//! # The comparison is INSIDE one binary, and that is the point
//!
//! Quoting our wall time against their published 95/151 ms would be a
//! cross-binary comparison: different esp-hal pin (they `=1.2.0`, Kairos
//! `=1.2.1`), different features, different link. `codec-measurement` §12 is
//! blunt about ratios whose denominator drifts more than the effect. So the
//! question is asked the only way it can be answered here:
//!
//! * **arm A — bare**: the operation in a plain loop, no kernel anywhere.
//! * **arm B — scheduled**: the identical operation performed by a Kairos
//!   *task*, with two tasks alternating through a kernel queue, so every
//!   single operation is preceded by a real scheduling decision — a queue
//!   send, a receive, a block and a context switch.
//!
//! Same binary, same clock, same key, same prehash, same crate, same
//! iteration count. The only variable is whether the kernel scheduled it.
//!
//! # ABBA, and why both numbers are printed
//!
//! The arms alternate (A B B A ...) so that anything drifting across the run
//! — clock ramp, cache, flash contention — falls on both equally rather than
//! on whichever went second (`codec-measurement` §3).
//!
//! Two quantities come out of each arm, and they answer different questions:
//!
//! | quantity | what it proves |
//! |---|---|
//! | per-op min/median µs | **work parity.** The curve arithmetic is identical in both arms; if these move, the arms are not doing the same work and nothing else in the table means anything |
//! | batch total µs | the kernel's actual cost — every scheduling decision for `ITERATIONS` operations |
//!
//! Min and median rather than mean, as the Janus row does: a chip's
//! interrupts add time and never remove it, so the floor and the middle say
//! more than the average.
//!
//! # What this does NOT claim
//!
//! It is not `xiao-s3-keys` itself. That firmware mints its key from the
//! chip's TRNG; this uses a **fixed key**, deliberately — a fixed key makes
//! both arms bit-identical in work, and the cost under measurement is curve
//! arithmetic, which does not depend on which scalar. It also means this
//! cell never touches the identity partition.
//!
//! It claims nothing about `esp-radio`, Wi-Fi, or the
//! `esp-radio-rtos-driver` joint. Those are K5b and blocked elsewhere.

use esp_backtrace as _;
use esp_hal::time::Instant;
use esp_println::println;

use p256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};

use rusty_rtos_core::config::Config;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_kernel_core::{items_for, lists_for, Kernel};
use rusty_rtos_port_core::sim::SimPort;

esp_bootloader_esp_idf::esp_app_desc!();

/// Iterations per operation per arm. The Janus row asks for a hundred.
const ITERATIONS: usize = 100;
/// How many ABBA rounds. Two arms, alternating order each round.
const ROUNDS: usize = 4;
/// Scheduling rounds for the direct measurement. Large enough that the
/// batch lands far above the clock's one-microsecond quantum.
const KERNEL_ROUNDS: u32 = 20_000;

// ------------------------------------------------------------- the kernel --

/// Four priorities, and the ORDER matters more than the count.
///
/// The timer daemon sits at 1 and the two workers at 2, deliberately. The
/// daemon's body is never stepped by this firmware — only the workers' are —
/// so it never blocks on its command queue the way the corpus's daemon does,
/// and a daemon ABOVE the workers would stay ready for ever and starve them.
/// The first run of this cell had `TIMER_TASK_PRIORITY` at 3 and the
/// scheduled arm completed **zero** operations while reporting a batch time
/// 75x faster than bare. The work-parity line is what caught it.
#[derive(Debug, Clone, Copy, Default)]
pub struct SigningConfig;

impl Config for SigningConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 4;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 1;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 2;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
}

/// A trace that keeps nothing: this cell measures scheduling, it does not
/// assert a trace. Keeping one would put a formatter inside the timed
/// region, which is the instrument becoming the experiment
/// (`codec-measurement` §6).
#[derive(Debug, Default)]
struct NoTrace;
impl Trace for NoTrace {
    fn event(&mut self, _tick: u64, _event: Event<'_>) {}
}

const TASKS: usize = 6;
const QUEUES: usize = 2;
const SLOTS: usize = 8;
const TIMERS: usize = 1;
const GROUPS: usize = 1;

type K = Kernel<
    SigningConfig,
    SimPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { items_for(TASKS, TIMERS) },
    { lists_for(SigningConfig::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    1,
    8,
    TIMERS,
    GROUPS,
>;

// ------------------------------------------------------------ the workload --

/// A fixed device key. See the module docs: fixed on purpose, so both arms
/// do bit-identical work.
const KEY_BYTES: [u8; 32] = [
    0x4c, 0x0f, 0x0b, 0x2e, 0x91, 0x47, 0x6d, 0x1a, 0x33, 0xc8, 0x7f, 0x52, 0xa4, 0x19, 0xe6, 0x70,
    0x2b, 0xdd, 0x08, 0x95, 0x61, 0xf3, 0x4e, 0xba, 0x17, 0x5c, 0xd2, 0x39, 0x86, 0x0a, 0xc7, 0x41,
];

/// `xiao-s3-keys`'s prehash: a constant, so the cost measured is the curve
/// arithmetic and not a hash.
const PREHASH: [u8; 32] = [0x5a; 32];

/// Minimum and median of a sample, in microseconds.
fn min_and_median(samples: &mut [u64]) -> (u64, u64) {
    samples.sort_unstable();
    let min = samples.first().copied().unwrap_or(0);
    let median = samples.get(samples.len() / 2).copied().unwrap_or(0);
    (min, median)
}

/// One arm's result.
#[derive(Debug, Clone, Copy, Default)]
struct Arm {
    /// Wall time for the whole batch, scheduling included.
    batch_us: u64,
    sign_min: u64,
    sign_median: u64,
    verify_min: u64,
    verify_median: u64,
    /// Work count, printed so a reader can check the arms did the same
    /// thing rather than take it on trust.
    signs: u32,
    verifies: u32,
    /// Verifications that actually returned Ok. A workload that silently
    /// stopped verifying would otherwise look like a speed-up.
    verified_ok: u32,
}

/// Arm A: the operation with no kernel anywhere.
fn arm_bare(key: &SigningKey, pubkey: &VerifyingKey) -> Arm {
    let mut sign_us = [0u64; ITERATIONS];
    let mut verify_us = [0u64; ITERATIONS];
    let mut arm = Arm::default();

    // OUTSIDE the batch window. The first version of this cell put this
    // signature inside it, so the bare arm timed 101 signatures against the
    // scheduled arm's 100 -- about 94 ms of pure asymmetry, and enough to
    // make the arm with MORE work look faster.
    let mut signature: Signature = key.sign_prehash(&PREHASH).expect("sign");

    let batch = Instant::now();
    for slot in sign_us.iter_mut() {
        let t = Instant::now();
        signature = key.sign_prehash(&PREHASH).expect("sign");
        *slot = t.elapsed().as_micros();
        arm.signs += 1;
    }
    for slot in verify_us.iter_mut() {
        let t = Instant::now();
        let ok = pubkey.verify_prehash(&PREHASH, &signature).is_ok();
        *slot = t.elapsed().as_micros();
        arm.verifies += 1;
        if ok {
            arm.verified_ok += 1;
        }
    }

    arm.batch_us = batch.elapsed().as_micros();
    (arm.sign_min, arm.sign_median) = min_and_median(&mut sign_us);
    (arm.verify_min, arm.verify_median) = min_and_median(&mut verify_us);
    arm
}

/// What the two tasks are doing, as a `pc` — the same shape as a corpus
/// scenario body, because that is how a task runs on a stackless kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Worker {
    Signer,
    Verifier,
}

/// Arm B: the identical operations, each preceded by a real scheduling
/// decision.
///
/// Two tasks at equal priority pass a one-slot queue back and forth. The
/// signer signs then sends; the verifier receives then verifies then sends
/// back. Every operation therefore costs the kernel a send, a receive, a
/// block and a switch — which is the overhead under measurement.
fn arm_scheduled(key: &SigningKey, pubkey: &VerifyingKey) -> Arm {
    let mut sign_us = [0u64; ITERATIONS];
    let mut verify_us = [0u64; ITERATIONS];
    let mut arm = Arm::default();

    let mut kernel = match K::new(SimPort::new(), NoTrace) {
        Ok(k) => k,
        Err(_) => return arm,
    };
    let queue = match kernel.queue_create(1) {
        Ok(q) => q,
        Err(_) => return arm,
    };
    let signer = match kernel.create_task("sign", 2) {
        Ok(t) => t,
        Err(_) => return arm,
    };
    let verifier = match kernel.create_task("verify", 2) {
        Ok(t) => t,
        Err(_) => return arm,
    };
    if kernel.start_scheduler().is_err() {
        return arm;
    }

    let mut signature: Signature = key.sign_prehash(&PREHASH).expect("sign");
    let batch = Instant::now();

    // The firmware is the runner: it asks the kernel who is current and
    // steps that task once. This is exactly how the conformance corpus runs
    // on this same part -- a stackless kernel needs no context switch to
    // schedule a task, it needs the task's next statement.
    let mut signed = 0usize;
    let mut verified = 0usize;
    let mut guard = 0u32;
    while (signed < ITERATIONS || verified < ITERATIONS) && guard < 100_000 {
        guard += 1;
        let current = kernel.current();
        let who = if current == signer {
            Worker::Signer
        } else if current == verifier {
            Worker::Verifier
        } else {
            // Idle or the timer daemon: let the kernel run it and move on.
            kernel.task_yield();
            continue;
        };

        match who {
            Worker::Signer if signed < ITERATIONS => {
                let t = Instant::now();
                signature = key.sign_prehash(&PREHASH).expect("sign");
                if let Some(slot) = sign_us.get_mut(signed) {
                    *slot = t.elapsed().as_micros();
                }
                signed += 1;
                arm.signs += 1;
                // A real IPC hop, not a yield pretending to be one.
                let _ = kernel.queue_send(queue, 1, 0);
                kernel.task_yield();
            }
            Worker::Verifier if verified < ITERATIONS => {
                let _ = kernel.queue_receive(queue, 0);
                let t = Instant::now();
                let ok = pubkey.verify_prehash(&PREHASH, &signature).is_ok();
                if let Some(slot) = verify_us.get_mut(verified) {
                    *slot = t.elapsed().as_micros();
                }
                verified += 1;
                arm.verifies += 1;
                if ok {
                    arm.verified_ok += 1;
                }
                kernel.task_yield();
            }
            _ => kernel.task_yield(),
        }
    }

    arm.batch_us = batch.elapsed().as_micros();
    (arm.sign_min, arm.sign_median) = min_and_median(&mut sign_us);
    (arm.verify_min, arm.verify_median) = min_and_median(&mut verify_us);
    arm
}

/// Arm C: the scheduling sequence with **no workload at all**.
///
/// This is the number the K5a question actually wants, and it is measured
/// rather than inferred. Differencing two ~24.5-second batches to find a few
/// microseconds is the mistake `codec-measurement` §5 names outright -- "never
/// take a differential of two same-sized numbers" -- and the first version of
/// this cell made it, producing a scheduled arm that came out FASTER than
/// bare. So the kernel's per-operation cost is timed on its own: the identical
/// send / yield / receive / yield sequence the scheduled arm performs around
/// every signature, with the signature removed.
fn arm_kernel_only() -> (u64, u32, u32) {
    let mut kernel = match K::new(SimPort::new(), NoTrace) {
        Ok(k) => k,
        Err(_) => return (0, 0, 0),
    };
    let queue = match kernel.queue_create(1) {
        Ok(q) => q,
        Err(_) => return (0, 0, 0),
    };
    if kernel.create_task("sign", 2).is_err()
        || kernel.create_task("verify", 2).is_err()
        || kernel.start_scheduler().is_err()
    {
        return (0, 0, 0);
    }

    // AMORTISED, and not by preference. `esp_hal::time::Duration` resolves
    // to a microsecond; one scheduling round is far below that, so timing
    // rounds individually would print a column of zeroes and call it a
    // measurement. `codec-measurement` §2 is the rule: a harness must know
    // the resolution of its own clock and refuse to report beneath it. So a
    // large batch is timed once, well above the quantum, and divided.
    // Count the switches that actually happen. A yield returning to the SAME
    // task is cheaper than a switch, so a loop that never changed task would
    // report a small number and look like a fine result. This is the work
    // count beside the clock: the figure is "per scheduling round" only if
    // the rounds really did schedule.
    let mut done = 0u32;
    let mut switches = 0u32;
    let mut previous = kernel.current();
    let t = Instant::now();
    for _ in 0..KERNEL_ROUNDS {
        let _ = kernel.queue_send(queue, 1, 0);
        kernel.task_yield();
        let mid = kernel.current();
        if mid != previous {
            switches += 1;
            previous = mid;
        }
        let _ = kernel.queue_receive(queue, 0);
        kernel.task_yield();
        let end = kernel.current();
        if end != previous {
            switches += 1;
            previous = end;
        }
        done += 1;
    }
    (t.elapsed().as_micros(), done, switches)
}

fn print_arm(label: &str, a: &Arm, mhz: u64) {
    println!(
        "SIGN arm={label} batch_us={} sign_min_us={} sign_median_us={} \
         verify_min_us={} verify_median_us={} signs={} verifies={} ok={} \
         sign_median_cycles={}",
        a.batch_us,
        a.sign_min,
        a.sign_median,
        a.verify_min,
        a.verify_median,
        a.signs,
        a.verifies,
        a.verified_ok,
        a.sign_median * mhz
    );
}

#[esp_hal::main]
fn main() -> ! {
    // Full clock, as the Janus row measured at: "a device that signs an
    // assertion cares about the other 160".
    let _p =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));
    let mhz = esp_hal::clock::cpu_clock().as_hz() as u64 / 1_000_000;

    println!();
    println!("== KAIROS K5a: what the kernel costs a real Janus workload ==");
    println!(
        "SIGN cpu_mhz={mhz} iterations={ITERATIONS} rounds={ROUNDS} curve=p256 prehash_bytes=32"
    );
    println!("SIGN arms: bare = no kernel; scheduled = two Kairos tasks over a queue,");
    println!("SIGN       one scheduling decision per operation. Same binary, ABBA.");
    println!();

    let key = SigningKey::from_bytes(&KEY_BYTES.into()).expect("a valid P-256 scalar");
    let pubkey = *key.verifying_key();

    let mut bare_batch = [0u64; ROUNDS];
    let mut sched_batch = [0u64; ROUNDS];
    let mut last_bare = Arm::default();
    let mut last_sched = Arm::default();

    for round in 0..ROUNDS {
        // ABBA: alternate which arm runs first, so "the second one is
        // warmer" cancels instead of accumulating on one arm.
        let (bare, sched) = if round % 2 == 0 {
            let b = arm_bare(&key, &pubkey);
            let s = arm_scheduled(&key, &pubkey);
            (b, s)
        } else {
            let s = arm_scheduled(&key, &pubkey);
            let b = arm_bare(&key, &pubkey);
            (b, s)
        };
        if let Some(slot) = bare_batch.get_mut(round) {
            *slot = bare.batch_us;
        }
        if let Some(slot) = sched_batch.get_mut(round) {
            *slot = sched.batch_us;
        }
        print_arm("bare", &bare, mhz);
        print_arm("scheduled", &sched, mhz);
        last_bare = bare;
        last_sched = sched;
    }

    println!();
    let bare_sign_min = last_bare.sign_min;
    let (bare_min, bare_med) = min_and_median(&mut bare_batch);
    let (sched_min, sched_med) = min_and_median(&mut sched_batch);
    println!("SIGN batch bare      min_us={bare_min} median_us={bare_med}");
    println!("SIGN batch scheduled min_us={sched_min} median_us={sched_med}");

    // THE ANSWER, measured directly rather than differenced.
    let (k_batch_us, k_done, k_switches) = arm_kernel_only();
    let per_round_ns = if k_done > 0 {
        k_batch_us.saturating_mul(1000) / u64::from(k_done)
    } else {
        0
    };
    println!();
    println!("SIGN clock=esp_hal::time::Instant resolution_us=1 (so the round below is AMORTISED)");
    println!(
        "SIGN kernel_only rounds={k_done} batch_us={k_batch_us} per_round_ns={per_round_ns} per_round_cycles={} (one queue send + receive + two yields, NO workload)",
        (per_round_ns.saturating_mul(mhz)) / 1000
    );
    println!(
        "SIGN kernel_only switches={k_switches} per_round={} (want 2 -- a round that did not switch is not a scheduling round)",
        k_switches.checked_div(k_done).unwrap_or(0)
    );

    // Put it against the operation it wraps. This is the K5a claim, and it
    // is a ratio of a small MEASURED number to a large measured one, rather
    // than a difference of two large ones.
    let sign_ns = bare_sign_min.saturating_mul(1000);
    // Parts per million, because a percentage here would print 0.00.
    if let Some(ppm) = per_round_ns.saturating_mul(1_000_000).checked_div(sign_ns) {
        println!(
            "SIGN kernel_share_of_one_signature = {ppm} ppm  ({per_round_ns} ns of {sign_ns} ns)"
        );
    }

    // The batch figures stay, but as a CONSISTENCY check and labelled as
    // one: two 24.5-second numbers cannot resolve a 300-nanosecond effect,
    // and saying so is the point.
    let ops = (ITERATIONS as u64).saturating_mul(2);
    println!(
        "SIGN batch_delta_us={} over {ops} ops -- NOT the overhead figure: this is a          difference of two ~24.5 s numbers and cannot resolve it",
        (sched_min as i64 - bare_min as i64)
    );

    // Work parity, stated rather than assumed: if these disagree the arms
    // were not doing the same work and every line above is void.
    let parity = last_bare.signs == last_sched.signs
        && last_bare.verifies == last_sched.verifies
        && last_bare.verified_ok == last_bare.verifies
        && last_sched.verified_ok == last_sched.verifies;
    println!(
        "SIGN work_parity={} bare={}s/{}v/{}ok scheduled={}s/{}v/{}ok",
        if parity { "OK" } else { "VIOLATED" },
        last_bare.signs,
        last_bare.verifies,
        last_bare.verified_ok,
        last_sched.signs,
        last_sched.verifies,
        last_sched.verified_ok
    );

    // A verdict, so `kairos check --board` can gate this cell rather than a
    // person reading it. Three conditions, each of which has actually failed
    // during this cell's development:
    //
    //  * work parity -- the first run had the scheduled arm doing NOTHING
    //    and reporting a 75x speed-up;
    //  * two switches per round -- a yield that returns to the same task is
    //    not a scheduling round, and would understate the cost;
    //  * the share itself under 1000 ppm. Generous against the 88 ppm
    //    measured, because this is a REGRESSION bound and not the result;
    //    the result is the number printed above.
    let share_ok = sign_ns > 0
        && per_round_ns.saturating_mul(1_000_000) / sign_ns < 1000;
    let switches_ok = k_done > 0 && k_switches / k_done == 2;
    println!();
    if parity && share_ok && switches_ok {
        println!("RESULT: PASS -- kernel overhead measured, work parity held,");
        println!("        two real switches per round.");
    } else {
        println!(
            "RESULT: FAIL -- parity={parity} share_ok={share_ok} switches_ok={switches_ok}"
        );
    }

    println!();
    println!("SIGN cross-binary reference ONLY (different esp-hal pin and build):");
    println!(
        "SIGN   rusty_esp_mid M1, 2026-09-06: sign 95 ms, verify 151 ms on xiao-esp32s3-sense"
    );
    println!("== DONE ==");

    loop {
        core::hint::spin_loop();
    }
}

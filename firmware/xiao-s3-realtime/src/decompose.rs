//! `--features decompose`: the interrupt -> task latency, cut into segments.
//!
//! The headline row measures one number, ~1,440 cycles for a notify. This
//! stamps `ccount` at ten points along the same path and keeps a small
//! histogram per segment, per mechanism:
//!
//! ```text
//!  A  alarm-1 handler, first statement           (the headline's start)
//!  B  alarm cleared
//!  C  irq_fire: about to call the kernel
//!  D  irq_fire: the kernel's _from_isr returned
//!  E  Software0 raised; the alarm handler is about to return
//!  F  Software0 handler, first statement
//!  G  kernel switch_context returned
//!  H  idle accounting done
//!  I  port context copy done; Software0 is about to return
//!  J  the woken task: first statement after the blocking call came back
//!  K  the woken task: the call made again has returned the value (the end)
//! ```
//!
//! What no stamp can see: the trap ENTRY before A. So, before the scheduler
//! starts, a bare peripheral interrupt (`FROM_CPU_INTR1`, dispatched by the
//! same esp-hal path as the alarm) is raised 1,000 times, and its entry
//! (raise -> handler's first statement) and exit (handler's last statement
//! -> the raiser continues) are measured on their own.
//!
//! Every stamp is a `ccount` read and a store: a few cycles each, about
//! thirty in all. The total row here is therefore slightly above the plain
//! build's, and the report prints both so that tax is visible.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

use esp_hal::interrupt::software::SoftwareInterrupt;
use esp_println::println;
use xtensa_lx::timer::get_cycle_count;

use super::Mark;

/// The stamps of the event in flight. One writer per point: B..E in the
/// alarm handler, F..I in `Software0`.
static MARKS: [AtomicU32; 9] = [const { AtomicU32::new(0) }; 9];

pub fn mark(m: Mark) {
    if let Some(slot) = MARKS.get(m as usize) {
        slot.store(get_cycle_count(), Ordering::Relaxed);
    }
}

const SEGMENTS: [&str; 10] = [
    "A>B  clear the alarm",
    "B>C  irq_fire bookkeeping",
    "C>D  kernel _from_isr call",
    "D>E  raise Software0, unwind",
    "E>F  trap exit + trap entry",
    "F>G  kernel switch_context",
    "G>H  idle accounting",
    "H>I  port context copy",
    "I>J  trap exit + task resume",
    "J>K  the call again: Ready",
];

/// A small histogram in cycles: `BIN`-cycle bins to `256 * BIN`, the max kept
/// exactly. Segments use 4 (to 1,024); the total uses 32 (to 8,192). The first
/// board run used 4 for the total too: every total overflowed, and its p50
/// printed as its max.
const NB: usize = 256;

struct Seg<const BIN: u32> {
    bins: [AtomicU32; NB],
    n: AtomicU32,
    min: AtomicU32,
    max: AtomicU32,
}

impl<const BIN: u32> Seg<BIN> {
    const fn new() -> Self {
        Self {
            bins: [const { AtomicU32::new(0) }; NB],
            n: AtomicU32::new(0),
            min: AtomicU32::new(u32::MAX),
            max: AtomicU32::new(0),
        }
    }

    fn record(&self, c: u32) {
        let i = ((c / BIN) as usize).min(NB - 1);
        if let Some(b) = self.bins.get(i) {
            b.fetch_add(1, Ordering::Relaxed);
        }
        self.n.fetch_add(1, Ordering::Relaxed);
        self.min.fetch_min(c, Ordering::Relaxed);
        self.max.fetch_max(c, Ordering::Relaxed);
    }

    fn p(&self, per_10k: u32) -> u32 {
        let n = self.n.load(Ordering::Relaxed);
        let (lo, hi) = (self.min.load(Ordering::Relaxed), self.max.load(Ordering::Relaxed));
        if n == 0 {
            return 0;
        }
        let target = (u64::from(n) * u64::from(per_10k)).div_ceil(10_000).max(1);
        let mut seen = 0u64;
        for (i, b) in self.bins.iter().enumerate() {
            seen += u64::from(b.load(Ordering::Relaxed));
            if seen >= target {
                return if i == NB - 1 { hi } else { ((i as u32 + 1) * BIN).clamp(lo, hi) };
            }
        }
        hi
    }
}

/// Per mechanism (notify, queue, semaphore): ten segments, and the total.
static SEG: [[Seg<4>; 10]; 3] = [const { [const { Seg::new() }; 10] }; 3];
static TOTAL: [Seg<32>; 3] = [const { Seg::new() }; 3];
static INCOHERENT: AtomicU32 = AtomicU32::new(0);

/// Called by the woken task with the event's first stamp (A), its resume
/// stamp (J) and its final stamp (K). Rejects an event whose ISR stamps do
/// not belong to it -- a segment over 20,000 cycles means a stamp is stale.
pub fn done(slot: usize, a: u32, j: u32, k: u32) {
    let m: [u32; 9] = core::array::from_fn(|i| MARKS.get(i).map_or(0, |s| s.load(Ordering::Relaxed)));
    // A, B..I, J, K in path order.
    let path = [a, m[1], m[2], m[3], m[4], m[5], m[6], m[7], m[8], j, k];
    let segs: [u32; 10] = core::array::from_fn(|i| path[i + 1].wrapping_sub(path[i]));
    if segs.iter().any(|&s| s > 20_000) {
        INCOHERENT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some(row) = SEG.get(slot) else { return };
    for (s, v) in row.iter().zip(segs) {
        s.record(v);
    }
    if let Some(total) = TOTAL.get(slot) {
        total.record(k.wrapping_sub(a));
    }
}

pub fn report() {
    println!();
    println!("=== interrupt -> task, decomposed (cycles @ 240 MHz; p50 [min .. p99] max) ===");
    let names = ["notify", "queue", "semaphore"];
    for ((name, row), t) in names.iter().zip(SEG.iter()).zip(TOTAL.iter()) {
        println!("{name}:");
        let mut sum = 0u32;
        for (label, s) in SEGMENTS.iter().zip(row.iter()) {
            let p50 = s.p(5_000);
            sum += p50;
            println!(
                "  {label:<30} {p50:>5}  [{:>5} .. {:>5}] {:>6}",
                s.min.load(Ordering::Relaxed),
                s.p(9_900),
                s.max.load(Ordering::Relaxed)
            );
        }
        println!(
            "  {:<30} {:>5}  [{:>5} .. {:>5}] {:>6}   n={}, sum of segment p50s {sum}",
            "A>K  total (32-cycle bins)",
            t.p(5_000),
            t.min.load(Ordering::Relaxed),
            t.p(9_900),
            t.max.load(Ordering::Relaxed),
            t.n.load(Ordering::Relaxed)
        );
    }
    println!("events rejected as incoherent: {}", INCOHERENT.load(Ordering::Relaxed));
    let (e, x, tax) = (ENTRY.load(Ordering::Relaxed), EXIT.load(Ordering::Relaxed), TAX.load(Ordering::Relaxed));
    println!("one bare esp-hal peripheral interrupt (FROM_CPU_INTR1, median of {PROBES}):");
    println!("  entry: raise -> handler's first statement   {e:>5}");
    println!("  exit:  handler's last statement -> raiser   {x:>5}");
    println!("  bracket tax (an empty ccount pair)          {tax:>5}   (not subtracted above)");
}

// ------------------------------------------------- the entry/exit probe --

const PROBES: usize = 1_000;

struct SwiCell(UnsafeCell<Option<SoftwareInterrupt<'static, 1>>>);
// SAFETY: written once by `probe_entry_exit` before the interrupt is raised;
// afterwards only the handler reads it, and the raiser waits for it.
#[allow(unsafe_code)]
unsafe impl Sync for SwiCell {}
static SWI: SwiCell = SwiCell(UnsafeCell::new(None));

static P_IN: AtomicU32 = AtomicU32::new(0);
static P_OUT: AtomicU32 = AtomicU32::new(0);
static P_SEQ: AtomicU32 = AtomicU32::new(0);
static ENTRY: AtomicU32 = AtomicU32::new(0);
static EXIT: AtomicU32 = AtomicU32::new(0);
static TAX: AtomicU32 = AtomicU32::new(0);

#[esp_hal::handler]
fn probe_handler() {
    let t = get_cycle_count();
    P_IN.store(t, Ordering::Relaxed);
    // SAFETY: see `SwiCell`.
    #[allow(unsafe_code)]
    let swi = unsafe { &*SWI.0.get() };
    if let Some(swi) = swi.as_ref() {
        swi.reset();
    }
    P_SEQ.fetch_add(1, Ordering::Relaxed);
    P_OUT.store(get_cycle_count(), Ordering::Relaxed);
}

/// Measure one bare interrupt's entry and exit, before anything else runs.
pub fn probe_entry_exit(mut swi: SoftwareInterrupt<'static, 1>) {
    swi.set_interrupt_handler(probe_handler);
    // SAFETY: the interrupt has not been raised yet, so no handler reads this.
    #[allow(unsafe_code)]
    unsafe {
        *SWI.0.get() = Some(swi);
    }
    // SAFETY: as above; this is the only reference held while raising, and
    // the handler takes only a shared one.
    #[allow(unsafe_code)]
    let swi = unsafe { &*SWI.0.get() };
    let Some(swi) = swi.as_ref() else { return };

    let mut entry = [0u32; PROBES];
    let mut exit = [0u32; PROBES];
    let mut tax = [0u32; PROBES];
    for i in 0..PROBES {
        let t0 = get_cycle_count();
        let t1 = get_cycle_count();
        tax[i] = t1.wrapping_sub(t0);

        let seq = P_SEQ.load(Ordering::Relaxed);
        let t0 = get_cycle_count();
        swi.raise();
        while P_SEQ.load(Ordering::Relaxed) == seq {}
        let t1 = get_cycle_count();
        entry[i] = P_IN.load(Ordering::Relaxed).wrapping_sub(t0);
        exit[i] = t1.wrapping_sub(P_OUT.load(Ordering::Relaxed));
    }
    for a in [&mut entry, &mut exit, &mut tax] {
        a.sort_unstable();
    }
    ENTRY.store(entry[PROBES / 2], Ordering::Relaxed);
    EXIT.store(exit[PROBES / 2], Ordering::Relaxed);
    TAX.store(tax[PROBES / 2], Ordering::Relaxed);
}

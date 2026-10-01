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

mod hammer;

use hammer::{Counting, K, TestPort, Worked, Xorshift, hammer};
use rusty_rtos_core::handle::StreamBufferHandle;

#[test]
fn no_call_panics_however_it_is_called() {
    // Sixty-four independent kernels, each taking four thousand arbitrary
    // calls: a quarter of a million calls over the whole public surface.
    // A failure names the seed, and a single seed reproduces it exactly.
    let mut total = Worked::default();
    for seed in 1..=64_u64 {
        let seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let worked = hammer(Xorshift(seed | 1), 4_000);
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

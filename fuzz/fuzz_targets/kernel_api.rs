//! Every public kernel call, with arguments chosen by libFuzzer.
//!
//! The dispatcher is `tests/hammer/mod.rs`, the one `tests/no_panic.rs`
//! drives with a seeded xorshift -- so this target and that test cover the
//! same surface, and a crash found here reproduces as an ordinary test by
//! feeding the same bytes. The property is the test's: whatever the
//! arguments -- forged, stale or foreign handles, extreme tick counts,
//! lengths past the arenas -- every call comes back, and never panics.

#![no_main]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/rusty_rtos_kernel-core/tests/hammer/mod.rs"]
mod hammer;

/// libFuzzer's bytes, eight at a time; zeros once they run out.
struct Bytes<'a>(&'a [u8]);

impl hammer::Entropy for Bytes<'_> {
    fn next_u64(&mut self) -> u64 {
        let take = self.0.len().min(8);
        let (head, rest) = self.0.split_at(take);
        let mut word = [0_u8; 8];
        word[..take].copy_from_slice(head);
        self.0 = rest;
        u64::from_le_bytes(word)
    }
}

fuzz_target!(|data: &[u8]| {
    // About a dozen draws per call; enough calls to spend the input, and a
    // cap so one input cannot run for ever.
    let calls = (data.len() / 96).clamp(1, 2_000) as u32;
    let _ = hammer::hammer(Bytes(data), calls);
});

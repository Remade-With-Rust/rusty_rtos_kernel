//! A task's name, stored the way the C kernel stores it.
//!
//! `prvInitialiseNewTask` copies at most `configMAX_TASK_NAME_LEN`
//! characters into the TCB and writes a NUL at the last index, so a longer
//! name is silently truncated and the trace prints the truncated form. A
//! `&'static str` would have been simpler and wrong: the oracle's line for a
//! long name is the truncated one, and the diff is the gate.

use core::fmt;
use core::str;

/// The largest `configMAX_TASK_NAME_LEN` this build supports.
///
/// The C default is 16 and the `Posix_GCC` demo uses 12; a configuration
/// asking for more than this is refused by [`Name::new`] rather than
/// truncated silently at a limit nobody wrote down.
pub const NAME_CAPACITY: usize = 16;

// `as_str` validates a FIXED 16-byte window because that is the smallest one
// `run_utf8_validation` will take its word-at-a-time ASCII path for. While the
// capacity equals that window, `end <= WINDOW` is provably true and the
// widening branch folds away — which is why `as_str` needs no `if` at run
// time and why the test below has no wide-window case.
//
// Raise the capacity above 16 and that stops being true: the widening path
// goes live, untested, on the hot path that prints two names per context
// switch. This makes that a build failure instead.
const _: () = assert!(
    NAME_CAPACITY <= 16,
    "NAME_CAPACITY above 16 makes as_str's widening branch live again: give      it a test case, and re-measure the trace rows, before raising this."
);

/// A task name: at most [`NAME_CAPACITY`] bytes, truncated to the
/// configuration's `MAX_TASK_NAME_LEN` exactly as C truncates it.
///
/// # Why this is aligned to eight ON A 64-BIT HOST ONLY
///
/// `as_str` validates a sixteen-byte window so that `run_utf8_validation`
/// takes its word-at-a-time ASCII path instead of walking the name a byte at
/// a time. That path has TWO conditions and the window size is only one of
/// them: `core`'s loop reads two `usize`s at a time and declines unless the
/// slice is `usize`-ALIGNED (`align_offset(size_of::<usize>()) == 0`).
///
/// Nothing said so, and on the host the alignment was absent. `Tcb` holds a
/// `Name` as its first field and is four-aligned deliberately -- see
/// `WaitFrame`, whose `Split64` fields exist to keep it that way -- and
/// `Slot<Tcb>` puts a `u32` generation in front of it, so `bytes` landed at
/// offset four of every slot. Measured with callgrind on a sixteen-byte
/// window holding `CNT_INC`:
///
/// | window offset | Ir per `from_utf8` |
/// |---|---:|
/// | 0 (`usize`-aligned) | **46** |
/// | 4 (what `Slot<Tcb>` gave it) | **143** |
/// | 1 | 161 |
///
/// `bench/kernel-ir` measured 142.7 in production -- offset four to within a
/// third of an instruction. The optimisation was documented, tested, and off.
///
/// # ★ And it was off only on the HOST, which is why it survived
///
/// `usize` is FOUR bytes on every target this kernel ships to, so offset four
/// is already `usize`-aligned there and the ASCII path was being taken all
/// along. The defect existed only where `usize` is eight -- the 64-bit host
/// that runs the sim and the conformance suite. That is the pointer-width
/// asymmetry this project has paid for before (`docs/LEDGER.md`), and it is
/// why the alignment is requested with a `cfg_attr` rather than outright:
/// asking for eight unconditionally would grow `Tcb` on a 32-bit target, and
/// spend real firmware RAM to fix a host-only cost.
///
/// So this buys the sim and `kairos conform --all` about ten percent of their
/// instructions and buys firmware nothing, by design. It changes no layout a
/// target sees.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(target_pointer_width = "64", repr(align(8)))]
pub struct Name {
    bytes: [u8; NAME_CAPACITY],
    len: u8,
}

// `as_str`'s fast path depends on the window being `usize`-aligned, and a
// layout change removes that silently -- which is exactly what happened here.
// Fail the build instead.
//
// ONLY on the 64-bit host, because that is the only place the attribute above
// promises anything. `Name`'s own alignment is ONE -- every field is a `u8` --
// so on a 32-bit target the alignment does not come from this type at all: it
// comes from the container. `Tcb` holds a `Name` at offset zero and is itself
// four-aligned (`WaitFrame`'s `Split64` fields are what keep it so), and
// `Slot<Tcb>` puts a `u32` in front, so the window lands on a multiple of four
// -- which IS `usize`-aligned where `usize` is four bytes. That is why the
// defect never existed on a target, and why asking for four here would only
// grow `Tcb` for nothing.
//
// This assertion was written the other way round first, unconditionally, and
// it failed the rv32 build immediately: `align_of::<Name>()` is 1, not 4. The
// failure is the reason the paragraph above exists.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    core::mem::align_of::<Name>() >= core::mem::size_of::<usize>(),
    "Name must be usize-aligned on the host or as_str's word-at-a-time UTF-8      path stops being taken and every name a trace prints costs three times      what it should."
);

impl Default for Name {
    fn default() -> Self {
        Self {
            bytes: [0; NAME_CAPACITY],
            len: 0,
        }
    }
}

impl Name {
    /// Copy `name`, truncated the way `prvInitialiseNewTask` truncates:
    /// at most `max_len - 1` bytes, because the C array's last slot is the
    /// NUL. `max_len` of 0 yields an empty name, as C's loop would.
    ///
    /// Truncation happens on a character boundary, so the result is always
    /// valid UTF-8 — C truncates on a byte, but every name in the corpus is
    /// ASCII and a name that is not is a configuration error, not a silent
    /// mojibake.
    #[must_use]
    // In line at both callers (`rusty-compiler-leverage` A3): win 24, -20 B of
    // flash. `#[inline]` alone was a hint LLVM declined at this size.
    #[inline(always)]
    pub fn new(name: &str, max_len: usize) -> Self {
        let limit = max_len.saturating_sub(1).min(NAME_CAPACITY).min(name.len());
        // Walk back to a character boundary; for ASCII this is `limit`.
        let mut end = limit;
        while end > 0 && !name.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let src = name.as_bytes().get(..end).unwrap_or(&[]);
        let mut bytes = [0u8; NAME_CAPACITY];
        // A zip, NOT `copy_from_slice`. The slice version lowers to `memcpy`
        // and reads like the better choice -- but its length-mismatch arm
        // panics with TWO FORMATTED INTEGERS, and that drags
        // `core::fmt::Formatter::pad_integral` (596 B), `Display for usize`
        // (362 B) and `str::count::do_count_chars` (376 B) into a `no_std`
        // kernel that formats nothing anywhere else. Measured at **1,052
        // bytes** of flash for a branch that cannot be taken: `dst` is
        // `bytes[..src.len()]`, so the lengths are equal by construction and
        // LLVM still would not prove it.
        //
        // The zip has no panicking arm at all, so none of that machinery is
        // reachable. It copies at most NAME_CAPACITY (16) bytes.
        for (d, s) in bytes.iter_mut().zip(src.iter()) {
            *d = *s;
        }
        Self {
            bytes,
            len: u8::try_from(end).unwrap_or(0),
        }
    }

    /// The name as it will be printed.
    ///
    /// In line on purpose, and with the clamp below: a trace prints two
    /// names on every context switch, and out of line each one paid a call
    /// and a frame to hand back a slice of a field the caller already had.
    #[must_use]
    #[inline(always)]
    pub fn as_str(&self) -> &str {
        // `Name::new` is the only constructor and it writes a `len` no
        // larger than the buffer, so the clamp changes no value. It says so
        // to the compiler, which otherwise carries a bounds check and an
        // `Option` into every read of a name -- and a trace prints two of
        // them on every context switch.
        let end = usize::from(self.len).min(NAME_CAPACITY);
        // Validate a WINDOW, not the used prefix.
        //
        // `run_utf8_validation` only reaches its word-at-a-time ASCII path
        // when two `usize`s remain, so an eleven-byte task name -- which is
        // every name in this corpus -- is checked a byte at a time. The
        // profile put `from_utf8` at 9,776,286 instructions over 113,645
        // calls, 86 each, and that is where they went.
        //
        // Sixteen bytes is the smallest window that reaches the fast path.
        // The answer is unchanged because the tail is NULs: `Name::new`
        // starts from a zeroed array and writes only a prefix, `Default` is
        // all zeros, `bytes` is private and nothing else writes it. A NUL
        // is valid UTF-8, so widening the check cannot change its verdict.
        const WINDOW: usize = 16;
        let window = if end <= WINDOW { WINDOW } else { NAME_CAPACITY };
        match str::from_utf8(self.bytes.get(..window).unwrap_or(&[])) {
            // Every byte is ASCII or NUL, so `end` is a character boundary
            // and this is a length test rather than a scan.
            Ok(all) => all.get(..end).unwrap_or(""),
            Err(_) => "",
        }
    }

    /// Whether this name is `other`.
    ///
    /// Compares BYTES. Two `str`s are equal exactly when their bytes are, so
    /// this answers what `self.as_str() == other` answered — without the
    /// UTF-8 validation that makes a `&str`, which is the only thing that
    /// pulled `core::str::from_utf8` into a kernel built with `NoTrace`.
    /// Measured on `bench/kernel-flash`: **534 bytes**, 3.2 % of the arm, for
    /// one comparison in `task_get_handle`.
    #[must_use]
    // A3: ONE caller in the linked kernel, so this pays a prologue and epilogue
    // for a single call. Inlining moves the body rather than duplicating it.
    #[inline(always)]
    pub fn matches(&self, other: &str) -> bool {
        let end = usize::from(self.len).min(NAME_CAPACITY);
        self.bytes.get(..end) == Some(other.as_bytes())
    }

    /// How many bytes the name occupies.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the name is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_survive_whole() {
        assert_eq!(Name::new("CNT_INC", 12).as_str(), "CNT_INC");
        assert_eq!(Name::new("Tmr Svc", 12).as_str(), "Tmr Svc");
        assert_eq!(Name::new("IDLE", 12).as_str(), "IDLE");
    }

    #[test]
    fn long_names_truncate_like_the_c_kernel() {
        // configMAX_TASK_NAME_LEN counts the NUL, so 12 keeps 11 characters.
        assert_eq!(Name::new("ABCDEFGHIJKLMNOP", 12).as_str(), "ABCDEFGHIJK");
        assert_eq!(Name::new("ABCDEFGHIJKLMNOP", 12).len(), 11);
    }

    #[test]
    fn degenerate_limits_are_errors_not_panics() {
        assert!(Name::new("anything", 0).is_empty());
        assert!(Name::new("anything", 1).is_empty());
        assert_eq!(Name::new("anything", 2).as_str(), "a");
        assert_eq!(Name::new("", 12).as_str(), "");
    }

    /// `as_str` validates a sixteen-byte window and slices the answer out
    /// of it, which is only right while the bytes past `len` stay valid
    /// UTF-8. They are NULs today because `new` starts from a zeroed array
    /// and writes only a prefix. Pin that: a `new` that left the tail dirty
    /// would make `as_str` answer `""` for every name, and the conformance
    /// gate would report a diff in twenty-two scenarios at once with no
    /// clue which line caused it.
    #[test]
    fn the_tail_stays_valid_so_as_str_can_check_a_whole_window() {
        for (text, limit) in [("StrTrig", 12), ("", 12), ("a", 2), ("IDLE", 12)] {
            let n = Name::new(text, limit);
            assert!(
                str::from_utf8(n.bytes.get(..16).unwrap_or(&[])).is_ok(),
                "the sixteen-byte window must be valid UTF-8, not just the prefix"
            );
            assert!(
                n.bytes
                    .get(n.len()..)
                    .unwrap_or(&[])
                    .iter()
                    .all(|b| *b == 0),
                "the tail past `len` must be NUL"
            );
        }
        // A name that FILLS the capacity still reads back whole. This used
        // to assert `len() > 16` to prove it exercised `as_str`'s WIDE
        // window -- and when `NAME_CAPACITY` came down to 16 that assertion
        // failed, correctly: at capacity 16 the wide window cannot be
        // reached, because `end` is clamped to the capacity and the window
        // IS the capacity. The branch folds at compile time and the case it
        // guarded no longer exists.
        //
        // The guard below is what replaces it. If anyone raises the capacity
        // above the window, the widening path becomes live again and this
        // test must grow a case for it — so the coupling fails the build
        // rather than going quietly untested.
        // `NAME_CAPACITY - 1`, not `NAME_CAPACITY`: `max_len` counts the C
        // array's NUL slot, exactly as `prvInitialiseNewTask` does, so a
        // configuration asking for 16 gets 15 characters. Asserting 16 here
        // is the mistake this comment exists to stop being made twice.
        let full = Name::new(&"A".repeat(NAME_CAPACITY), NAME_CAPACITY);
        assert_eq!(full.len(), NAME_CAPACITY - 1);
        assert_eq!(full.as_str(), "A".repeat(NAME_CAPACITY - 1));
        // And the buffer CAN be filled to the last byte, when the caller asks
        // for more than the capacity — `as_str` reads `len` rather than
        // scanning for a NUL, so a name with no terminator still reads back.
        // This is also the case that keeps `end == WINDOW` reachable.
        let brim = Name::new(&"B".repeat(NAME_CAPACITY * 2), usize::MAX);
        assert_eq!(brim.len(), NAME_CAPACITY);
        assert_eq!(brim.as_str(), "B".repeat(NAME_CAPACITY));
    }

    #[test]
    fn a_name_longer_than_the_capacity_is_capped() {
        let long = "x".repeat(NAME_CAPACITY * 2);
        let n = Name::new(&long, usize::MAX);
        assert_eq!(n.len(), NAME_CAPACITY);
    }

    /// `is_empty` is `len == 0` both ways round -- a name with characters is
    /// not empty (plan P5: `is_empty -> true` survived every oracle, because
    /// nothing asked a non-empty name).
    #[test]
    fn a_name_with_characters_is_not_empty() {
        assert!(!Name::new("IDLE", 12).is_empty());
        assert!(Name::new("", 12).is_empty());
    }

    /// Both formatters print the name -- `Display` bare, `Debug` quoted, as
    /// a `str` would (plan P5: both survived replaced by `Ok(())`).
    #[test]
    fn display_and_debug_print_the_name() {
        let n = Name::new("Tmr Svc", 12);
        assert_eq!(std::format!("{n}"), "Tmr Svc");
        assert_eq!(std::format!("{n:?}"), "\"Tmr Svc\"");
    }
}

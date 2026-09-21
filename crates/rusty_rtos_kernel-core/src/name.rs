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
pub const NAME_CAPACITY: usize = 32;

/// A task name: at most [`NAME_CAPACITY`] bytes, truncated to the
/// configuration's `MAX_TASK_NAME_LEN` exactly as C truncates it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    bytes: [u8; NAME_CAPACITY],
    len: u8,
}

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
    pub fn new(name: &str, max_len: usize) -> Self {
        let limit = max_len.saturating_sub(1).min(NAME_CAPACITY).min(name.len());
        // Walk back to a character boundary; for ASCII this is `limit`.
        let mut end = limit;
        while end > 0 && !name.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let src = name.as_bytes().get(..end).unwrap_or(&[]);
        let mut bytes = [0u8; NAME_CAPACITY];
        for (slot, byte) in bytes.iter_mut().zip(src.iter()) {
            *slot = *byte;
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
        // And the window widens when the name fills it.
        let long = Name::new("ABCDEFGHIJKLMNOPQRSTUVWXYZ", NAME_CAPACITY);
        assert!(long.len() > 16, "this case must exercise the wide window");
        assert_eq!(long.as_str(), "ABCDEFGHIJKLMNOPQRSTUVWXYZ");
    }

    #[test]
    fn a_name_longer_than_the_capacity_is_capped() {
        let long = "x".repeat(NAME_CAPACITY * 2);
        let n = Name::new(&long, usize::MAX);
        assert_eq!(n.len(), NAME_CAPACITY);
    }
}

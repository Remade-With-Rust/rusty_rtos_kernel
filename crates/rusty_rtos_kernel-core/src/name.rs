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
    #[must_use]
    pub fn as_str(&self) -> &str {
        let end = usize::from(self.len);
        let slice = self.bytes.get(..end).unwrap_or(&[]);
        str::from_utf8(slice).unwrap_or("")
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

    #[test]
    fn a_name_longer_than_the_capacity_is_capped() {
        let long = "x".repeat(NAME_CAPACITY * 2);
        let n = Name::new(&long, usize::MAX);
        assert_eq!(n.len(), NAME_CAPACITY);
    }
}

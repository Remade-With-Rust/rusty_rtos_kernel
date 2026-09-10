//! The Rust face: the same kernel, with the invariants in the type system.
//!
//! Everything else in this crate is FreeRTOS's shape, because that shape is
//! what the conformance corpus proves. This module is the other half of the
//! promise — the API a Rust application should actually hold — and it is
//! built *on* that kernel rather than beside it, so the corpus keeps
//! gating the scheduler underneath both faces (mission plan, K2.1).
//!
//! # What the type system holds here that the C only asserts
//!
//! A FreeRTOS queue copies `uxItemSize` bytes from a pointer you supply.
//! Four things can go wrong and all four are runtime problems there:
//!
//! - the item size and the thing you sent disagree,
//! - you sent a pointer to a stack local that is gone by the time it is
//!   read (`MessageBufferAMP` in the corpus does exactly this, on purpose),
//! - the value is read out as a different type than it went in as,
//! - a send that failed left you unsure whether the value was taken.
//!
//! [`Queue<T, N>`] answers all four by *moving* `T`. The size is the type's,
//! the value is owned rather than pointed at, what comes out is a `T`
//! because nothing else could have gone in, and a send that did not happen
//! hands the value back — [`Sent`] has no variant that drops it, so a
//! caller cannot fail to notice.
//!
//! # What it costs
//!
//! Nothing, and that is a measured claim rather than a design intention.
//! The kernel's own queue still carries the item: a `u64` slot index, the
//! way the timer command queue carries an index into its message ring. So
//! a typed send is one `queue_send` and a typed receive is one
//! `queue_receive` — the same calls, in the same order, taking the same
//! critical sections. `PollQ-typed` in `rusty_rtos_demo` is the same
//! scenario as `PollQ` written against this face, and it is diffed against
//! the *same C oracle trace*: identical, exits included, or the claim is
//! false.

use core::cell::RefCell;
use rusty_rtos_core::error::Result;

use rusty_rtos_core::handle::QueueHandle;
use rusty_rtos_core::isr::Woken;

use crate::queue::Wait;

/// The raw kernel calls the safe face is built from.
///
/// A trait rather than the kernel's inherent methods so that the face is
/// generic over the kernel's thirteen const parameters instead of
/// repeating them, and so that it can be exercised without one.
pub trait Raw {
    /// `xQueueCreate`, in items.
    ///
    /// # Errors
    /// As [`crate::Kernel::queue_create`].
    fn raw_queue_create(&mut self, length: usize) -> Result<QueueHandle>;

    /// `xQueueSendToBack`.
    ///
    /// # Errors
    /// As [`crate::Kernel::queue_send`].
    fn raw_queue_send(&mut self, queue: QueueHandle, value: u64, ticks: u64) -> Result<Wait<()>>;

    /// `xQueueReceive`.
    ///
    /// # Errors
    /// As [`crate::Kernel::queue_receive`].
    fn raw_queue_receive(&mut self, queue: QueueHandle, ticks: u64) -> Result<Wait<u64>>;

    /// `uxQueueMessagesWaiting`, which takes a critical section and so
    /// costs sim time — this is the API call, not a peek at the field.
    ///
    /// # Errors
    /// As [`crate::Kernel::queue_messages_waiting`].
    fn raw_queue_messages_waiting(&mut self, queue: QueueHandle) -> Result<usize>;

    /// Whether the queue could take one more item, *without* the critical
    /// section `uxQueueSpacesAvailable` would take.
    ///
    /// This is the kernel reading its own queue rather than a task asking
    /// it a question, so it must cost no sim time — the same rule the timer
    /// command queue's `timer_queue_has_room` follows, and for the same
    /// reason: a slot in the ring may only be written when the queue can
    /// take the index that names it.
    fn raw_queue_has_room(&self, queue: QueueHandle) -> bool;

    /// Whether the kernel is in interrupt context — `xPortIsInsideInterrupt`.
    fn raw_in_isr(&self) -> bool;

    /// `xQueueSendToBackFromISR`.
    ///
    /// # Errors
    /// As [`crate::Kernel::queue_send_from_isr`].
    fn raw_queue_send_from_isr(&mut self, queue: QueueHandle, value: u64) -> Result<Woken>;

    /// `xSemaphoreCreateMutex`.
    ///
    /// # Errors
    /// As [`crate::Kernel::mutex_create`].
    fn raw_mutex_create(&mut self) -> Result<QueueHandle>;

    /// `xSemaphoreTake`.
    ///
    /// # Errors
    /// As [`crate::Kernel::semaphore_take`].
    fn raw_mutex_take(&mut self, mutex: QueueHandle, ticks: u64) -> Result<Wait<()>>;

    /// `xSemaphoreGive`.
    ///
    /// # Errors
    /// As [`crate::Kernel::semaphore_give`].
    fn raw_mutex_give(&mut self, mutex: QueueHandle) -> Result<()>;
}

/// Proof that the code holding it is running in interrupt context.
///
/// # The bug this exists to delete
///
/// FreeRTOS has two of almost every call — `xQueueSend` and
/// `xQueueSendFromISR` — and picking the wrong one is a bug the compiler
/// cannot see. From an interrupt the task version can try to block, which
/// on most ports corrupts the scheduler; from a task the ISR version takes
/// the wrong lock. FreeRTOS's own answer is `configASSERT(
/// xPortIsInsideInterrupt() )` on the ports that can tell, and silence on
/// the ones that cannot.
///
/// Here the two halves live on two different types. A task holds `&mut K`
/// and can reach [`Queue::send`]; an interrupt holds `&mut Isr<K>` and can
/// reach [`Queue::send_from_isr`]; neither can reach the other's. There is
/// exactly one runtime check, at the boundary where [`Isr::with`] hands the
/// token out, and everything downstream of it is the type system.
///
/// The shape is [`critical_section::with`]'s, and for the same reason: a
/// capability that cannot outlive the context that granted it has to be
/// borrowed inside a closure rather than returned.
///
/// [`critical_section::with`]: https://docs.rs/critical-section
///
/// # The two halves cannot be confused
///
/// The control arm: an interrupt takes the capability and uses the from-ISR
/// half, a task uses the task half, and both work.
///
/// ```
/// # use rusty_rtos_kernel_core::typed::{Isr, Queue, Raw, Sent};
/// # use rusty_rtos_kernel_core::queue::Wait;
/// # use rusty_rtos_core::error::Result;
/// # use rusty_rtos_core::handle::QueueHandle;
/// # use rusty_rtos_core::isr::Woken;
/// # #[derive(Default)]
/// # struct Fake { held: Vec<u64>, in_isr: bool }
/// # impl Raw for Fake {
/// #     fn raw_queue_create(&mut self, _n: usize) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(1)) }
/// #     fn raw_queue_send(&mut self, _q: QueueHandle, v: u64, _t: u64) -> Result<Wait<()>> { self.held.push(v); Ok(Wait::Ready(())) }
/// #     fn raw_queue_receive(&mut self, _q: QueueHandle, _t: u64) -> Result<Wait<u64>> {
/// #         if self.held.is_empty() { Ok(Wait::Blocked) } else { Ok(Wait::Ready(self.held.remove(0))) }
/// #     }
/// #     fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> { Ok(self.held.len()) }
/// #     fn raw_queue_has_room(&self, _q: QueueHandle) -> bool { self.held.len() < 4 }
/// #     fn raw_in_isr(&self) -> bool { self.in_isr }
/// #     fn raw_queue_send_from_isr(&mut self, _q: QueueHandle, v: u64) -> Result<Woken> { self.held.push(v); Ok(Woken::NO) }
/// #     fn raw_mutex_create(&mut self) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(2)) }
/// #     fn raw_mutex_take(&mut self, _m: QueueHandle, _t: u64) -> Result<Wait<()>> { Ok(Wait::Ready(())) }
/// #     fn raw_mutex_give(&mut self, _m: QueueHandle) -> Result<()> { Ok(()) }
/// # }
/// let k = &mut Fake { in_isr: true, ..Fake::default() };
/// let mut q = Queue::<u16, 4>::create(k).unwrap();
/// // In interrupt context the capability is granted, and the from-ISR
/// // half is what is reachable through it.
/// let sent = Isr::with(k, |isr| q.send_from_isr(isr, 7_u16).is_ok());
/// assert_eq!(sent, Some(true));
/// // Out of interrupt context it is refused, and `f` never runs.
/// k.in_isr = false;
/// assert!(Isr::with(k, |_| unreachable!()).is_none());
/// // ...and the task half is the one that works here.
/// assert!(q.send(k, 8_u16, 0).is_ok());
/// ```
///
/// **A task cannot reach the from-ISR half.** In C both are in scope from
/// everywhere and `xQueueSendFromISR` from a task takes the wrong lock:
///
/// ```compile_fail
/// # use rusty_rtos_kernel_core::typed::{Isr, Queue, Raw, Sent};
/// # use rusty_rtos_kernel_core::queue::Wait;
/// # use rusty_rtos_core::error::Result;
/// # use rusty_rtos_core::handle::QueueHandle;
/// # use rusty_rtos_core::isr::Woken;
/// # #[derive(Default)]
/// # struct Fake { held: Vec<u64>, in_isr: bool }
/// # impl Raw for Fake {
/// #     fn raw_queue_create(&mut self, _n: usize) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(1)) }
/// #     fn raw_queue_send(&mut self, _q: QueueHandle, v: u64, _t: u64) -> Result<Wait<()>> { self.held.push(v); Ok(Wait::Ready(())) }
/// #     fn raw_queue_receive(&mut self, _q: QueueHandle, _t: u64) -> Result<Wait<u64>> {
/// #         if self.held.is_empty() { Ok(Wait::Blocked) } else { Ok(Wait::Ready(self.held.remove(0))) }
/// #     }
/// #     fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> { Ok(self.held.len()) }
/// #     fn raw_queue_has_room(&self, _q: QueueHandle) -> bool { self.held.len() < 4 }
/// #     fn raw_in_isr(&self) -> bool { self.in_isr }
/// #     fn raw_queue_send_from_isr(&mut self, _q: QueueHandle, v: u64) -> Result<Woken> { self.held.push(v); Ok(Woken::NO) }
/// #     fn raw_mutex_create(&mut self) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(2)) }
/// #     fn raw_mutex_take(&mut self, _m: QueueHandle, _t: u64) -> Result<Wait<()>> { Ok(Wait::Ready(())) }
/// #     fn raw_mutex_give(&mut self, _m: QueueHandle) -> Result<()> { Ok(()) }
/// # }
/// let k = &mut Fake::default();
/// let mut q = Queue::<u16, 4>::create(k).unwrap();
/// // `send_from_isr` wants an `Isr`, and a task has only the kernel.
/// let _ = q.send_from_isr(k, 7_u16);
/// ```
///
/// **An interrupt cannot reach the task half.** In C this is the one that
/// corrupts the scheduler, because the task version may try to block:
///
/// ```compile_fail
/// # use rusty_rtos_kernel_core::typed::{Isr, Queue, Raw, Sent};
/// # use rusty_rtos_kernel_core::queue::Wait;
/// # use rusty_rtos_core::error::Result;
/// # use rusty_rtos_core::handle::QueueHandle;
/// # use rusty_rtos_core::isr::Woken;
/// # #[derive(Default)]
/// # struct Fake { held: Vec<u64>, in_isr: bool }
/// # impl Raw for Fake {
/// #     fn raw_queue_create(&mut self, _n: usize) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(1)) }
/// #     fn raw_queue_send(&mut self, _q: QueueHandle, v: u64, _t: u64) -> Result<Wait<()>> { self.held.push(v); Ok(Wait::Ready(())) }
/// #     fn raw_queue_receive(&mut self, _q: QueueHandle, _t: u64) -> Result<Wait<u64>> {
/// #         if self.held.is_empty() { Ok(Wait::Blocked) } else { Ok(Wait::Ready(self.held.remove(0))) }
/// #     }
/// #     fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> { Ok(self.held.len()) }
/// #     fn raw_queue_has_room(&self, _q: QueueHandle) -> bool { self.held.len() < 4 }
/// #     fn raw_in_isr(&self) -> bool { self.in_isr }
/// #     fn raw_queue_send_from_isr(&mut self, _q: QueueHandle, v: u64) -> Result<Woken> { self.held.push(v); Ok(Woken::NO) }
/// #     fn raw_mutex_create(&mut self) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(2)) }
/// #     fn raw_mutex_take(&mut self, _m: QueueHandle, _t: u64) -> Result<Wait<()>> { Ok(Wait::Ready(())) }
/// #     fn raw_mutex_give(&mut self, _m: QueueHandle) -> Result<()> { Ok(()) }
/// # }
/// let k = &mut Fake { in_isr: true, ..Fake::default() };
/// let mut q = Queue::<u16, 4>::create(k).unwrap();
/// Isr::with(k, |isr| {
///     // `send` wants the kernel, and an interrupt has only the `Isr`.
///     let _ = q.send(isr, 7_u16, 0);
/// });
/// ```
#[derive(Debug)]
pub struct Isr<'a, K: Raw> {
    kernel: &'a mut K,
}

impl<K: Raw> Isr<'_, K> {
    /// Run `f` with interrupt-context capability, if this really is
    /// interrupt context.
    ///
    /// Answers `None` — and does not run `f` — when it is not, which is the
    /// `configASSERT` the C would have fired, moved to a place where the
    /// caller has to look at it.
    pub fn with<R>(kernel: &mut K, f: impl FnOnce(&mut Isr<'_, K>) -> R) -> Option<R> {
        if !kernel.raw_in_isr() {
            return None;
        }
        Some(f(&mut Isr { kernel }))
    }
}

/// What became of a value handed to [`Queue::send`].
///
/// There is deliberately no variant that drops the value. A FreeRTOS send
/// that fails leaves the caller's copy where it was and says `pdFAIL`,
/// which is easy to ignore; a move has to say where the thing went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a send that did not happen is holding your value"]
pub enum Sent<T> {
    /// The queue took it.
    Ok,
    /// The queue was full and the send did not block: here it is back.
    Full(T),
    /// The send would have blocked. In a kernel with no stacks the call
    /// returns so the caller can be re-entered; hand the value back to the
    /// same `send` when this task next runs.
    Blocked(T),
}

impl<T> Sent<T> {
    /// Whether the queue took it — `== pdPASS`.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// The value back, if the queue did not take it.
    #[must_use]
    pub fn into_value(self) -> Option<T> {
        match self {
            Self::Ok => None,
            Self::Full(value) | Self::Blocked(value) => Some(value),
        }
    }
}

/// A mutex that **owns what it protects**.
///
/// # The bug this exists to delete
///
/// A FreeRTOS mutex protects nothing. It is a counting semaphore with
/// priority inheritance and a name, and the data it is *said* to guard is
/// an unrelated variable somewhere else. Nothing connects the two but a
/// comment and everybody's good intentions, so "forgot to take the mutex"
/// is a bug that compiles, ships, and shows up as corruption under load
/// years later.
///
/// Here the data is *inside* the mutex and there is no way to reach it
/// except through [`Mutex::with`], which takes the lock first and gives it
/// back after. There is no `get`, no `lock` that returns the value, and no
/// field you can reach: not because they are discouraged, but because they
/// do not exist.
///
/// # Why a closure rather than a guard
///
/// A guard would have to give the mutex back when it dropped, and `Drop`
/// cannot reach the kernel — it takes no arguments, and this crate has no
/// globals and no allocator to hide one in. A guard you must remember to
/// release is the same bug in a new coat. The closure cannot be forgotten.
///
/// It is the shape [`Isr::with`] uses, and the standard one for a
/// capability that must not outlive its context.
///
/// # What it costs
///
/// One `xSemaphoreTake` and one `xSemaphoreGive`, which is what the C
/// would have done — plus a [`RefCell`] borrow, which is a counter and
/// which can never fail here: the RTOS mutex has already serialised every
/// task that could reach it. That redundancy is the price of
/// `forbid(unsafe)`, and it is a compare-and-branch.
///
/// ```
/// # use rusty_rtos_kernel_core::typed::{Mutex, Raw};
/// # use rusty_rtos_kernel_core::queue::Wait;
/// # use rusty_rtos_core::error::Result;
/// # use rusty_rtos_core::handle::QueueHandle;
/// # use rusty_rtos_core::isr::Woken;
/// # #[derive(Default)]
/// # struct Fake { taken: bool }
/// # impl Raw for Fake {
/// #     fn raw_queue_create(&mut self, _n: usize) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(1)) }
/// #     fn raw_queue_send(&mut self, _q: QueueHandle, _v: u64, _t: u64) -> Result<Wait<()>> { Ok(Wait::Ready(())) }
/// #     fn raw_queue_receive(&mut self, _q: QueueHandle, _t: u64) -> Result<Wait<u64>> { Ok(Wait::Blocked) }
/// #     fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> { Ok(0) }
/// #     fn raw_queue_has_room(&self, _q: QueueHandle) -> bool { true }
/// #     fn raw_in_isr(&self) -> bool { false }
/// #     fn raw_queue_send_from_isr(&mut self, _q: QueueHandle, _v: u64) -> Result<Woken> { Ok(Woken::NO) }
/// #     fn raw_mutex_create(&mut self) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(2)) }
/// #     fn raw_mutex_take(&mut self, _m: QueueHandle, _t: u64) -> Result<Wait<()>> { self.taken = true; Ok(Wait::Ready(())) }
/// #     fn raw_mutex_give(&mut self, _m: QueueHandle) -> Result<()> { self.taken = false; Ok(()) }
/// # }
/// # let k = &mut Fake::default();
/// let counter = Mutex::new(k, 0_u32).unwrap();
///
/// // The only way in. The lock is taken before `f` and given back after.
/// let seen = counter.with(k, 0, |n| { *n += 1; *n }).unwrap();
/// assert_eq!(seen, 1);
/// assert!(!k.taken);
/// ```
///
/// **The data cannot be reached without the lock.** In C the variable is
/// simply in scope:
///
/// ```compile_fail
/// # use rusty_rtos_kernel_core::typed::{Mutex, Raw};
/// # use rusty_rtos_kernel_core::queue::Wait;
/// # use rusty_rtos_core::error::Result;
/// # use rusty_rtos_core::handle::QueueHandle;
/// # use rusty_rtos_core::isr::Woken;
/// # #[derive(Default)]
/// # struct Fake { taken: bool }
/// # impl Raw for Fake {
/// #     fn raw_queue_create(&mut self, _n: usize) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(1)) }
/// #     fn raw_queue_send(&mut self, _q: QueueHandle, _v: u64, _t: u64) -> Result<Wait<()>> { Ok(Wait::Ready(())) }
/// #     fn raw_queue_receive(&mut self, _q: QueueHandle, _t: u64) -> Result<Wait<u64>> { Ok(Wait::Blocked) }
/// #     fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> { Ok(0) }
/// #     fn raw_queue_has_room(&self, _q: QueueHandle) -> bool { true }
/// #     fn raw_in_isr(&self) -> bool { false }
/// #     fn raw_queue_send_from_isr(&mut self, _q: QueueHandle, _v: u64) -> Result<Woken> { Ok(Woken::NO) }
/// #     fn raw_mutex_create(&mut self) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(2)) }
/// #     fn raw_mutex_take(&mut self, _m: QueueHandle, _t: u64) -> Result<Wait<()>> { self.taken = true; Ok(Wait::Ready(())) }
/// #     fn raw_mutex_give(&mut self, _m: QueueHandle) -> Result<()> { self.taken = false; Ok(()) }
/// # }
/// # let k = &mut Fake::default();
/// let counter = Mutex::new(k, 0_u32).unwrap();
/// // There is no way to say this. The field is private and there is no
/// // accessor that does not take the lock first.
/// let _n = counter.value;
/// ```
#[derive(Debug)]
pub struct Mutex<T> {
    handle: QueueHandle,
    value: RefCell<T>,
}

impl<T> Mutex<T> {
    /// `xSemaphoreCreateMutex`, with the thing it protects.
    ///
    /// # Errors
    /// As [`Raw::raw_mutex_create`].
    pub fn new<K: Raw>(kernel: &mut K, value: T) -> Result<Self> {
        Ok(Self {
            handle: kernel.raw_mutex_create()?,
            value: RefCell::new(value),
        })
    }

    /// The handle the kernel knows this mutex by, for the calls the face
    /// does not cover yet.
    #[must_use]
    pub const fn handle(&self) -> QueueHandle {
        self.handle
    }

    /// Take the mutex, run `f` on what it protects, and give it back.
    ///
    /// Answers `None` — without running `f` — when the mutex could not be
    /// taken in `ticks`, which is the C's `xSemaphoreTake( ... ) != pdPASS`
    /// branch that is so easy to leave empty.
    pub fn with<K: Raw, R>(
        &self,
        kernel: &mut K,
        ticks: u64,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        if !matches!(
            kernel.raw_mutex_take(self.handle, ticks),
            Ok(Wait::Ready(()))
        ) {
            return None;
        }
        // The RTOS mutex has serialised everyone who could reach this, so
        // the borrow cannot fail. `try_borrow_mut` rather than `borrow_mut`
        // because this crate does not panic, ever, for any reason.
        let answer = self
            .value
            .try_borrow_mut()
            .ok()
            .map(|mut value| f(&mut value));
        let _ = kernel.raw_mutex_give(self.handle);
        answer
    }
}

/// What became of a value handed to [`Queue::send_from_isr`].
///
/// There is no `Blocked`: an interrupt never blocks, which is the whole
/// reason the from-ISR half exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a send that did not happen is holding your value"]
pub enum SentFromIsr<T> {
    /// The queue took it, and this is whether a higher-priority task woke.
    Ok(Woken),
    /// The queue was full: here it is back.
    Full(T),
}

impl<T> SentFromIsr<T> {
    /// Whether the queue took it — `== pdPASS`.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// Whether a higher-priority task was woken and a yield is owed.
    pub const fn woken(&self) -> Woken {
        match self {
            Self::Ok(woken) => *woken,
            Self::Full(_) => Woken::NO,
        }
    }
}

/// A queue that moves `T` instead of copying bytes.
///
/// `N` is the length in items, and it is a const parameter rather than a
/// runtime argument because the storage is here: a queue that cannot be
/// created with the wrong capacity cannot be sent to with the wrong one
/// either.
///
/// The kernel's queue underneath carries slot indices, so the ring and the
/// queue are always the same length and a slot is only ever written when
/// the queue has room for the index that names it. That rule is not an
/// optimisation — writing first and sending after is exactly the bug that
/// cost `TimerDemo` its first divergence, where a refused command
/// overwrote a message whose index was still queued.
///
/// It is [`Copy`] exactly when `T` is, which is the derive's own rule and
/// the right one: a queue of values that can be duplicated can be, and a
/// queue of values that cannot, cannot.
///
/// # The refusals
///
/// These are the bug classes the C-shaped API cannot refuse, each one a
/// `compile_fail` doctest paired with the working line it is one character
/// away from — because a "this must not compile" test that fails for the
/// wrong reason is worse than no test at all. The pairing is the control
/// arm: the good version below compiles, so the bad versions do not fail
/// because the setup was broken.
///
/// It compiles when the types agree — and note the `deny` here, which is
/// the same one the third refusal below relies on. The control arm carries
/// it deliberately: it proves the attribute is not itself what makes that
/// one fail.
///
/// ```
/// #![deny(unused_must_use)]
/// # use rusty_rtos_kernel_core::typed::{Queue, Raw, Sent};
/// # use rusty_rtos_kernel_core::queue::Wait;
/// # use rusty_rtos_core::error::Result;
/// # use rusty_rtos_core::handle::QueueHandle;
/// # use rusty_rtos_core::isr::Woken;
/// # #[derive(Default)]
/// # struct Fake { held: Vec<u64> }
/// # impl Raw for Fake {
/// #     fn raw_queue_create(&mut self, _n: usize) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(1)) }
/// #     fn raw_queue_send(&mut self, _q: QueueHandle, v: u64, _t: u64) -> Result<Wait<()>> { self.held.push(v); Ok(Wait::Ready(())) }
/// #     fn raw_queue_receive(&mut self, _q: QueueHandle, _t: u64) -> Result<Wait<u64>> {
/// #         if self.held.is_empty() { Ok(Wait::Blocked) } else { Ok(Wait::Ready(self.held.remove(0))) }
/// #     }
/// #     fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> { Ok(self.held.len()) }
/// #     fn raw_queue_has_room(&self, _q: QueueHandle) -> bool { self.held.len() < 4 }
/// #     fn raw_in_isr(&self) -> bool { false }
/// #     fn raw_queue_send_from_isr(&mut self, _q: QueueHandle, v: u64) -> Result<Woken> { self.held.push(v); Ok(Woken::NO) }
/// #     fn raw_mutex_create(&mut self) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(2)) }
/// #     fn raw_mutex_take(&mut self, _m: QueueHandle, _t: u64) -> Result<Wait<()>> { Ok(Wait::Ready(())) }
/// #     fn raw_mutex_give(&mut self, _m: QueueHandle) -> Result<()> { Ok(()) }
/// # }
/// # let k = &mut Fake::default();
/// let mut q = Queue::<u16, 4>::create(k).unwrap();
/// assert!(q.send(k, 7_u16, 0).is_ok());
/// let out: Option<u16> = match q.receive(k, 0).unwrap() {
///     Wait::Ready(v) => v,
///     Wait::Blocked => None,
/// };
/// assert_eq!(out, Some(7));
/// ```
///
/// **The item size cannot disagree with the item.** In C this is
/// `xQueueCreate( n, sizeof( uint16_t ) )` and then a send of something
/// else, which copies the wrong number of bytes and is found, if at all,
/// much later:
///
/// ```compile_fail
/// # use rusty_rtos_kernel_core::typed::{Queue, Raw, Sent};
/// # use rusty_rtos_kernel_core::queue::Wait;
/// # use rusty_rtos_core::error::Result;
/// # use rusty_rtos_core::handle::QueueHandle;
/// # use rusty_rtos_core::isr::Woken;
/// # #[derive(Default)]
/// # struct Fake { held: Vec<u64> }
/// # impl Raw for Fake {
/// #     fn raw_queue_create(&mut self, _n: usize) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(1)) }
/// #     fn raw_queue_send(&mut self, _q: QueueHandle, v: u64, _t: u64) -> Result<Wait<()>> { self.held.push(v); Ok(Wait::Ready(())) }
/// #     fn raw_queue_receive(&mut self, _q: QueueHandle, _t: u64) -> Result<Wait<u64>> {
/// #         if self.held.is_empty() { Ok(Wait::Blocked) } else { Ok(Wait::Ready(self.held.remove(0))) }
/// #     }
/// #     fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> { Ok(self.held.len()) }
/// #     fn raw_queue_has_room(&self, _q: QueueHandle) -> bool { self.held.len() < 4 }
/// #     fn raw_in_isr(&self) -> bool { false }
/// #     fn raw_queue_send_from_isr(&mut self, _q: QueueHandle, v: u64) -> Result<Woken> { self.held.push(v); Ok(Woken::NO) }
/// #     fn raw_mutex_create(&mut self) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(2)) }
/// #     fn raw_mutex_take(&mut self, _m: QueueHandle, _t: u64) -> Result<Wait<()>> { Ok(Wait::Ready(())) }
/// #     fn raw_mutex_give(&mut self, _m: QueueHandle) -> Result<()> { Ok(()) }
/// # }
/// # let k = &mut Fake::default();
/// let mut q = Queue::<u16, 4>::create(k).unwrap();
/// // `u32` is not `u16`, and the queue's item type is not a comment.
/// let _ = q.send(k, 7_u32, 0);
/// ```
///
/// **What comes out is what went in.** In C a receive fills a buffer you
/// nominate, and nothing checks that you nominated the right type:
///
/// ```compile_fail
/// # use rusty_rtos_kernel_core::typed::{Queue, Raw, Sent};
/// # use rusty_rtos_kernel_core::queue::Wait;
/// # use rusty_rtos_core::error::Result;
/// # use rusty_rtos_core::handle::QueueHandle;
/// # use rusty_rtos_core::isr::Woken;
/// # #[derive(Default)]
/// # struct Fake { held: Vec<u64> }
/// # impl Raw for Fake {
/// #     fn raw_queue_create(&mut self, _n: usize) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(1)) }
/// #     fn raw_queue_send(&mut self, _q: QueueHandle, v: u64, _t: u64) -> Result<Wait<()>> { self.held.push(v); Ok(Wait::Ready(())) }
/// #     fn raw_queue_receive(&mut self, _q: QueueHandle, _t: u64) -> Result<Wait<u64>> {
/// #         if self.held.is_empty() { Ok(Wait::Blocked) } else { Ok(Wait::Ready(self.held.remove(0))) }
/// #     }
/// #     fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> { Ok(self.held.len()) }
/// #     fn raw_queue_has_room(&self, _q: QueueHandle) -> bool { self.held.len() < 4 }
/// #     fn raw_in_isr(&self) -> bool { false }
/// #     fn raw_queue_send_from_isr(&mut self, _q: QueueHandle, v: u64) -> Result<Woken> { self.held.push(v); Ok(Woken::NO) }
/// #     fn raw_mutex_create(&mut self) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(2)) }
/// #     fn raw_mutex_take(&mut self, _m: QueueHandle, _t: u64) -> Result<Wait<()>> { Ok(Wait::Ready(())) }
/// #     fn raw_mutex_give(&mut self, _m: QueueHandle) -> Result<()> { Ok(()) }
/// # }
/// # let k = &mut Fake::default();
/// let mut q = Queue::<u16, 4>::create(k).unwrap();
/// let _ = q.send(k, 7_u16, 0);
/// // The queue carries `u16`; a `u32` cannot come out of it.
/// let _out: Option<u32> = match q.receive(k, 0).unwrap() {
///     Wait::Ready(v) => v,
///     Wait::Blocked => None,
/// };
/// ```
///
/// **A send that failed cannot be ignored.** `xQueueSend`'s return is a
/// `BaseType_t` like any other, and dropping it on the floor is legal C;
/// here the value itself is in the answer, so ignoring it drops the thing
/// you were trying to send:
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// # use rusty_rtos_kernel_core::typed::{Queue, Raw, Sent};
/// # use rusty_rtos_kernel_core::queue::Wait;
/// # use rusty_rtos_core::error::Result;
/// # use rusty_rtos_core::handle::QueueHandle;
/// # use rusty_rtos_core::isr::Woken;
/// # #[derive(Default)]
/// # struct Fake { held: Vec<u64> }
/// # impl Raw for Fake {
/// #     fn raw_queue_create(&mut self, _n: usize) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(1)) }
/// #     fn raw_queue_send(&mut self, _q: QueueHandle, v: u64, _t: u64) -> Result<Wait<()>> { self.held.push(v); Ok(Wait::Ready(())) }
/// #     fn raw_queue_receive(&mut self, _q: QueueHandle, _t: u64) -> Result<Wait<u64>> {
/// #         if self.held.is_empty() { Ok(Wait::Blocked) } else { Ok(Wait::Ready(self.held.remove(0))) }
/// #     }
/// #     fn raw_queue_messages_waiting(&mut self, _q: QueueHandle) -> Result<usize> { Ok(self.held.len()) }
/// #     fn raw_queue_has_room(&self, _q: QueueHandle) -> bool { self.held.len() < 4 }
/// #     fn raw_in_isr(&self) -> bool { false }
/// #     fn raw_queue_send_from_isr(&mut self, _q: QueueHandle, v: u64) -> Result<Woken> { self.held.push(v); Ok(Woken::NO) }
/// #     fn raw_mutex_create(&mut self) -> Result<QueueHandle> { Ok(QueueHandle::from_raw(2)) }
/// #     fn raw_mutex_take(&mut self, _m: QueueHandle, _t: u64) -> Result<Wait<()>> { Ok(Wait::Ready(())) }
/// #     fn raw_mutex_give(&mut self, _m: QueueHandle) -> Result<()> { Ok(()) }
/// # }
/// # let k = &mut Fake::default();
/// let mut q = Queue::<u16, 4>::create(k).unwrap();
/// // `Sent` is `#[must_use]`, and it is holding your `u16`.
/// q.send(k, 7_u16, 0);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Queue<T, const N: usize> {
    handle: QueueHandle,
    slots: [Option<T>; N],
    next: usize,
}

impl<T, const N: usize> Queue<T, N> {
    /// `xQueueCreate`, with the item type and the length in one place.
    ///
    /// # Errors
    /// As [`Raw::raw_queue_create`] — [`rusty_rtos_core::error::Error::Full`]
    /// when the kernel's queue arena is full.
    pub fn create<K: Raw>(kernel: &mut K) -> Result<Self> {
        Ok(Self {
            handle: kernel.raw_queue_create(N)?,
            slots: [const { None }; N],
            next: 0,
        })
    }

    /// The handle the kernel knows this queue by, for the calls the face
    /// does not cover yet.
    #[must_use]
    pub const fn handle(&self) -> QueueHandle {
        self.handle
    }

    /// `xQueueSendToBack`, moving the value in.
    ///
    /// A `ticks` of zero is the C's `xQueueSend( ..., 0 )`. Anything larger
    /// can answer [`Sent::Blocked`], which is this kernel's "the call would
    /// have blocked, call me again" and not an error.
    pub fn send<K: Raw>(&mut self, kernel: &mut K, value: T, ticks: u64) -> Sent<T> {
        if !kernel.raw_queue_has_room(self.handle) {
            // No room for the index, so no room for the item: leave the
            // slot alone. Making the call anyway is what gives the trace
            // its `QUEUE_SEND_FAILED`, and what blocks when it should.
            return match kernel.raw_queue_send(self.handle, 0, ticks) {
                Ok(Wait::Blocked) => Sent::Blocked(value),
                _ => Sent::Full(value),
            };
        }
        let slot = self.next;
        if let Some(cell) = self.slots.get_mut(slot) {
            *cell = Some(value);
        }
        match kernel.raw_queue_send(self.handle, slot as u64, ticks) {
            Ok(Wait::Ready(())) => {
                self.next = slot.saturating_add(1).checked_rem(N).unwrap_or(0);
                Sent::Ok
            }
            other => {
                // The queue refused it after all, so the slot is still
                // ours; take the value back out of it.
                let value = self.slots.get_mut(slot).and_then(Option::take);
                match (other, value) {
                    (Ok(Wait::Blocked), Some(value)) => Sent::Blocked(value),
                    (_, Some(value)) => Sent::Full(value),
                    // Unreachable: the slot was written two lines above.
                    (_, None) => Sent::Ok,
                }
            }
        }
    }

    /// `xQueueReceive`, moving the value out.
    ///
    /// # Errors
    /// As [`Raw::raw_queue_receive`].
    pub fn receive<K: Raw>(&mut self, kernel: &mut K, ticks: u64) -> Result<Wait<Option<T>>> {
        match kernel.raw_queue_receive(self.handle, ticks)? {
            Wait::Ready(slot) => {
                let value = self.slots.get_mut(slot as usize).and_then(Option::take);
                Ok(Wait::Ready(value))
            }
            Wait::Blocked => Ok(Wait::Blocked),
        }
    }

    /// `xQueueSendToBackFromISR`, moving the value in.
    ///
    /// It takes an [`Isr`] rather than the kernel, so it can only be
    /// reached from interrupt context — and [`Queue::send`], which takes the
    /// kernel, can only be reached from a task. Choosing the wrong half is
    /// a type error rather than a `configASSERT`.
    ///
    /// The answer carries whether a higher-priority task was woken, which
    /// is the C's `pxHigherPriorityTaskWoken` out-parameter turned into a
    /// return value that cannot be passed as `NULL` by accident.
    pub fn send_from_isr<K: Raw>(&mut self, isr: &mut Isr<'_, K>, value: T) -> SentFromIsr<T> {
        let kernel = &mut *isr.kernel;
        if !kernel.raw_queue_has_room(self.handle) {
            let _ = kernel.raw_queue_send_from_isr(self.handle, 0);
            return SentFromIsr::Full(value);
        }
        let slot = self.next;
        if let Some(cell) = self.slots.get_mut(slot) {
            *cell = Some(value);
        }
        match kernel.raw_queue_send_from_isr(self.handle, slot as u64) {
            Ok(woken) => {
                self.next = slot.saturating_add(1).checked_rem(N).unwrap_or(0);
                SentFromIsr::Ok(woken)
            }
            Err(_) => match self.slots.get_mut(slot).and_then(Option::take) {
                Some(value) => SentFromIsr::Full(value),
                // Unreachable: the slot was written two lines above.
                None => SentFromIsr::Ok(Woken::NO),
            },
        }
    }

    /// `uxQueueMessagesWaiting`.
    ///
    /// # Errors
    /// As [`Raw::raw_queue_messages_waiting`].
    pub fn len<K: Raw>(&self, kernel: &mut K) -> Result<usize> {
        kernel.raw_queue_messages_waiting(self.handle)
    }

    /// Whether the queue holds nothing.
    ///
    /// # Errors
    /// As [`Queue::len`].
    pub fn is_empty<K: Raw>(&self, kernel: &mut K) -> Result<bool> {
        Ok(self.len(kernel)? == 0)
    }
}

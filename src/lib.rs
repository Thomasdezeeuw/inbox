//! Bounded capacity channel.
//!
//! The channel is a multi-producer, single-consumer (MPSC) bounded queue. It is
//! designed to be used as inbox for actors, following the [actor model].
//!
//! [actor model]: https://en.wikipedia.org/wiki/Actor_model
//!
//! # Notes
//!
//! The implementation assumes the access to the channel is mostly uncontested
//! and optimises for this use case. Furthermore it optimises for small memory
//! footprint, sometimes over faster access.
//!
//! The implementation doesn't provide a lot of guarantees. For example this
//! channel is **not** guaranteed to be First In First Out (FIFO), it does this
//! on a best effort basis. In return it means that a slow `Sender` does not
//! block the receiving of other messages.
//!
//! # Examples
//!
//! Simple creation of a channel and sending a message over it.
//!
//! ```
//! use std::thread;
//!
//! use inbox::RecvError;
//!
//! // Create a new small channel.
//! let (sender, mut receiver) = inbox::new_small();
//!
//! let sender_handle = thread::spawn(move || {
//!     if let Err(err) = sender.try_send("Hello world!".to_owned()) {
//!         panic!("Failed to send value: {}", err);
//!     }
//! });
//!
//! let receiver_handle = thread::spawn(move || {
//!     # thread::sleep(std::time::Duration::from_millis(1)); // Don't waste cycles.
//!     // NOTE: this is just an example don't actually use a loop like this, it
//!     // will waste CPU cycles when the channel is empty!
//!     loop {
//!         match receiver.try_recv() {
//!             Ok(value) => println!("Got a value: {}", value),
//!             Err(RecvError::Empty) => continue,
//!             Err(RecvError::Disconnected) => break,
//!         }
//!     }
//! });
//!
//! sender_handle.join().unwrap();
//! receiver_handle.join().unwrap();
//! ```

#![feature(maybe_uninit_extra)]
#![warn(
    missing_debug_implementations,
    missing_docs,
    unused_results,
    variant_size_differences
)]
// Disallow warnings when running tests.
#![cfg_attr(test, deny(warnings))]
// Disallow warnings in examples, we want to set a good example after all.
#![doc(test(attr(deny(warnings))))]

use std::alloc::{alloc, handle_alloc_error, Layout};
use std::cell::UnsafeCell;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomPinned;
use std::mem::{size_of, MaybeUninit};
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::sync::atomic::{fence, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::task::{self, Poll};

use parking_lot::Mutex;

#[cfg(test)]
mod tests;

pub mod oneshot;

mod waker;
use waker::WakerRegistration;

/// The capacity of a small channel.
const SMALL_CAP: usize = 8;
/// Maximum capacity of a channel.
// NOTE: see [`Channel::new`] why.
pub const MAX_CAP: usize = 29;
/// Minimum capacity of a channel.
pub const MIN_CAP: usize = 1;

/// Create a small bounded channel.
pub fn new_small<T>() -> (Sender<T>, Receiver<T>) {
    new(SMALL_CAP)
}

/// Create a new bounded channel.
///
/// The `capacity` must be in the range [`MIN_CAP`]`..=`[`MAX_CAP`].
pub fn new<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(
        (MIN_CAP..=MAX_CAP).contains(&capacity),
        "inbox channel capacity must be between {} and {}",
        MIN_CAP,
        MAX_CAP
    );
    let channel = Channel::new(capacity);
    let sender = Sender { channel };
    let receiver = Receiver { channel };
    (sender, receiver)
}

/// Bit mask to mark the receiver as alive.
const RECEIVER_ALIVE: usize = 1 << (size_of::<usize>() * 8 - 1);
/// Bit mask to mark the manager as alive.
const MANAGER_ALIVE: usize = 1 << (size_of::<usize>() * 8 - 2);

/// Returns `true` if the manager is alive in `status`.
#[inline(always)]
const fn has_manager(status: usize) -> bool {
    status & MANAGER_ALIVE != 0
}

// Bits to mark the status of a slot.
const STATUS_BITS: u64 = 2; // Number of bits used per slot.
const STATUS_MASK: u64 = (1 << STATUS_BITS) - 1;
#[cfg(test)]
const ALL_STATUSES_MASK: u64 = (1 << (MAX_CAP as u64 * STATUS_BITS)) - 1;
// The possible statuses of a slot.
const EMPTY: u64 = 0b00; // Slot is empty (initial state).
const TAKEN: u64 = 0b01; // `Sender` acquired write access, currently writing.
const FILLED: u64 = 0b11; // `Sender` wrote a value into the slot.
const READING: u64 = 0b10; // A `Receiver` is reading from the slot.

// Status transitions.
const MARK_TAKEN: u64 = 0b01; // OR to go from EMPTY -> TAKEN.
const MARK_FILLED: u64 = 0b11; // OR to go from TAKEN -> FILLED.
const MARK_READING: u64 = 0b01; // XOR to go from FILLED -> READING.
const MARK_EMPTIED: u64 = 0b11; // ! AND to go from FILLED or READING -> EMPTY.

/// Returns `true` if `slot` in `status` is empty.
#[inline(always)]
fn is_available(status: u64, slot: usize) -> bool {
    has_status(status, slot, EMPTY)
}

/// Returns `true` if `slot` in `status` is filled.
#[inline(always)]
fn is_filled(status: u64, slot: usize) -> bool {
    has_status(status, slot, FILLED)
}

/// Returns `true` if `slot` (in `status`) equals the `expected` status.
#[inline(always)]
fn has_status(status: u64, slot: usize, expected: u64) -> bool {
    slot_status(status, slot) == expected
}

/// Returns the `STATUS_BITS` for `slot` in `status`.
#[inline(always)]
fn slot_status(status: u64, slot: usize) -> u64 {
    debug_assert!(slot <= MAX_CAP);
    (status >> (STATUS_BITS * slot as u64)) & STATUS_MASK
}

/// Creates a mask to transition `slot` using `transition`. `transition` must be
/// one of the `MARK_*` constants.
#[inline(always)]
fn mark_slot(slot: usize, transition: u64) -> u64 {
    debug_assert!(slot <= MAX_CAP);
    transition << (STATUS_BITS * slot as u64)
}

/// Returns a string name for the `slot_status`.
fn dbg_status(slot_status: u64) -> &'static str {
    match slot_status {
        EMPTY => "EMPTY",
        TAKEN => "TAKEN",
        FILLED => "FILLED",
        READING => "READING",
        _ => "INVALID",
    }
}

// Bits to mark the position of the receiver.
const MARK_NEXT_POS: u64 = 1 << (STATUS_BITS * MAX_CAP as u64); // Add to increase position by 1.

/// Returns the position of the receiver. Will be in 0..[`MAX_CAP`] range.
#[inline(always)]
fn receiver_pos(status: u64, capacity: usize) -> usize {
    (status >> (STATUS_BITS * MAX_CAP as u64)) as usize % capacity
}

/// Sending side of the channel.
pub struct Sender<T> {
    channel: NonNull<Channel<T>>,
}

/// Error returned in case sending a value across the channel fails. See
/// [`Sender::try_send`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SendError<T> {
    /// Channel is full.
    Full(T),
    /// [`Receiver`] and [`Manager`] are disconnected.
    Disconnected(T),
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Full(..) => f.pad("channel is full"),
            SendError::Disconnected(..) => f.pad("receiver is disconnected"),
        }
    }
}

impl<T: fmt::Debug> Error for SendError<T> {}

impl<T> Sender<T> {
    /// Attempts to send the `value` into the channel.
    pub fn try_send(&self, value: T) -> Result<(), SendError<T>> {
        try_send(self.channel(), value)
    }

    /// Returns a future that sends a value into the channel, waiting if the
    /// channel is full.
    ///
    /// If the returned [`Future`] returns an error it means the [`Receiver`]
    /// and [`Manager`] are [disconnected] and no more values will be read from
    /// the channel. This is the same error as [`SendError::Disconnected`].
    /// [`SendError::Full`] will never be returned, the `Future` will return
    /// [`Poll::Pending`] instead.
    ///
    /// [disconnected]: Sender::is_connected
    pub fn send<'s>(&'s self, value: T) -> SendValue<'s, T> {
        SendValue {
            channel: self.channel(),
            value: Some(value),
            waker_node: UnsafeCell::new(None),
            _unpin: PhantomPinned,
        }
    }

    /// Returns the capacity of the channel.
    pub fn capacity(&self) -> usize {
        self.channel().slots.len()
    }

    /// Returns `true` if the [`Receiver`] and or the [`Manager`] are connected.
    ///
    /// # Notes
    ///
    /// Unlike [`Receiver::is_connected`] this method takes the [`Manager`] into
    /// account. This is done to support the use case in which an actor is
    /// restarted and a new receiver is created for it.
    pub fn is_connected(&self) -> bool {
        is_sender_connected(self.channel())
    }

    /// Returns `true` if the [`Manager`] is connected.
    pub fn has_manager(&self) -> bool {
        // Relaxed is fine here since there is always a bit of a race condition
        // when using this method (and then doing something based on it).
        self.channel().ref_count.load(Ordering::Relaxed) & MANAGER_ALIVE != 0
    }

    /// Returns `true` if senders send into the same channel.
    pub fn same_channel(&self, other: &Sender<T>) -> bool {
        self.channel == other.channel
    }

    /// Returns `true` if this sender sends to the `receiver`.
    pub fn sends_to(&self, receiver: &Receiver<T>) -> bool {
        self.channel == receiver.channel
    }

    /// Returns `true` if this is the only sender alive.
    fn only_sender(&self) -> bool {
        // Relaxed is fine here since there is always a bit of a race condition
        // when using this method (and then doing something based on it).
        self.channel().ref_count.load(Ordering::Relaxed) & !(RECEIVER_ALIVE | MANAGER_ALIVE) == 1
    }

    /// Returns the id of this sender.
    pub fn id(&self) -> Id {
        Id(self.channel.as_ptr() as *const () as usize)
    }

    fn channel(&self) -> &Channel<T> {
        unsafe { self.channel.as_ref() }
    }
}

/// See [`Sender::try_send`].
fn try_send<T>(channel: &Channel<T>, value: T) -> Result<(), SendError<T>> {
    if !is_sender_connected(channel) {
        return Err(SendError::Disconnected(value));
    }

    // NOTE: relaxed ordering here is ok because we acquire unique
    // permission to write to the slot later before writing to it. Something
    // we have to do no matter the ordering.
    let mut status: u64 = channel.status.load(Ordering::Relaxed);
    let cap = channel.slots.len();
    let start = receiver_pos(status, cap);
    for slot in (0..cap).cycle().skip(start).take(cap) {
        if !is_available(status, slot) {
            continue;
        }

        // In our local status the slot is available, however another sender
        // could have taken it between the time we read the status and the
        // time we got here. So we write our `TAKEN` status and check if in
        // the *previous* (up-to-date) status (returned by `fetch_or`) the
        // slot was still available. If it was it means we have acquired the
        // slot, otherwise another sender beat us to it.
        //
        // NOTE: The OR operation here is safe: if another sender already
        // wrote TAKEN (01) or FILLED (11) we're not overwriting anything.
        // If a reader wrote READING (10) we won't use the slot and the
        // reader will overwrite it with EMPTY later. If we overwrite EMPTY
        // (00) we can reuse the slot safely, but the message will be in a
        // different order.
        status = channel
            .status
            .fetch_or(mark_slot(slot, MARK_TAKEN), Ordering::AcqRel);
        if !is_available(status, slot) {
            // Another thread beat us to taking the slot.
            continue;
        }

        // Safety: we've acquired the slot above so we're ensured unique
        // access to the slot.
        unsafe {
            let _ = (&mut *channel.slots[slot].get()).write(value);
        }

        // Now we've writing to the slot we can mark it slot as filled.
        let old_status = channel
            .status
            .fetch_or(mark_slot(slot, MARK_FILLED), Ordering::AcqRel);
        // Debug assertion to check the slot was in the TAKEN status.
        debug_assert!(has_status(old_status, slot, TAKEN));

        // If the receiver is waiting for this lot we wake it.
        if receiver_pos(old_status, cap) == slot {
            channel.wake_receiver();
        }

        return Ok(());
    }

    Err(SendError::Full(value))
}

/// See [`Sender::is_connected`].
fn is_sender_connected<T>(channel: &Channel<T>) -> bool {
    // Relaxed is fine here since there is always a bit of a race condition
    // when using this method (and then doing something based on it).
    channel.ref_count.load(Ordering::Relaxed) & (RECEIVER_ALIVE | MANAGER_ALIVE) != 0
}

/// # Safety
///
/// Only `2 ^ 30` (a billion) `Sender`s may be alive concurrently, more then
/// enough for all practical use cases.
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        // For the reasoning behind this relaxed ordering see `Arc::clone`.
        let _ = self.channel().ref_count.fetch_add(1, Ordering::Relaxed);
        Sender {
            channel: self.channel,
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("channel", &self.channel())
            .finish()
    }
}

// Safety: if the value can be send across thread than so can the channel.
unsafe impl<T: Send> Send for Sender<T> {}

unsafe impl<T> Sync for Sender<T> {}

impl<T> Unpin for Sender<T> {}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // If we're the last sender being dropped wake the receiver.
        //
        // NOTE: there is a race condition between the `wake` and `fetch_sub`
        // below: in between those calls the receiver could run (after we
        // woke it) and see we're still connected and sleep (return
        // `Poll::Pending`) again. This can't be fixed.
        // The alternative would be to call `fetch_sub` on the `ref_count`
        // before waking, ensuring the count is valid once the `Sender` runs,
        // however that opens another race condition in which the `Sender` can
        // be dropped and deallocate the `Channel` memory, after which we'll
        // access it to wake the `Sender`. Basically we're choosing the least
        // worse of the two race conditions in which in the worst case scenario
        // is that the `Sender` loses a wake-up notification, but it doesn't
        // have any memory unsafety.
        if self.only_sender() {
            self.channel().wake_receiver();
        }

        // If the previous value was `1` it means that the receiver was dropped
        // as well as all other senders, the receiver and the manager, so we
        // need to do the deallocating.
        //
        // Safety: for the reasoning behind this ordering see `Arc::drop`.
        if self.channel().ref_count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }

        // For the reasoning behind this ordering see `Arc::drop`.
        fence(Ordering::Acquire);

        // Drop the memory.
        unsafe { drop(Box::from_raw(self.channel.as_ptr())) }
    }
}

/// [`Future`] implementation behind [`Sender::send`].
///
/// # Safety
///
/// It is not safe to leak this `SendValue` (by using [`mem::forget`]). Always
/// make sure the destructor is run, by calling [`drop`], or letting it go out
/// of scope.
///
/// [`mem::forget`]: std::mem::forget
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SendValue<'s, T> {
    channel: &'s Channel<T>,
    value: Option<T>,
    /// This future's `task::Waker`, maybe registered in `Channel`s list.
    ///
    /// This is wrapped in an `UnsafeCell` because we don't always have unique
    /// access to it. If the inside of the `UnsafeCell` is `None` it means the
    /// waker was not yet registered (added to the `Channel`s linked list). If
    /// this is the case we can safely modify it (as we have unique access).
    /// However once it is `Some` it means it is added to the `Channel`s linked
    /// list and thus we **don't** have unique access anymore.
    ///
    /// Safety: only `register_waker` may read/write to this field.
    waker_node: UnsafeCell<Option<WakerList>>,
    /// Once `waker_node` is added to the `Channel`s linked list it must be
    /// pinned and can't move.
    _unpin: PhantomPinned,
}

impl<'s, T> SendValue<'s, T> {
    /// Register `new_waker` with list in `Channel`.
    fn register_waker(self: Pin<&Self>, new_waker: &task::Waker) {
        if let Some(waker_node) = unsafe { (*self.waker_node.get()).as_ref() } {
            // We've already registered, check if we already used the same
            // waker.
            let mut waker = waker_node.waker.lock();
            match &mut *waker {
                // Already registered the same waker.
                Some(waker) if waker.will_wake(new_waker) => {}
                _ => {
                    *waker = Some(new_waker.clone());
                }
            }
        } else {
            // Haven't yet added ourselves to the list in `Channel`, so we'll do
            // that now.
            // Safety: because we haven't added ourselves to the list in
            // `Channel` it means we have unique access and thus making this
            // mutable reference safe.
            let waker_node = unsafe { &mut *self.waker_node.get() };
            *waker_node = Some(WakerList {
                waker: Mutex::new(Some(new_waker.clone())),
                next: AtomicPtr::new(ptr::null_mut()),
            });
            // Safety: just initialised it above, so `unwrap` is safe.
            let waker_ref = waker_node.as_mut().unwrap();
            // Then add our node the `Channel`s list.
            self.channel.add_waker(waker_ref);
        }
    }
}

impl<'s, T> Future for SendValue<'s, T> {
    type Output = Result<(), T>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut task::Context) -> Poll<Self::Output> {
        // Safety: only `waker_node` is pinned, which is only used by
        // `register_waker`.
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        let value = this
            .value
            .take()
            .expect("SendValue polled after completion");

        // First we try to send the value, if this succeeds we don't have to
        // allocate in the waker list.
        match try_send(this.channel, value) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(SendError::Full(value)) => {
                // The channel is full, we'll register ourselves as wanting to
                // be woken once a slot opens up.
                // Safety: the caller ensures we're pinned.
                unsafe { Pin::new_unchecked(&*this) }.register_waker(ctx.waker());

                // But it could be the case that the received received a value
                // in the time after we tried to send the value and before we
                // added the our waker to list. So we try to send a value again
                // to ensure we don't awoken and the channel has a slot
                // available.
                match try_send(this.channel, value) {
                    Ok(()) => Poll::Ready(Ok(())),
                    Err(SendError::Full(value)) => {
                        // Channel is still full, we'll have to wait.
                        this.value = Some(value);
                        Poll::Pending
                    }
                    Err(SendError::Disconnected(value)) => Poll::Ready(Err(value)),
                }
            }
            Err(SendError::Disconnected(value)) => Poll::Ready(Err(value)),
        }
    }
}

unsafe impl<'s, T> Sync for SendValue<'s, T> {}

impl<'s, T> Drop for SendValue<'s, T> {
    fn drop(&mut self) {
        if let Some(waker_node) = unsafe { (*self.waker_node.get()).as_ref() } {
            // First remove the waker from `waker_node`, replacing it with
            // `None`.
            let mut waker = waker_node.waker.lock();
            // Remove our `task::Waker`.
            drop(waker.take());
            // Release the lock.
            drop(waker);

            // Remove our waker from the list in `Channel`.
            self.channel.remove_waker(waker_node);
        }
    }
}

/// Receiving side of the channel.
pub struct Receiver<T> {
    channel: NonNull<Channel<T>>,
}

/// Error returned in case receiving a value from the channel fails. See
/// [`Receiver::try_recv`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RecvError {
    /// Channel is empty.
    Empty,
    /// All [`Sender`]s (but not necessarily the [`Manager`]) are disconnected
    /// and the channel is empty, see [`Receiver::is_connected`].
    Disconnected,
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvError::Empty => f.pad("channel is empty"),
            RecvError::Disconnected => f.pad("all senders are disconnected"),
        }
    }
}

impl Error for RecvError {}

impl<T> Receiver<T> {
    /// Attempts to receive a value from this channel.
    pub fn try_recv(&mut self) -> Result<T, RecvError> {
        try_recv(self.channel())
    }

    /// Returns a future that receives a value from the channel, waiting if the
    /// channel is empty.
    ///
    /// If the returned [`Future`] returns `None` it means all [`Sender`]s are
    /// [disconnected]. This is the same error as [`RecvError::Disconnected`].
    /// [`RecvError::Empty`] will never be returned, the `Future` will return
    /// [`Poll::Pending`] instead.
    ///
    /// [disconnected]: Receiver::is_connected
    pub fn recv<'r>(&'r mut self) -> RecvValue<'r, T> {
        RecvValue {
            channel: self.channel(),
        }
    }

    /// Create a new [`Sender`] that sends to this channel.
    ///
    /// # Safety
    ///
    /// The same restrictions apply to this function as they do to
    /// [`Sender::clone`].
    ///
    /// [`Sender::clone`]: struct.Sender.html#impl-Clone
    pub fn new_sender(&self) -> Sender<T> {
        // For the reasoning behind this relaxed ordering see `Arc::clone`.
        let _ = self.channel().ref_count.fetch_add(1, Ordering::Relaxed);
        Sender {
            channel: self.channel,
        }
    }

    /// Returns the capacity of the channel.
    pub fn capacity(&self) -> usize {
        self.channel().slots.len()
    }

    /// Returns `false` if all [`Sender`]s are disconnected.
    ///
    /// # Notes
    ///
    /// Unlike [`Sender::is_connected`] this method doesn't take the [`Manager`]
    /// into account. This means that this method can return `false` and later
    /// `true` (if the `Manager` created another `Sender`), which might be
    /// unexpected.
    pub fn is_connected(&self) -> bool {
        is_receiver_connected(self.channel())
    }

    /// Returns `true` if the [`Manager`] is connected.
    pub fn has_manager(&self) -> bool {
        // Relaxed is fine here since there is always a bit of a race condition
        // when using this method (and then doing something based on it).
        has_manager(self.channel().ref_count.load(Ordering::Relaxed))
    }

    /// Set the receiver's waker to `waker`, if they are different. Returns
    /// `true` if the waker is changed, `false` otherwise.
    ///
    /// This is useful if you can't call [`Receiver::recv`] but still want a
    /// wake-up notification once messages are added to the inbox.
    pub fn register_waker(&mut self, waker: &task::Waker) -> bool {
        self.channel().receiver_waker.register(waker)
    }

    /// Returns the id of this receiver.
    pub fn id(&self) -> Id {
        Id(self.channel.as_ptr() as *const () as usize)
    }

    fn channel(&self) -> &Channel<T> {
        unsafe { self.channel.as_ref() }
    }
}

/// See [`Receiver::try_recv`].
fn try_recv<T>(channel: &Channel<T>) -> Result<T, RecvError> {
    // We check if we are connected **before** checking for messages. This
    // is important because there is a time between 1) the checking of the
    // messages in the channel and 2) checking if we're connected (if we
    // would do it in the last `if` statement of this method) in which the
    // sender could send a message and be dropped.
    // In this case, if we would check if we're connected after checking for
    // messages, we would incorrectly return `RecvError::Disconnected` (all
    // senders are dropped after all), however we would miss the last
    // message send.
    // Checking before hand causes us to return `RecvError::Empty`, which
    // technically isn't correct either but it will cause the user to check
    // again later. In `RecvValue` this is solved by calling `try_recv`
    // after registering the task waker, ensuring no wake-up events are
    // missed.
    let is_connected = is_receiver_connected(channel);

    // Since we substract from the `status` this will overflow at some
    // point. But `fetch_add` wraps-around on overflow, so the position will
    // "reset" itself to 0. The status bits will not be touched (even on
    // wrap-around).
    let mut status = channel.status.fetch_add(MARK_NEXT_POS, Ordering::AcqRel);
    let cap = channel.slots.len();
    let start = receiver_pos(status, cap);
    for slot in (0..cap).cycle().skip(start).take(cap) {
        if !is_filled(status, slot) {
            continue;
        }

        // Mark the slot as being read.
        status = channel
            .status
            .fetch_xor(mark_slot(slot, MARK_READING), Ordering::AcqRel);
        if !is_filled(status, slot) {
            // Slot isn't available after all.
            continue;
        }

        // Safety: we've acquired unique access the slot above and we're
        // ensured the slot is filled.
        let value = unsafe { (&*channel.slots[slot].get()).assume_init_read() };

        // Mark the slot as empty.
        let old_status = channel
            .status
            .fetch_and(!mark_slot(slot, MARK_EMPTIED), Ordering::AcqRel);

        // Debug assertion to check the slot was in the READING or FILLED
        // status. The slot can be in the FILLED status if the sender tried
        // to mark this slot as TAKEN (01) after we marked it as READING
        // (10) (01 | 10 = 11 (FILLED)).
        debug_assert!(
            has_status(old_status, slot, READING) || has_status(old_status, slot, FILLED)
        );

        if let Some(waker) = channel.next_waker() {
            waker.wake()
        }

        return Ok(value);
    }

    if !is_connected {
        Err(RecvError::Disconnected)
    } else {
        Err(RecvError::Empty)
    }
}

/// See [`Receiver::is_connected`].
fn is_receiver_connected<T>(channel: &Channel<T>) -> bool {
    // Relaxed is fine here since there is always a bit of a race condition
    // when using this method (and then doing something based on it).
    channel.ref_count.load(Ordering::Relaxed) & !(RECEIVER_ALIVE | MANAGER_ALIVE) > 0
}

impl<T: fmt::Debug> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("channel", &self.channel())
            .finish()
    }
}

// Safety: if the value can be send across thread than so can the channel.
unsafe impl<T: Send> Send for Receiver<T> {}

unsafe impl<T> Sync for Receiver<T> {}

impl<T> Unpin for Receiver<T> {}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // See the `if` statement why we do this.
        let sender = self.new_sender();

        // First mark the receiver as dropped.
        let channel = self.channel();
        let status = channel
            .ref_count
            .fetch_and(!RECEIVER_ALIVE, Ordering::Release);

        if !has_manager(status) {
            // If the channel doesn't have a manager we empty the channel.
            //
            // We do this to support the use case were the channel holds a
            // `oneshot::Sender` and the receiver of the oneshot channel is
            // holding a `Sender` to this channel.
            // Effectively this creates a cyclic drop dependency: `Sender` ->
            // `Channel` -> `oneshot::Sender` which blocks
            // `oneshot::Receiver::recv`.
            // If the actor holding a `Sender` calls `oneshot::Receiver::recv`
            // it will wait for a response or until the `oneshot::Sender` is
            // dropped, while the actor is holding a `Sender` to this channel.
            // However if this `Receiver` is dropped it won't drop the
            // `oneshot::Sender` without the emptying below. This causes
            // `oneshot::Receiver::recv` to wait forever, while holding a
            // `Sender`.
            //
            // NOTE: we use `self` here, this is only safe because we created a
            // new `sender` above ensure the channel is not deallocated.
            while let Ok(msg) = self.try_recv() {
                drop(msg);
            }
        }

        // Now we can deallocate the channel safety.
        drop(sender);
    }
}

/// [`Future`] implementation behind [`Receiver::recv`].
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct RecvValue<'r, T> {
    channel: &'r Channel<T>,
}

impl<'r, T> Future for RecvValue<'r, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, ctx: &mut task::Context) -> Poll<Self::Output> {
        match try_recv(self.channel) {
            Ok(value) => Poll::Ready(Some(value)),
            Err(RecvError::Empty) => {
                // The channel is empty, we'll set the waker.
                if !self.channel.receiver_waker.register(ctx.waker()) {
                    // Waker already set.
                    return Poll::Pending;
                }

                // But it could be the case that a sender send a value in the
                // time between we last checked and we actually marked ourselves
                // as needing a wake up, so we need to check again.
                match try_recv(self.channel) {
                    Ok(value) => Poll::Ready(Some(value)),
                    // The `Sender` will wake us when a new message is send.
                    Err(RecvError::Empty) => Poll::Pending,
                    Err(RecvError::Disconnected) => Poll::Ready(None),
                }
            }
            Err(RecvError::Disconnected) => Poll::Ready(None),
        }
    }
}

impl<'r, T> Unpin for RecvValue<'r, T> {}

/// Channel internals shared between zero or more [`Sender`]s, zero or one
/// [`Receiver`] and zero or one [`Manager`].
struct Channel<T> {
    inner: Inner,
    /// The slots in the channel, see `status` for what slots are used/unused.
    slots: [UnsafeCell<MaybeUninit<T>>],
}

/// Inner data of [`Channel`].
///
/// This is only in a different struct to calculate the `Layout` of `Channel`,
/// see [`Channel::new`].
struct Inner {
    /// The number of senders alive. If the [`RECEIVER_ALIVE`] bit is set the
    /// [`Receiver`] is alive. If the [`MANAGER_ALIVE`] bit is the [`Manager`]
    /// is alive.
    ref_count: AtomicUsize,
    /// This is a linked list of `task::Waker`.
    ///
    /// If this is not null it must point to valid memory.
    sender_waker_head: AtomicPtr<WakerList>,
    receiver_waker: WakerRegistration,
    /// Status of the slots.
    ///
    /// This contains the status of the slots. Each status consists of
    /// [`STATUS_BITS`] bits to describe if the slot is taken or not.
    ///
    /// The first `STATUS_BITS * MAX_CAP` bits are the statuses for the `slots`
    /// field. The remaining bits are used by the `Sender` to indicate its
    /// current reading position (modulo [`MAX_CAP`]).
    status: AtomicU64,
}

// Safety: if the value can be send across thread than so can the channel.
unsafe impl<T: Send> Send for Channel<T> {}

unsafe impl<T> Sync for Channel<T> {}

/// Atomic linked list of `task::Waker`s.
#[derive(Debug)]
struct WakerList {
    waker: Mutex<Option<task::Waker>>,
    /// If this is null it must point to valid memory.
    next: AtomicPtr<Self>,
}

impl<T> Channel<T> {
    /// Allocates a new `Channel` on the heap.
    ///
    /// `capacity` must small enough to ensure each slot has 2 bits for the
    /// status, while ensuring that the remaining bits can store `capacity` (in
    /// binary) to keep track of the reading position. This means following must
    /// hold true where $N is capacity: `2 ^ (64 - ($N * 2)) >= $N`. The maximum
    /// is 29.
    fn new(capacity: usize) -> NonNull<Channel<T>> {
        assert!(capacity != 0, "capacity can't be zero");
        assert!(capacity <= MAX_CAP, "capacity too large");

        // Allocate some raw bytes.
        // Safety: returns an error on arithmetic overflow, but it should be OK
        // with a capacity <= MAX_CAP.
        let (layout, _) = Layout::array::<UnsafeCell<MaybeUninit<T>>>(capacity)
            .and_then(|slots_layout| Layout::new::<Inner>().extend(slots_layout))
            .unwrap();
        // Safety: we check if the allocation is successful.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        let ptr = ptr::slice_from_raw_parts_mut(ptr as *mut T, capacity) as *mut Channel<T>;

        // Initialise all fields (that need it).
        unsafe {
            ptr::addr_of_mut!((*ptr).inner.ref_count).write(AtomicUsize::new(RECEIVER_ALIVE | 1));
            ptr::addr_of_mut!((*ptr).inner.sender_waker_head)
                .write(AtomicPtr::new(ptr::null_mut()));
            ptr::addr_of_mut!((*ptr).inner.receiver_waker).write(WakerRegistration::new());
            ptr::addr_of_mut!((*ptr).inner.status).write(AtomicU64::new(0));
            // TODO: does `slots` need to be initialised with
            // `UnsafeCell::new(MaybeUninit::uninit())`?
        }

        // Safety: checked if the pointer is null above.
        unsafe { NonNull::new_unchecked(ptr) }
    }

    /// Returns the next `task::Waker` to wake, if any.
    fn next_waker(&self) -> Option<task::Waker> {
        loop {
            let head_ptr = self.sender_waker_head.load(Ordering::Relaxed);
            if head_ptr.is_null() {
                return None;
            }

            // Safety: checked for null above.
            let next_node: &WakerList = unsafe { &*head_ptr };
            let next_ptr: *mut WakerList = next_node.next.load(Ordering::Relaxed);
            let res = self.sender_waker_head.compare_exchange(
                head_ptr,
                next_ptr,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            match res {
                Ok(..) => {
                    // Safety: checked for null above.
                    let waker = next_node.waker.lock().take();

                    // It could be that a `Receiver` added its waker in between
                    // we loaded the `next_ptr` and stored it in
                    // `sender_waker_head`. If this is the case add the waker
                    // back into the list.
                    let updated_next_ptr = next_node.next.swap(ptr::null_mut(), Ordering::AcqRel);
                    if updated_next_ptr != next_ptr && !updated_next_ptr.is_null() {
                        self.add_waker(updated_next_ptr);
                    }

                    if waker.is_none() {
                        // If the `SendValue` `Future` is in the process of
                        // being dropped this will be `None`, so we need to wake
                        // the next sender.
                        continue;
                    }

                    return waker;
                }
                // Failed, so let's try again.
                Err(..) => continue,
            }
        }
    }

    /// Adds `node` to the list of wakers to wake.
    ///
    /// # Safety
    ///
    /// `node` must be at a stable (pinned) address and must remain valid until
    /// its removed from `Channel`, or `Channel` is dropped.
    fn add_waker(&self, node: *mut WakerList) {
        let mut ptr: &AtomicPtr<WakerList> = &self.sender_waker_head;
        loop {
            // Safety: Relaxed is fine because we use `compare_exchange` below
            // as the deciding point.
            let next_ptr = ptr.load(Ordering::Relaxed);
            if next_ptr.is_null() {
                // No next link, try to put our node in that spot.
                let res = ptr.compare_exchange(
                    ptr::null_mut(),
                    node,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
                match res {
                    Ok(..) => return,
                    Err(next_ptr) => {
                        // Failed, another node already took the spot, so we
                        // need to try again.
                        ptr = unsafe { &(*next_ptr).next };
                    }
                }
            } else {
                // Already taken, follow the next link.
                ptr = unsafe { &(*next_ptr).next };
            }
        }
    }

    /// Remove `node` from receiver waker list.
    fn remove_waker(&self, node: *const WakerList) {
        if node.is_null() {
            return;
        }

        let node_ptr = node as *mut _;
        let node: &WakerList = unsafe { &*node };

        'main: loop {
            let head_ptr = self.sender_waker_head.load(Ordering::Relaxed);
            if head_ptr.is_null() {
                // List is empty, then we're done quickly.
                return;
            } else if head_ptr == node_ptr {
                // Next node is the node we're looking for. Link the next node
                // (after `node`) to the previous node (`self.sender_waker_head`).
                let next_ptr = node.next.load(Ordering::Relaxed);
                let res = self.sender_waker_head.compare_exchange(
                    node_ptr,
                    next_ptr,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
                match res {
                    // Successfully removed ourself.
                    Ok(..) => return,
                    // Failed to remove ourself, try again.
                    Err(_) => continue,
                }
            }

            let mut parent: &WakerList = unsafe { &*head_ptr };
            loop {
                let link_ptr = parent.next.load(Ordering::Relaxed);
                if link_ptr.is_null() {
                    // End of the list, so the node isn't in it.
                    return;
                } else if link_ptr == node_ptr {
                    // Next node is the node we're looking for. Link the next
                    // node (after `node`) to the previous node (`parent`).
                    let next_ptr = node.next.load(Ordering::Relaxed);
                    let res = parent.next.compare_exchange(
                        node_ptr,
                        next_ptr,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                    match res {
                        // Successfully removed ourself.
                        Ok(..) => return,
                        // Failed to remove ourself, try again.
                        Err(_) => continue 'main,
                    }
                } else {
                    // Not in this spot, move to the next node.
                    parent = unsafe { &*link_ptr };
                }
            }
        }
    }

    /// Wake the `Receiver`.
    fn wake_receiver(&self) {
        self.receiver_waker.wake()
    }
}

// NOTE: this is here so we don't have to type `self.channel().inner`
// everywhere.
impl<T> Deref for Channel<T> {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> fmt::Debug for Channel<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = self.status.load(Ordering::Relaxed);
        let recv_pos = receiver_pos(status, self.slots.len());
        let ref_count = self.ref_count.load(Ordering::Relaxed);
        let sender_count = ref_count & (!(RECEIVER_ALIVE | MANAGER_ALIVE));
        let mut slots = [""; MAX_CAP];
        for n in 0..self.slots.len() {
            slots[n] = dbg_status(slot_status(status, n));
        }
        let slots = &slots[..self.slots.len()];
        f.debug_struct("Channel")
            .field("senders_alive", &sender_count)
            .field("receiver_alive", &(ref_count & RECEIVER_ALIVE != 0))
            .field("manager_alive", &(ref_count & MANAGER_ALIVE != 0))
            .field("receiver_position", &recv_pos)
            .field("slots", &slots)
            .finish()
    }
}

impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        // Safety: we have unique access, per the mutable reference, so relaxed
        // is fine.
        let status: u64 = self.status.load(Ordering::Relaxed);
        for slot in 0..self.slots.len() {
            if is_filled(status, slot) {
                // Safety: we have unique access to the slot and it's properly
                // aligned.
                unsafe { ptr::drop_in_place((&mut *self.slots[slot].get()).as_mut_ptr()) };
            }
        }
    }
}

/// Manager of a channel.
///
/// A channel manager can be used to create [`Sender`]s and [`Receiver`]s for a
/// channel, without having access to either. Its made for the following use
/// case: restarting an actor which takes ownership of the `Receiver` and
/// crashes, and to restart the actor we need another `Receiver`. Using the
/// manager a new `Receiver` can be created, ensuring only a single `Receiver`
/// is alive at any given time.
pub struct Manager<T> {
    channel: NonNull<Channel<T>>,
}

/// Error returned by [`Manager::new_receiver`] if a receiver is already
/// connected.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ReceiverConnected;

impl fmt::Display for ReceiverConnected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("receiver already connected")
    }
}

impl Error for ReceiverConnected {}

impl<T> Manager<T> {
    /// Create a small bounded channel with a `Manager`.
    ///
    /// Same as [`new_small`] but with a `Manager`.
    pub fn new_small_channel() -> (Manager<T>, Sender<T>, Receiver<T>) {
        Manager::new_channel(SMALL_CAP)
    }

    /// Create a bounded channel with a `Manager`.
    ///
    /// Same as [`new`] but with a `Manager`.
    pub fn new_channel(capacity: usize) -> (Manager<T>, Sender<T>, Receiver<T>) {
        let (sender, receiver) = new(capacity);
        let old_count = sender
            .channel()
            .ref_count
            .fetch_or(MANAGER_ALIVE, Ordering::Relaxed);
        debug_assert!(old_count & MANAGER_ALIVE == 0);
        let manager = Manager {
            channel: sender.channel,
        };
        (manager, sender, receiver)
    }

    /// Create a new [`Sender`].
    ///
    /// # Safety
    ///
    /// See the [safety nodes] on `Sender`'s [`Clone`] implemenation, the same
    /// conditions apply here.
    ///
    /// [safety nodes]: struct.Sender.html#impl-Clone
    pub fn new_sender(&self) -> Sender<T> {
        // For the reasoning behind this relaxed ordering see `Arc::clone`.
        let _ = self.channel().ref_count.fetch_add(1, Ordering::Relaxed);
        Sender {
            channel: self.channel,
        }
    }

    /// Attempt to create a new [`Receiver`].
    ///
    /// This will fail if there already is a receiver.
    pub fn new_receiver(&self) -> Result<Receiver<T>, ReceiverConnected> {
        let old_count = self
            .channel()
            .ref_count
            .fetch_or(RECEIVER_ALIVE, Ordering::AcqRel);
        if old_count & RECEIVER_ALIVE == 0 {
            // No receiver was connected so its safe to create one.
            Ok(Receiver {
                channel: self.channel,
            })
        } else {
            Err(ReceiverConnected)
        }
    }

    fn channel(&self) -> &Channel<T> {
        unsafe { self.channel.as_ref() }
    }
}

impl<T> fmt::Debug for Manager<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Manager")
            .field("channel", &self.channel())
            .finish()
    }
}

// Safety: if the value can be send across thread than so can the channel.
unsafe impl<T: Send> Send for Manager<T> {}

unsafe impl<T> Sync for Manager<T> {}

impl<T> Unpin for Manager<T> {}

impl<T> Drop for Manager<T> {
    #[rustfmt::skip] // For the if statement, its complicated enough.
    fn drop(&mut self) {
        // If the previous value was `MANAGER_ALIVE` it means that all senders
        // and receivers were dropped, so we need to do the deallocating.
        //
        // Safety: for the reasoning behind this ordering see `Arc::drop`.
        if self.channel().ref_count.fetch_and(!MANAGER_ALIVE, Ordering::Release) != MANAGER_ALIVE {
            return;
        }

        // For the reasoning behind this ordering see `Arc::drop`.
        fence(Ordering::Acquire);

        // Drop the memory.
        unsafe { drop(Box::from_raw(self.channel.as_ptr())) }
    }
}

/// Identifier of a channel.
///
/// This type can be created by calling [`Sender::id`] or [`Receiver::id`] and
/// be used to identify channels. It only use case is to compare two ids with
/// one another, if two id are the same the sender(s) and receiver(s) point to
/// the same channel.
///
/// # Notes
///
/// The id is only valid for the lifetime of the channel. Once the channel is
/// dropped all ids of the channel are invalidated and might return incorrect
/// results after.
///
/// The methods [`Sender::same_channel`] and [`Sender::sends_to`] should be
/// preferred over using this type as they are less error-prone.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Id(usize);

//! The core of the ring buffer: one heap buffer, reader/writer positions
//! and the four state flags packed into a single `AtomicUsize`.
//!
//! # Design
//!
//! `RingBuffer` exclusively owns **one** heap-allocated `[T]` buffer. The
//! concrete storage type is generic (`B: DerefMut<Target = [T]>`), so any
//! heap pointer such as `Box<[T]>` works.
//!
//! All shared state lives in [`RingCore`]:
//!
//! * one `AtomicUsize` holding the reader position `rp` (low bits), the
//!   writer position `wp` (next bits) and four state flags (high bits):
//!   `tx_closed`, `rx_closed`, `send_in_flight`, `recv_in_flight`. A single
//!   atomic load observes everything, and every transition is a single
//!   compare-exchange loop (spin-CAS), the same approach as `atomic_sync`:
//!
//!   * `data = (wp - rp) mod cap` — the number of buffered items;
//!   * `free = cap - 1 - data` — the number of free slots.
//!
//! The ring is **full** when `free == 0`, i.e. when the writer position is
//! immediately behind the reader position (`(wp + 1) mod cap == rp`); the
//! ring is **empty** when `wp == rp`. One slot is always left unused (the
//! classic single-gap scheme), which is what makes the full/empty states
//! distinguishable from the two packed positions alone.
//!
//! Because `rp`, `wp` and the flags share one word, each position occupies
//! `(usize::BITS - FLAG_BITS) / 2` bits (e.g. 30 bits on 64-bit targets), so
//! the buffer length is limited to [`MAX_CAPACITY`] items. This matches the
//! vectored-IO requirement: each slice submitted to the kernel fits in the
//! native iovec size field.
//!
//! The readable region `[rp, rp+data)` and the writable region
//! `[wp, wp+free)` may wrap around the end of the buffer; they are exposed as
//! **two** slices (scatter/gather). The runtime side takes them as an iovec
//! pair and submits them to the kernel with a single `readv` / `writev`
//! syscall ([`RingBuffer::take_send_iovecs`],
//! [`RingBuffer::take_recv_iovecs`]).
//!
//! The user side borrows segments through `abs_buff`'s [`SegmMut`] /
//! [`SegmRef`]; the segment's buffer is the ring's own memory (no extra
//! copies), and its reclaim advances the ring position by the amount the
//! segment actually consumed when it drops (per-piece reclaim granularity).
//!
//! Parking/waking uses a single waker slot per side ([`DemandSlot`]). All
//! state transitions are single-atomic, so the ring works from two threads
//! (one producer, one consumer) without any lock and without any async-runtime
//! dependency.

use core::{
    mem::MaybeUninit,
    ops::DerefMut,
    ptr,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

use abs_buff::buffer::{SegmMut, SegmRef};

use super::reclaim_::{NoReclaim, ReaderReclaim, ReclPeekRef, ReclSliceMut, ReclSliceRef, WriterReclaim};

/// Number of high bits reserved for the state flags.
const FLAG_BITS: u32 = 4;

/// Number of bits reserved for each position.
const POS_BITS: u32 = (usize::BITS - FLAG_BITS) / 2;

/// Mask for one position.
const POS_MASK: usize = (1usize << POS_BITS) - 1;

/// The maximum buffer length (also the maximum per-slice length).
pub const MAX_CAPACITY: usize = POS_MASK;

// --- the four state flags (the high `FLAG_BITS` bits of the state word) ----

/// The user writer has closed the tx end.
const TX_CLOSED: usize = 1usize << (usize::BITS - 1);
/// The user reader has closed the rx end.
const RX_CLOSED: usize = 1usize << (usize::BITS - 2);
/// The readable region is reserved by the runtime for a kernel write.
const SEND_IN_FLIGHT: usize = 1usize << (usize::BITS - 3);
/// The writable region is reserved by the runtime for a kernel read.
const RECV_IN_FLIGHT: usize = 1usize << (usize::BITS - 4);

/// Mask of all four flags.
const FLAG_MASK: usize = TX_CLOSED | RX_CLOSED | SEND_IN_FLIGHT | RECV_IN_FLIGHT;

#[inline]
fn unpack(state: usize) -> (usize, usize) {
    (state & POS_MASK, (state >> POS_BITS) & POS_MASK)
}

#[inline]
fn pack(rp: usize, wp: usize) -> usize {
    rp | (wp << POS_BITS)
}

#[inline]
fn has_flag(state: usize, flag: usize) -> bool {
    state & flag != 0
}

/// A waker slot: the ring only ever points at the *single* waiter that is
/// currently parked on a given side. SPSC guarantees at most one waiter per
/// slot.
pub(super) struct DemandSlot(AtomicPtr<Waiter>);

impl DemandSlot {
    pub const fn new() -> Self {
        DemandSlot(AtomicPtr::new(ptr::null_mut()))
    }

    /// Register `w` as the current waiter of this slot. Spins if another
    /// (stale) waiter is still registered; the previous waiter deregisters
    /// itself on completion or drop, so the spin always terminates.
    pub fn register(&self, w: &Waiter) {
        let p = w as *const Waiter as *mut Waiter;
        loop {
            match self.0.compare_exchange_weak(
                ptr::null_mut(),
                p,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(cur) if cur == p => return, // already registered by us
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// Remove `w` from this slot if it is still the registered waiter.
    pub fn deregister(&self, w: &Waiter) {
        let p = w as *const Waiter as *mut Waiter;
        let _ = self
            .0
            .compare_exchange(p, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
    }

    /// Wake the currently registered waiter, if any.
    pub fn signal(&self) {
        let p = self.0.swap(ptr::null_mut(), Ordering::AcqRel);
        if !p.is_null() {
            let w = unsafe { &*p };
            if let Some(waker) = w.waker.as_ref() {
                waker.wake_by_ref();
            }
        }
    }
}

/// The per-waiter state. Lives inside the parking future (or the half, for
/// the poll-based traits) and is referenced by the ring through a raw
/// pointer, so it must not move while registered.
pub(super) struct Waiter {
    pub waker: Option<Waker>,
}

impl Waiter {
    pub const fn new() -> Self {
        Waiter { waker: None }
    }
}

/// Which demand slot a park target registers into.
#[derive(Clone, Copy)]
pub(super) enum ParkSide {
    /// The user writer waits for free space.
    TxUser,
    /// The runtime waits for buffered data to send (writev).
    TxRuntime,
    /// The user reader waits for buffered data.
    RxUser,
    /// The runtime waits for free space to receive (readv).
    RxRuntime,
}

impl ParkSide {
    fn register<B, T>(self, ring: &RingBuffer<B, T>, w: &Waiter)
    where
        B: DerefMut<Target = [T]>,
    {
        match self {
            ParkSide::TxUser => ring.register_tx_user(w),
            ParkSide::TxRuntime => ring.register_tx_runtime(w),
            ParkSide::RxUser => ring.register_rx_user(w),
            ParkSide::RxRuntime => ring.register_rx_runtime(w),
        }
    }

    fn deregister<B, T>(self, ring: &RingBuffer<B, T>, w: &Waiter)
    where
        B: DerefMut<Target = [T]>,
    {
        match self {
            ParkSide::TxUser => ring.deregister_tx_user(w),
            ParkSide::TxRuntime => ring.deregister_tx_runtime(w),
            ParkSide::RxUser => ring.deregister_rx_user(w),
            ParkSide::RxRuntime => ring.deregister_rx_runtime(w),
        }
    }
}

/// Condition checked by a parked future before (re)registering its waker.
/// `arg` is a parameter (e.g. the requested borrow length).
pub(super) type ParkCheck<B, T> = fn(&RingBuffer<B, T>, usize) -> bool;

/// A parking helper: registers a single waker on the ring when the condition
/// does not hold yet. The ring signals the registered waker on every relevant
/// state change; the future re-checks the condition on wake-up.
pub(super) struct Park<B, T>
where
    B: DerefMut<Target = [T]>,
{
    waiter: Waiter,
    registered: bool,
    side: ParkSide,
    check: ParkCheck<B, T>,
}

impl<B, T> Park<B, T>
where
    B: DerefMut<Target = [T]>,
{
    pub const fn new(side: ParkSide, check: ParkCheck<B, T>) -> Self {
        Park {
            waiter: Waiter::new(),
            registered: false,
            side,
            check,
        }
    }

    /// Poll the park: if the condition holds, deregister and return `Ready`;
    /// otherwise register the waker and return `Pending`.
    ///
    /// The condition is re-checked *after* registering to close the
    /// lost-wakeup window: a state change that happens between the first
    /// check and the registration would otherwise signal nobody.
    pub fn poll(&mut self, cx: &mut Context<'_>, ring: &RingBuffer<B, T>, arg: usize) -> Poll<()> {
        if (self.check)(ring, arg) {
            self.deregister(ring);
            return Poll::Ready(());
        }
        self.waiter.waker = Some(cx.waker().clone());
        self.side.register(ring, &self.waiter);
        self.registered = true;
        if (self.check)(ring, arg) {
            self.deregister(ring);
            return Poll::Ready(());
        }
        Poll::Pending
    }

    pub fn deregister(&mut self, ring: &RingBuffer<B, T>) {
        if self.registered {
            self.side.deregister(ring, &self.waiter);
            self.registered = false;
        }
    }
}

/// The shared state of the ring: the packed positions + flags word and the
/// four waker slots.
///
/// Every field is an atomic, so `RingCore` is unconditionally `Send + Sync`.
/// The segment reclaim types hold a `&RingCore` (plus a `usize` copy of the
/// capacity) and therefore satisfy `abs_buff::buffer::TrReclaim`'s
/// `Send + Sync` super-trait without needing the storage element type to be
/// `Send`/`Sync`.
pub(super) struct RingCore {
    /// `rp` in the low `POS_BITS` bits, `wp` in the next `POS_BITS` bits,
    /// and the four flags in the high `FLAG_BITS` bits.
    state: AtomicUsize,
    tx_user_demand: DemandSlot,
    tx_runtime_demand: DemandSlot,
    rx_user_demand: DemandSlot,
    rx_runtime_demand: DemandSlot,
}

impl RingCore {
    const fn new() -> Self {
        RingCore {
            state: AtomicUsize::new(0),
            tx_user_demand: DemandSlot::new(),
            tx_runtime_demand: DemandSlot::new(),
            rx_user_demand: DemandSlot::new(),
            rx_runtime_demand: DemandSlot::new(),
        }
    }

    #[inline]
    fn load_state(&self) -> usize {
        self.state.load(Ordering::Acquire)
    }

    /// Spin compare-exchange loop: replace the state word with `f(state)`,
    /// retrying on contention (the `atomic_sync` way of updating packed
    /// flags).
    fn update_state(&self, f: impl Fn(usize) -> usize) {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            match self.state.compare_exchange_weak(
                state,
                f(state),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(x) => state = x,
            }
        }
    }

    fn set_flag(&self, flag: usize) {
        self.update_state(|s| s | flag);
    }

    /// The ring is shared by both the user pipe ends and the runtime drivers,
    /// so every state change potentially satisfies any parked side. Waking all
    /// four slots is cheap; the parked futures re-check their conditions.
    fn signal_all(&self) {
        self.tx_user_demand.signal();
        self.tx_runtime_demand.signal();
        self.rx_user_demand.signal();
        self.rx_runtime_demand.signal();
    }

    /// Atomically advance the writer position by `amount` (mod `cap`),
    /// preserving the flags.
    pub(super) fn advance_write(&self, cap: usize, amount: usize) {
        self.update_state(|s| {
            let (rp, wp) = unpack(s);
            debug_assert!(rp < cap && wp < cap);
            pack(rp, (wp + amount) % cap) | (s & FLAG_MASK)
        });
        self.signal_all();
    }

    /// Atomically advance the reader position by `amount` (mod `cap`),
    /// preserving the flags.
    pub(super) fn advance_read(&self, cap: usize, amount: usize) {
        self.update_state(|s| {
            let (rp, wp) = unpack(s);
            debug_assert!(rp < cap && wp < cap);
            pack((rp + amount) % cap, wp) | (s & FLAG_MASK)
        });
        self.signal_all();
    }

    fn close_tx(&self) {
        self.set_flag(TX_CLOSED);
        self.signal_all();
    }

    fn close_rx(&self) {
        self.set_flag(RX_CLOSED);
        self.signal_all();
    }

    fn register_tx_user(&self, w: &Waiter) {
        self.tx_user_demand.register(w);
    }
    fn deregister_tx_user(&self, w: &Waiter) {
        self.tx_user_demand.deregister(w);
    }
    fn register_tx_runtime(&self, w: &Waiter) {
        self.tx_runtime_demand.register(w);
    }
    fn deregister_tx_runtime(&self, w: &Waiter) {
        self.tx_runtime_demand.deregister(w);
    }
    fn register_rx_user(&self, w: &Waiter) {
        self.rx_user_demand.register(w);
    }
    fn deregister_rx_user(&self, w: &Waiter) {
        self.rx_user_demand.deregister(w);
    }
    fn register_rx_runtime(&self, w: &Waiter) {
        self.rx_runtime_demand.register(w);
    }
    fn deregister_rx_runtime(&self, w: &Waiter) {
        self.rx_runtime_demand.deregister(w);
    }
}

/// A ring buffer between a user thread and a runtime (kernel) side. See the
/// [module docs](self) for the design.
pub struct RingBuffer<B, T = u8>
where
    B: DerefMut<Target = [T]>,
{
    /// The one heap buffer.
    buffer: B,
    /// The packed positions + flags and the four waker slots.
    core: RingCore,
}

impl<B, T> RingBuffer<B, T>
where
    B: DerefMut<Target = [T]>,
{
    /// Create a ring buffer from one owned heap buffer.
    ///
    /// Returns `Err(len)` if the buffer is too large (longer than
    /// [`MAX_CAPACITY`]).
    pub fn try_new(buffer: B) -> Result<Self, usize> {
        let cap = buffer.len();
        if cap > MAX_CAPACITY {
            return Err(cap);
        }
        Ok(RingBuffer {
            buffer,
            core: RingCore::new(),
        })
    }

    /// The buffer length (the ring's capacity).
    #[inline]
    fn cap(&self) -> usize {
        self.buffer.len()
    }

    /// The buffer length.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap()
    }

    /// A snapshot of the number of buffered items.
    #[inline]
    pub fn data_size(&self) -> usize {
        let (rp, wp) = unpack(self.core.load_state());
        self.data_(rp, wp)
    }

    /// The number of free slots.
    #[inline]
    pub fn free_size(&self) -> usize {
        let (rp, wp) = unpack(self.core.load_state());
        self.free_(rp, wp)
    }

    #[inline]
    fn data_(&self, rp: usize, wp: usize) -> usize {
        (wp + self.cap() - rp) % self.cap()
    }

    /// Free slots; the single-gap scheme always keeps one slot unused.
    #[inline]
    fn free_(&self, rp: usize, wp: usize) -> usize {
        self.cap() - 1 - self.data_(rp, wp)
    }

    /// The reader position.
    pub fn reader_pos(&self) -> usize {
        unpack(self.core.load_state()).0
    }

    /// The writer position.
    pub fn writer_pos(&self) -> usize {
        unpack(self.core.load_state()).1
    }

    pub fn is_tx_closed(&self) -> bool {
        has_flag(self.core.load_state(), TX_CLOSED)
    }

    pub fn is_rx_closed(&self) -> bool {
        has_flag(self.core.load_state(), RX_CLOSED)
    }

    /// Atomically advance the writer position by `amount` (mod `cap`).
    pub fn advance_write(&self, amount: usize) {
        self.core.advance_write(self.cap(), amount);
    }

    /// Atomically advance the reader position by `amount` (mod `cap`).
    pub fn advance_read(&self, amount: usize) {
        self.core.advance_read(self.cap(), amount);
    }

    // ------------------------------------------------------------------
    // user side: write (contiguous borrow)
    // ------------------------------------------------------------------

    /// Borrow up to `length` contiguous writable items starting at `wp`.
    ///
    /// * `TxError::Stuffed` — the ring is full (or the writable region is
    ///   reserved by the runtime for a kernel read).
    /// * `TxError::Closing` — the tx end is closed.
    pub fn try_write_at(&self, length: usize) -> Result<(usize, usize), super::TxError<usize>> {
        use super::TxError;
        let state = self.core.load_state();
        let (rp, wp) = unpack(state);
        let free = self.free_(rp, wp);
        if free == 0 || has_flag(state, RECV_IN_FLIGHT) {
            if has_flag(state, TX_CLOSED) {
                return Err(TxError::Closing);
            }
            return Err(TxError::Stuffed(wp));
        }
        let take = core::cmp::min(length, core::cmp::min(free, self.cap() - wp));
        debug_assert!(take > 0);
        Ok((wp, take))
    }

    // ------------------------------------------------------------------
    // user side: read (contiguous borrow)
    // ------------------------------------------------------------------

    /// Borrow up to `length` contiguous readable items starting at `rp`.
    pub fn try_read_at(&self, length: usize) -> Result<(usize, usize), super::RxError<usize>> {
        use super::RxError;
        let state = self.core.load_state();
        let (rp, wp) = unpack(state);
        let data = self.data_(rp, wp);
        if data == 0 || has_flag(state, SEND_IN_FLIGHT) {
            if has_flag(state, RX_CLOSED) {
                return Err(RxError::Closing);
            }
            return Err(RxError::Drained(rp));
        }
        let take = core::cmp::min(length, core::cmp::min(data, self.cap() - rp));
        debug_assert!(take > 0);
        Ok((rp, take))
    }

    /// Borrow all contiguous readable items starting at `rp` (for peeking).
    pub fn try_peek_at(&self) -> Result<(usize, usize), super::RxError<usize>> {
        use super::RxError;
        let state = self.core.load_state();
        let (rp, wp) = unpack(state);
        let data = self.data_(rp, wp);
        if data == 0 || has_flag(state, SEND_IN_FLIGHT) {
            if has_flag(state, RX_CLOSED) {
                return Err(RxError::Closing);
            }
            return Err(RxError::Drained(rp));
        }
        let take = core::cmp::min(data, self.cap() - rp);
        debug_assert!(take > 0);
        Ok((rp, take))
    }

    // ------------------------------------------------------------------
    // runtime side: kernel submission (scatter / gather)
    // ------------------------------------------------------------------

    /// Take the readable region as an iovec pair for a kernel `writev`.
    ///
    /// The readable region `[rp, rp+data)` is returned as one or two
    /// `&'static [T]` slices (the second is empty when the region does not
    /// wrap). While the region is reserved, the user reader is blocked
    /// (`RxError::Drained`). After the kernel completes, call
    /// [`RingBuffer::put_back_send`] with the number of bytes actually
    /// written, which advances the reader position.
    ///
    /// With `T = u8` the slices can be submitted to compio directly, e.g.
    /// `socket.write_vectored((a, b)).await` — a single syscall.
    pub fn take_send_iovecs(&self) -> Option<(&'static [T], &'static [T])> {
        let mut state = self.core.load_state();
        loop {
            let (rp, wp) = unpack(state);
            let data = self.data_(rp, wp);
            if data == 0 || has_flag(state, SEND_IN_FLIGHT) {
                return None;
            }
            match self.core.state.compare_exchange_weak(
                state,
                state | SEND_IN_FLIGHT,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let (a, b) = self.readable_slices_(rp, data);
                    return Some((a, b));
                }
                Err(x) => state = x,
            }
        }
    }

    /// Return the reserved region after the kernel `writev` completed,
    /// advancing the reader position by `written`.
    pub fn put_back_send(&self, written: usize) {
        let cap = self.cap();
        self.core.update_state(|s| {
            let (rp, wp) = unpack(s);
            debug_assert!(has_flag(s, SEND_IN_FLIGHT));
            let nr = (rp + written) % cap;
            // clear SEND_IN_FLIGHT, keep the other flags
            pack(nr, wp) | (s & FLAG_MASK & !SEND_IN_FLIGHT)
        });
        self.core.signal_all();
    }

    /// Take the writable region as an iovec pair for a kernel `readv`.
    ///
    /// The writable region `[wp, wp+free)` is returned as one or two
    /// `&'static mut [T]` slices. While reserved, the user writer is blocked
    /// (`TxError::Stuffed`). After the kernel completes, call
    /// [`RingBuffer::put_back_recv`] with the number of bytes actually read,
    /// which advances the writer position.
    pub fn take_recv_iovecs(&self) -> Option<(&'static mut [T], &'static mut [T])> {
        let mut state = self.core.load_state();
        loop {
            let (rp, wp) = unpack(state);
            let free = self.free_(rp, wp);
            if free == 0 || has_flag(state, RECV_IN_FLIGHT) {
                return None;
            }
            match self.core.state.compare_exchange_weak(
                state,
                state | RECV_IN_FLIGHT,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let (a, b) = self.writable_slices_(wp, free);
                    return Some((a, b));
                }
                Err(x) => state = x,
            }
        }
    }

    /// Return the reserved region after the kernel `readv` completed,
    /// advancing the writer position by `received`.
    pub fn put_back_recv(&self, received: usize) {
        let cap = self.cap();
        self.core.update_state(|s| {
            let (rp, wp) = unpack(s);
            debug_assert!(has_flag(s, RECV_IN_FLIGHT));
            let nw = (wp + received) % cap;
            // clear RECV_IN_FLIGHT, keep the other flags
            pack(rp, nw) | (s & FLAG_MASK & !RECV_IN_FLIGHT)
        });
        self.core.signal_all();
    }

    /// The readable region `[rp, rp+len)` as up to two slices.
    fn readable_slices_(&self, rp: usize, len: usize) -> (&'static [T], &'static [T]) {
        let base = self.buffer.as_ptr();
        let first = core::cmp::min(len, self.cap() - rp);
        let a = unsafe { core::slice::from_raw_parts(base.add(rp), first) };
        let b = if first < len {
            unsafe { core::slice::from_raw_parts(base, len - first) }
        } else {
            &[]
        };
        (a, b)
    }

    /// The writable region `[wp, wp+len)` as up to two slices.
    fn writable_slices_(&self, wp: usize, len: usize) -> (&'static mut [T], &'static mut [T]) {
        let base = self.buffer.as_ptr().cast_mut();
        let first = core::cmp::min(len, self.cap() - wp);
        let a = unsafe { core::slice::from_raw_parts_mut(base.add(wp), first) };
        let b = if first < len {
            unsafe { core::slice::from_raw_parts_mut(base, len - first) }
        } else {
            &mut []
        };
        (a, b)
    }

    /// A read view over the whole buffer (used by the framework adapters).
    #[inline]
    pub(super) fn buffer_ref(&self) -> &[T] {
        unsafe { core::slice::from_raw_parts(self.buffer.as_ptr(), self.cap()) }
    }

    /// A write view over the whole buffer (used by the framework adapters).
    ///
    /// The ring is shared (`&self`), but the write region is exclusively
    /// owned by the single producer while the positions say so; the view is
    /// handed out through the raw buffer pointer (interior-mutability style).
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(super) fn buffer_uninit(&self) -> &mut [MaybeUninit<T>] {
        unsafe {
            core::slice::from_raw_parts_mut(
                self.buffer.as_ptr().cast_mut().cast::<MaybeUninit<T>>(),
                self.cap(),
            )
        }
    }

    /// A writable borrow exists right now (used by `TrBuffWrite::is_blocked`).
    pub(super) fn has_tx_space(&self) -> bool {
        self.try_write_at(1).is_ok()
    }

    /// Buffered data is available for a kernel `writev`.
    pub(super) fn has_tx_data(&self) -> bool {
        let state = self.core.load_state();
        let (rp, wp) = unpack(state);
        self.data_(rp, wp) > 0 && !has_flag(state, SEND_IN_FLIGHT)
    }

    /// Free space is available for a kernel `readv`.
    pub(super) fn has_recv_space(&self) -> bool {
        let state = self.core.load_state();
        let (rp, wp) = unpack(state);
        self.free_(rp, wp) > 0 && !has_flag(state, RECV_IN_FLIGHT)
    }

    /// Borrow a write segment over `[start, start + take)`.
    ///
    /// The segment's buffer is the ring's own memory. When it drops it
    /// commits the amount actually consumed to the ring (the `abs_buff`
    /// per-piece reclaim granularity).
    pub(super) fn write_segm<'a>(&'a self, start: usize, take: usize) -> ReclSliceMut<'a, T> {
        let whole: &'a mut [MaybeUninit<T>] = self.buffer_uninit();
        let slice: &'a mut [MaybeUninit<T>] = &mut whole[start..start + take];
        SegmMut::new(slice, WriterReclaim::new(&self.core, self.cap()))
    }

    /// Borrow a read segment over `[start, start + take)`.
    ///
    /// The segment's buffer is the ring's own memory. When it drops it
    /// commits the amount actually consumed to the ring.
    ///
    /// # Safety
    ///
    /// The returned `SegmRef` wraps `&'a mut [T]` over the readable region.
    /// This is sound as long as the caller never overlaps a live segment
    /// with a runtime reservation ([`RingBuffer::take_send_iovecs`]) or with
    /// another reader, which the SPSC contract of the ring rules out.
    pub(super) fn read_segm<'a>(&'a self, start: usize, take: usize) -> ReclSliceRef<'a, T> {
        // SAFETY: the readable region `[start, start + take)` is exclusively
        // owned by the reader while the returned segment is alive (SPSC
        // single consumer; the runtime's send reservation blocks concurrent
        // kernel reads).
        let base = self.buffer.as_ptr().cast_mut();
        let slice: &'a mut [T] = unsafe { core::slice::from_raw_parts_mut(base.add(start), take) };
        SegmRef::new(slice, ReaderReclaim::new(&self.core, self.cap()))
    }

    /// Borrow a peek segment (no reclaim) over `[start, start + take)`.
    pub(super) fn peek_segm<'a>(&'a self, start: usize, take: usize) -> ReclPeekRef<'a, T> {
        let base = self.buffer.as_ptr().cast_mut();
        let slice: &'a mut [T] = unsafe { core::slice::from_raw_parts_mut(base.add(start), take) };
        SegmRef::new(slice, NoReclaim)
    }

    // ------------------------------------------------------------------
    // closing
    // ------------------------------------------------------------------

    /// Close the tx end: no more data will be written by the user.
    pub fn close_tx(&self) {
        self.core.close_tx();
    }

    /// Close the rx end: no more data will be read by the user.
    pub fn close_rx(&self) {
        self.core.close_rx();
    }

    // ------------------------------------------------------------------
    // waker registration helpers (used by the futures)
    // ------------------------------------------------------------------

    pub(super) fn register_tx_user(&self, w: &Waiter) {
        self.core.register_tx_user(w);
    }
    pub(super) fn deregister_tx_user(&self, w: &Waiter) {
        self.core.deregister_tx_user(w);
    }
    pub(super) fn register_tx_runtime(&self, w: &Waiter) {
        self.core.register_tx_runtime(w);
    }
    pub(super) fn deregister_tx_runtime(&self, w: &Waiter) {
        self.core.deregister_tx_runtime(w);
    }
    pub(super) fn register_rx_user(&self, w: &Waiter) {
        self.core.register_rx_user(w);
    }
    pub(super) fn deregister_rx_user(&self, w: &Waiter) {
        self.core.deregister_rx_user(w);
    }
    pub(super) fn register_rx_runtime(&self, w: &Waiter) {
        self.core.register_rx_runtime(w);
    }
    pub(super) fn deregister_rx_runtime(&self, w: &Waiter) {
        self.core.deregister_rx_runtime(w);
    }
}

// --- park conditions -----------------------------------------------------

/// The user writer can proceed if there is free space (and the runtime is not
/// receiving into the ring).
pub(super) fn check_tx_writable<B, T>(ring: &RingBuffer<B, T>, arg: usize) -> bool
where
    B: DerefMut<Target = [T]>,
{
    ring.try_write_at(arg.max(1)).is_ok()
}

/// The user reader can proceed if there is data (and the runtime is not
/// sending from the ring), or the rx end is closed.
pub(super) fn check_rx_readable<B, T>(ring: &RingBuffer<B, T>, arg: usize) -> bool
where
    B: DerefMut<Target = [T]>,
{
    ring.try_read_at(arg).is_ok() || ring.is_rx_closed()
}

/// Same as [`check_rx_readable`] for peeking.
pub(super) fn check_rx_peekable<B, T>(ring: &RingBuffer<B, T>, _: usize) -> bool
where
    B: DerefMut<Target = [T]>,
{
    ring.try_peek_at().is_ok() || ring.is_rx_closed()
}

/// The runtime can proceed if buffered data is available for a kernel write,
/// or the tx end is closed.
pub(super) fn check_tx_flushed<B, T>(ring: &RingBuffer<B, T>, _: usize) -> bool
where
    B: DerefMut<Target = [T]>,
{
    ring.has_tx_data() || ring.is_tx_closed()
}

/// The runtime can proceed if free space is available for a kernel read, or
/// the rx end is closed.
pub(super) fn check_rx_idle<B, T>(ring: &RingBuffer<B, T>, _: usize) -> bool
where
    B: DerefMut<Target = [T]>,
{
    ring.has_recv_space() || ring.is_rx_closed()
}

// SAFETY: all shared state is atomic; the buffer memory is only touched
// through the position state machine, so a `RingBuffer` can be shared between
// the user thread and the runtime thread.
unsafe impl<B, T> Send for RingBuffer<B, T>
where
    B: DerefMut<Target = [T]>,
    B: Send,
    T: Send,
{
}

unsafe impl<B, T> Sync for RingBuffer<B, T>
where
    B: DerefMut<Target = [T]>,
    B: Sync,
    T: Send + Sync,
{
}

impl<B, T> Drop for RingBuffer<B, T>
where
    B: DerefMut<Target = [T]>,
{
    fn drop(&mut self) {
        // A region reserved by the runtime is referenced by `&'static`
        // slices. Returning them (put_back_*) before the last reference to
        // the ring drops is part of the protocol; dropping the ring
        // otherwise would dangle those references.
        let state = self.core.load_state();
        debug_assert!(
            !has_flag(state, SEND_IN_FLIGHT) && !has_flag(state, RECV_IN_FLIGHT),
            "[RingBuffer::drop] a region is still reserved by the runtime"
        );
    }
}

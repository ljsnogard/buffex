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
    borrow::Borrow,
    fmt,
    mem::MaybeUninit,
    ops::DerefMut,
    ptr,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

use super::{
    futures_::{WaitFlushed, WaitRxIdle},
    reclaim_::{
        ReadReclaim, ReaderReclaim, ReclPeekRef, ReclSliceMut, ReclSliceRef, SegmSlicesMut,
        SegmSlicesRef, WriterReclaim,
    },
    rx_::RingRx,
    tx_::RingTx,
};

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
    fn load_state_(&self) -> usize {
        self.state.load(Ordering::Acquire)
    }

    /// Spin compare-exchange loop: replace the state word with `f(state)`,
    /// retrying on contention (the `atomic_sync` way of updating packed
    /// flags).
    fn update_state_(&self, f: impl Fn(usize) -> usize) {
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

    fn set_flag_(&self, flag: usize) {
        self.update_state_(|s| s | flag);
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
        self.update_state_(|s| {
            let (rp, wp) = unpack(s);
            debug_assert!(rp < cap && wp < cap);
            pack(rp, (wp + amount) % cap) | (s & FLAG_MASK)
        });
        self.signal_all();
    }

    /// Atomically advance the reader position by `amount` (mod `cap`),
    /// preserving the flags.
    pub(super) fn advance_read(&self, cap: usize, amount: usize) {
        self.update_state_(|s| {
            let (rp, wp) = unpack(s);
            debug_assert!(rp < cap && wp < cap);
            pack((rp + amount) % cap, wp) | (s & FLAG_MASK)
        });
        self.signal_all();
    }

    fn close_tx(&self) {
        self.set_flag_(TX_CLOSED);
        self.signal_all();
    }

    fn close_rx(&self) {
        self.set_flag_(RX_CLOSED);
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
    // ==================================================================
    // All `RingBuffer` methods live in this single impl block, grouped by
    // role so the whole API can be reviewed at once. Visibility tiers:
    //
    // * `pub`         — the minimal safe API surface for end users;
    // * `pub(crate)`  — state-machine primitives used by the halves, the
    //                   async adapters and the kernel handoff; kept out of
    //                   the public API because misusing them (e.g. advancing
    //                   positions out of bounds) would corrupt the ring;
    // * `pub(super)`  — helpers shared within `ring_buffer` only.
    // ==================================================================

    // ------------------------------------------------------------------
    // construction & sizing (public)
    // ------------------------------------------------------------------

    /// Create a ring buffer from one owned heap buffer.
    ///
    /// Returns `Err(len)` if the buffer is too large (longer than
    /// [`MAX_CAPACITY`], 0x3FFFFFFF on 64 bit platform), or too small
    /// (shorter than 2)
    pub fn try_new(buffer: B) -> Result<Self, usize> {
        let cap = buffer.len();
        if !(2..=MAX_CAPACITY).contains(&cap) {
            return Result::Err(cap);
        }
        Result::Ok(RingBuffer {
            buffer,
            core: RingCore::new(),
        })
    }

    /// The buffer length.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    /// A snapshot of the number of buffered items.
    #[inline]
    pub fn data_size(&self) -> usize {
        let (rp, wp) = unpack(self.core.load_state_());
        self.data_(rp, wp)
    }

    /// The number of free slots.
    #[inline]
    pub fn free_size(&self) -> usize {
        let (rp, wp) = unpack(self.core.load_state_());
        self.free_(rp, wp)
    }

    // ------------------------------------------------------------------
    // state queries (public: read-only, cannot corrupt the ring)
    // ------------------------------------------------------------------

    pub fn is_tx_closed(&self) -> bool {
        has_flag(self.core.load_state_(), TX_CLOSED)
    }

    pub fn is_rx_closed(&self) -> bool {
        has_flag(self.core.load_state_(), RX_CLOSED)
    }

    // Raw position snapshots. Crate-internal: they expose the packed-layout
    // detail and are only used by tests / debugging; the public state
    // queries are `data_size` / `free_size` / `is_*_closed`.
    #[allow(dead_code)] // kept as the counterpart of `writer_pos` for debugging
    pub(crate) fn reader_pos(&self) -> usize {
        unpack(self.core.load_state_()).0
    }

    #[cfg_attr(not(test), allow(dead_code))] // used by the test suite
    pub(crate) fn writer_pos(&self) -> usize {
        unpack(self.core.load_state_()).1
    }

    // ------------------------------------------------------------------
    // position advancement (crate-internal)
    //
    // These mutate the packed positions without bounds checking: the caller
    // (the segments' reclaim, the async adapters) must pass amounts no
    // larger than the free / readable region. Public exposure would let a
    // caller forge readable data (`advance_write(n)` with `n` larger than
    // the free space), so they are crate-internal only.
    // ------------------------------------------------------------------

    /// Atomically advance the writer position by `amount` (mod `cap`).
    pub(crate) fn advance_write(&self, amount: usize) {
        self.core.advance_write(self.capacity(), amount);
    }

    /// Atomically advance the reader position by `amount` (mod `cap`).
    pub(crate) fn advance_read(&self, amount: usize) {
        self.core.advance_read(self.capacity(), amount);
    }

    // ------------------------------------------------------------------
    // user side: write (borrow of the whole writable region)
    //
    // The returned `(start, take)` may describe a region that wraps around
    // the buffer end; the two-piece segment types ([`ReclSliceMut`]) express
    // it as one logical segment.
    // ------------------------------------------------------------------

    /// Borrow up to `length` writable items starting at `wp`.
    ///
    /// The region may wrap around the buffer end; the caller must build a
    /// two-piece segment via [`RingBuffer::write_segm`].
    ///
    /// * `TxError::Stuffed` — the ring is full (or the writable region is
    ///   reserved by the runtime for a kernel read).
    /// * `TxError::Closing` — the tx end is closed.
    pub(crate) fn try_write_at(&self, length: usize) -> Result<(usize, usize), super::TxError<usize>> {
        use super::TxError;
        let state = self.core.load_state_();
        let (rp, wp) = unpack(state);
        let free = self.free_(rp, wp);
        if free == 0 || has_flag(state, RECV_IN_FLIGHT) {
            if has_flag(state, TX_CLOSED) {
                return Err(TxError::Closing);
            }
            return Err(TxError::Stuffed(wp));
        }
        // 取整个可写区域（最多 `length`），跨末端环绕时由两段式写段表达；
        let take = core::cmp::min(length, free);
        debug_assert!(take > 0);
        Ok((wp, take))
    }

    // ------------------------------------------------------------------
    // user side: read (borrow of the whole readable region)
    // ------------------------------------------------------------------

    /// Borrow up to `length` readable items starting at `rp`.
    ///
    /// The region may wrap around the buffer end; the caller must build a
    /// two-piece segment via [`RingBuffer::read_segm`].
    pub(crate) fn try_read_at(&self, length: usize) -> Result<(usize, usize), super::RxError<usize>> {
        use super::RxError;
        let state = self.core.load_state_();
        let (rp, wp) = unpack(state);
        let data = self.data_(rp, wp);
        if data == 0 || has_flag(state, SEND_IN_FLIGHT) {
            if has_flag(state, RX_CLOSED) {
                return Err(RxError::Closing);
            }
            return Err(RxError::Drained(rp));
        }
        // 取整个可读区域（最多 `length`）；
        let take = core::cmp::min(length, data);
        debug_assert!(take > 0);
        Ok((rp, take))
    }

    /// Borrow the whole readable region starting at `rp` (for peeking).
    pub(crate) fn try_peek_at(&self) -> Result<(usize, usize), super::RxError<usize>> {
        use super::RxError;
        let state = self.core.load_state_();
        let (rp, wp) = unpack(state);
        let data = self.data_(rp, wp);
        if data == 0 || has_flag(state, SEND_IN_FLIGHT) {
            if has_flag(state, RX_CLOSED) {
                return Err(RxError::Closing);
            }
            return Err(RxError::Drained(rp));
        }
        // 取整个可读区域；
        let take = data;
        debug_assert!(take > 0);
        Ok((rp, take))
    }

    // ------------------------------------------------------------------
    // runtime side: kernel submission (scatter / gather)
    //
    // Crate-internal (used by `crate::unix_stream::BufferedUnixStream`).
    // These hand out `&'static` slices over the ring's own memory: the
    // lifetime is not bound to the ring, so the caller must return the
    // reservation (`put_back_*`) before the last reference to the ring is
    // dropped, and must not let a live reservation overlap a user segment.
    // They are kept out of the public API until this ownership is reworked
    // into a lifetime-bound reservation guard.
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
    pub(crate) fn take_send_iovecs(&self) -> Option<(&'static [T], &'static [T])> {
        let mut state = self.core.load_state_();
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
    pub(crate) fn put_back_send(&self, written: usize) {
        let cap = self.capacity();
        self.core.update_state_(|s| {
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
    pub(crate) fn take_recv_iovecs(&self) -> Option<(&'static mut [T], &'static mut [T])> {
        let mut state = self.core.load_state_();
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
    pub(crate) fn put_back_recv(&self, received: usize) {
        let cap = self.capacity();
        self.core.update_state_(|s| {
            let (rp, wp) = unpack(s);
            debug_assert!(has_flag(s, RECV_IN_FLIGHT));
            let nw = (wp + received) % cap;
            // clear RECV_IN_FLIGHT, keep the other flags
            pack(rp, nw) | (s & FLAG_MASK & !RECV_IN_FLIGHT)
        });
        self.core.signal_all();
    }

    #[inline]
    fn data_(&self, rp: usize, wp: usize) -> usize {
        (wp + self.capacity() - rp) % self.capacity()
    }

    /// Free slots; the single-gap scheme always keeps one slot unused.
    #[inline]
    fn free_(&self, rp: usize, wp: usize) -> usize {
        self.capacity() - 1 - self.data_(rp, wp)
    }

    /// The readable region `[rp, rp+len)` as up to two slices.
    fn readable_slices_(&self, rp: usize, len: usize) -> (&'static [T], &'static [T]) {
        let base = self.buffer.as_ptr();
        let first = core::cmp::min(len, self.capacity() - rp);
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
        let first = core::cmp::min(len, self.capacity() - wp);
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
        unsafe { core::slice::from_raw_parts(self.buffer.as_ptr(), self.capacity()) }
    }

    /// A write view over the whole buffer (used by the framework adapters).
    ///
    /// The ring is shared (`&self`) but the view is handed out through the
    /// raw buffer pointer (interior-mutability style). This is only sound
    /// while the caller respects the SPSC ownership protocol: the writable
    /// region must not overlap a live write segment, a runtime recv
    /// reservation, or the reader's region.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(super) fn buffer_uninit(&self) -> &mut [MaybeUninit<T>] {
        unsafe {
            core::slice::from_raw_parts_mut(
                self.buffer.as_ptr().cast_mut().cast::<MaybeUninit<T>>(),
                self.capacity(),
            )
        }
    }

    /// A writable borrow exists right now (used by `TrBuffWrite::is_blocked`).
    pub(super) fn has_tx_space(&self) -> bool {
        self.try_write_at(1).is_ok()
    }

    /// Buffered data is available for a kernel `writev`.
    pub(super) fn has_tx_data(&self) -> bool {
        let state = self.core.load_state_();
        let (rp, wp) = unpack(state);
        self.data_(rp, wp) > 0 && !has_flag(state, SEND_IN_FLIGHT)
    }

    /// Free space is available for a kernel `readv`.
    pub(super) fn has_recv_space(&self) -> bool {
        let state = self.core.load_state_();
        let (rp, wp) = unpack(state);
        self.free_(rp, wp) > 0 && !has_flag(state, RECV_IN_FLIGHT)
    }

    /// The writable space is at least `amount` units (and not reserved by the
    /// runtime for a kernel read). Used to honour the lower bound of a
    /// [`Demand`](abs_buff::Demand).
    pub(super) fn has_free_at_least(&self, amount: usize) -> bool {
        let state = self.core.load_state_();
        let (rp, wp) = unpack(state);
        self.free_(rp, wp) >= amount && !has_flag(state, RECV_IN_FLIGHT)
    }

    /// The readable data is at least `amount` units (and not reserved by the
    /// runtime for a kernel write). Used to honour the lower bound of a
    /// [`Demand`](abs_buff::Demand).
    pub(super) fn has_data_at_least(&self, amount: usize) -> bool {
        let state = self.core.load_state_();
        let (rp, wp) = unpack(state);
        self.data_(rp, wp) >= amount && !has_flag(state, SEND_IN_FLIGHT)
    }

    /// Borrow a write segment over the region `[start, start + take)`, which
    /// may wrap around the buffer end (then it is a two-piece segment, see
    /// [`reclaim_`](self)).
    ///
    /// The segment's buffer is the ring's own memory. When it drops it
    /// commits the amount actually consumed to the ring (the per-piece
    /// reclaim granularity).
    pub(super) fn write_segm<'a>(&'a self, start: usize, take: usize) -> ReclSliceMut<'a, T> {
        let whole: &'a mut [MaybeUninit<T>] = self.buffer_uninit();
        let cap = self.capacity();
        let first = core::cmp::min(take, cap - start);
        let pieces = if first < take {
            // 跨末端环绕：两段物理空间 [start, cap) + [0, take - first)；
            // 先用 split_at_mut 切出 [0, start)，再从中取环绕段。
            let (head, tail) = whole.split_at_mut(start);
            let b = &mut head[..take - first];
            SegmSlicesMut::Two(tail, b)
        } else {
            SegmSlicesMut::One(&mut whole[start..start + take])
        };
        ReclSliceMut::new(pieces, WriterReclaim::new(&self.core, cap))
    }

    /// Borrow a read segment over the region `[start, start + take)`, which
    /// may wrap around the buffer end (then it is a two-piece segment).
    ///
    /// The segment's buffer is the ring's own memory. When it drops it
    /// commits the amount actually consumed to the ring.
    ///
    /// # Safety
    ///
    /// The returned segment wraps `&'a mut [T]` over the readable region.
    /// This is **not** enforced by the ring: the caller must ensure the
    /// region is not concurrently touched by a runtime reservation
    /// (`RingBuffer::take_send_iovecs`) or by another reader / writer while
    /// the segment is alive, and must not overlap two live segments. The SPSC
    /// contract of the ring is meant to rule this out, but it is a caller
    /// obligation, not a type-level guarantee.
    pub(super) fn read_segm<'a>(&'a self, start: usize, take: usize) -> ReclSliceRef<'a, T> {
        // SAFETY: the caller obtained `start`/`take` from `try_read_at`, so
        // the region is within the buffer; the aliasing obligation is
        // documented above.
        let base = self.buffer.as_ptr().cast_mut();
        let cap = self.capacity();
        let first = core::cmp::min(take, cap - start);
        let pieces = if first < take {
            let a = unsafe { core::slice::from_raw_parts_mut(base.add(start), first) };
            let b = unsafe { core::slice::from_raw_parts_mut(base, take - first) };
            SegmSlicesRef::Two(a, b)
        } else {
            let a = unsafe { core::slice::from_raw_parts_mut(base.add(start), take) };
            SegmSlicesRef::One(a)
        };
        ReclSliceRef::new(
            pieces,
            ReadReclaim::Consume(ReaderReclaim::new(&self.core, cap)),
        )
    }

    /// Borrow a peek segment (drop does not move the reader) over the region
    /// `[start, start + take)`, which may wrap around the buffer end.
    pub(super) fn peek_segm<'a>(&'a self, start: usize, take: usize) -> ReclPeekRef<'a, T> {
        let base = self.buffer.as_ptr().cast_mut();
        let cap = self.capacity();
        let first = core::cmp::min(take, cap - start);
        let pieces = if first < take {
            let a = unsafe { core::slice::from_raw_parts_mut(base.add(start), first) };
            let b = unsafe { core::slice::from_raw_parts_mut(base, take - first) };
            SegmSlicesRef::Two(a, b)
        } else {
            let a = unsafe { core::slice::from_raw_parts_mut(base.add(start), take) };
            SegmSlicesRef::One(a)
        };
        ReclSliceRef::new(pieces, ReadReclaim::Peek)
    }

    // ------------------------------------------------------------------
    // splitting into halves
    //
    // public: `try_split_shared` (the shared-ring pattern, e.g. over
    // `Arc<RingBuffer>`). The `&mut self` borrow splits and the single-half
    // variants are crate-internal for now: nothing outside the crate uses
    // them, and the shared pattern covers the same needs.
    //
    // The split must be guarded by a strong-reference-count check: splitting
    // clones the shared handle into the two halves, so a successful split
    // creates exactly one producer/consumer *pair*. If the handle had already
    // been cloned elsewhere (count > 1), those clones could be split into a
    // second (or further) pair, turning SPSC into MPMC and breaking the
    // lock-free state machine (two writers racing on `wp`, two readers on
    // `rp`, overlapping segments). Requiring the caller to be the sole owner
    // (count == 1) rules that out.
    // ------------------------------------------------------------------

    /// Split a ring shared through the smart pointer `S` (e.g. `Arc<Self>`)
    /// into a write half and a read half.
    ///
    /// `strong_count` must return the number of strong references on the
    /// shared allocation behind `ring_buff` (e.g. `|a| Arc::strong_count(a)`
    /// for `S = Arc<Self>`). The split is only allowed while that count is
    /// exactly `1` — i.e. the caller must hold the sole reference — so that
    /// no second producer/consumer pair can ever be created from another
    /// clone (see the section comment above). On failure the handle is
    /// returned unchanged in `Err`.
    ///
    /// For non-refcounted handles (e.g. `&Self`, where the borrow checker
    /// already prevents a second split) pass `|_| 1` and rely on the caller's
    /// own exclusivity guarantee.
    #[allow(clippy::type_complexity)] // the tuple of two generic halves is inherent to the API
    pub fn try_split_shared<S>(
        ring_buff: S,
        strong_count: impl Fn(&S) -> usize,
        weak_count: impl Fn(&S) -> usize,
    ) -> Result<(RingTx<S, B, T>, RingRx<S, B, T>), S>
    where
        S: Borrow<Self> + Clone + Send + Sync,
    {
        if strong_count(&ring_buff) == 1 && weak_count(&ring_buff) == 0 {
            Result::Ok((RingTx::new(ring_buff.clone()), RingRx::new(ring_buff)))
        } else {
            Result::Err(ring_buff)
        }
    }

    /// Split off only the write half from a shared ring. Guarded like
    /// [`RingBuffer::try_split_shared`] so that at most one producer exists.
    #[allow(dead_code)] // unused in-crate so far; kept as internal API
    pub(crate) fn try_split_shared_tx<S>(
        ring_buff: S,
        strong_count: impl Fn(&S) -> usize,
    ) -> Result<RingTx<S, B, T>, S>
    where
        S: Borrow<Self> + Send + Sync,
    {
        if strong_count(&ring_buff) == 1 {
            Result::Ok(RingTx::new(ring_buff))
        } else {
            Result::Err(ring_buff)
        }
    }

    /// Split off only the read half from a shared ring. Guarded like
    /// [`RingBuffer::try_split_shared`] so that at most one consumer exists.
    #[allow(dead_code)] // unused in-crate so far; kept as internal API
    pub(crate) fn try_split_shared_rx<S>(
        ring_buff: S,
        strong_count: impl Fn(&S) -> usize,
    ) -> Result<RingRx<S, B, T>, S>
    where
        S: Borrow<Self> + Send + Sync,
    {
        if strong_count(&ring_buff) == 1 {
            Result::Ok(RingRx::new(ring_buff))
        } else {
            Result::Err(ring_buff)
        }
    }

    /// Split the ring into a write half and a read half, borrowing the ring
    /// for `'a`.
    #[cfg_attr(not(test), allow(dead_code))] // used by the test suite
    pub(crate) fn split(&mut self) -> (RingTx<&Self, B, T>, RingRx<&Self, B, T>) {
        let ring: &Self = self;
        (RingTx::new(ring), RingRx::new(ring))
    }

    /// Split off only the write half.
    #[allow(dead_code)] // unused in-crate so far; kept as internal API
    pub(crate) fn split_tx(&mut self) -> RingTx<&Self, B, T> {
        let ring: &Self = self;
        RingTx::new(ring)
    }

    /// Split off only the read half.
    #[allow(dead_code)] // unused in-crate so far; kept as internal API
    pub(crate) fn split_rx(&mut self) -> RingRx<&Self, B, T> {
        let ring: &Self = self;
        RingRx::new(ring)
    }

    // ------------------------------------------------------------------
    // runtime-side waiting
    //
    // Crate-internal: they pair with the (also crate-internal) kernel
    // handoff, e.g. the `crate::unix_stream::BufferedUnixStream` driver
    // tasks. They are not part of the public API while the `&'static`
    // reservation lifetime of the handoff is under rework.
    // ------------------------------------------------------------------

    /// Wait until buffered data is available for a kernel `writev` (or the tx
    /// end is closed).
    pub(crate) fn wait_flushed(&self) -> WaitFlushed<'_, B, T> {
        WaitFlushed::new(self)
    }

    /// Wait until free space is available for a kernel `readv` (or the rx end
    /// is closed).
    pub(crate) fn wait_rx_idle(&self) -> WaitRxIdle<'_, B, T> {
        WaitRxIdle::new(self)
    }

    // ------------------------------------------------------------------
    // closing (crate-internal: users close through `RingTx::close` /
    // `RingRx::close`)
    // ------------------------------------------------------------------

    /// Close the tx end: no more data will be written by the user.
    pub(crate) fn close_tx(&self) {
        self.core.close_tx();
    }

    /// Close the rx end: no more data will be read by the user.
    pub(crate) fn close_rx(&self) {
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

/// Demand 下限版的 [`check_tx_writable`]：可写空间必须至少 `arg` 格（`arg` 为
/// `Demand::min`，无下限时传 0）才允许写者继续。用于尊重 `Demand::at_least`
/// 语义：空间不足下限时，等待中的写者不会被放行（保持 Pending）。
pub(super) fn check_tx_writable_at_least<B, T>(ring: &RingBuffer<B, T>, arg: usize) -> bool
where
    B: DerefMut<Target = [T]>,
{
    ring.has_free_at_least(arg.max(1))
}

/// Demand 下限版的 [`check_rx_readable`]：可读数据必须至少 `arg` 格（`arg` 为
/// `Demand::min`，无下限时传 0），或 rx 已关闭（EOF），才允许读者继续。
pub(super) fn check_rx_readable_at_least<B, T>(ring: &RingBuffer<B, T>, arg: usize) -> bool
where
    B: DerefMut<Target = [T]>,
{
    ring.has_data_at_least(arg.max(1)) || ring.is_rx_closed()
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

impl<B, T> fmt::Debug for RingBuffer<B, T>
where
    B: DerefMut<Target = [T]>,
{
    /// Print the positions and state flags (diagnostics only; the buffer
    /// contents are not printed).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.core.load_state_();
        let (rp, wp) = unpack(state);
        f.debug_struct("RingBuffer")
            .field("capacity", &self.capacity())
            .field("rp", &rp)
            .field("wp", &wp)
            .field("tx_closed", &has_flag(state, TX_CLOSED))
            .field("rx_closed", &has_flag(state, RX_CLOSED))
            .field("send_in_flight", &has_flag(state, SEND_IN_FLIGHT))
            .field("recv_in_flight", &has_flag(state, RECV_IN_FLIGHT))
            .finish()
    }
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
        let state = self.core.load_state_();
        debug_assert!(
            !has_flag(state, SEND_IN_FLIGHT) && !has_flag(state, RECV_IN_FLIGHT),
            "[RingBuffer::drop] a region is still reserved by the runtime"
        );
    }
}

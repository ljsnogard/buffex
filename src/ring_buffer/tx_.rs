//! The write (tx) half of the ring buffer.

use core::{
    borrow::Borrow,
    cell::UnsafeCell,
    marker::PhantomPinned,
    ops::DerefMut,
};

use anylr::SomeOf;

use abs_buff::{
    x_deps::{anylr, abs_cancel},
    Demand, TrBuffTryWrite, TrBuffWrite,
};

use super::{
    error_::TxError,
    futures_::WriteAsync,
    reclaim_::ReclSliceMut,
    state_::{RingBuffer, Waiter},
};

/// To move data into the ring buffer (the producer / user side).
///
/// The half holds a shared reference to the ring (`H: Borrow<RingBuffer>`),
/// which may be `&RingBuffer` or `Arc<RingBuffer>`.
pub struct RingTx<H, B, T = u8>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    _pin: PhantomPinned,
    ring: H,
    /// Waker slot used by the poll-based `AsyncWrite` implementations.
    pub(super) waiter: UnsafeCell<Waiter>,
    /// Marker tying the element / buffer types.
    _marker: core::marker::PhantomData<(B, T)>,
}

impl<H, B, T> RingTx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    pub(super) fn new(ring: H) -> Self {
        RingTx {
            _pin: PhantomPinned,
            ring,
            waiter: UnsafeCell::new(Waiter::new()),
            _marker: core::marker::PhantomData,
        }
    }

    #[inline]
    pub fn ring(&self) -> &RingBuffer<B, T> {
        self.ring.borrow()
    }

    /// The underlying shared handle (`H`), e.g. `&Arc<RingBuffer>`.
    ///
    /// Used after a `try_split_shared` split to clone the handle for a
    /// runtime-side (kernel handoff) task. Cloning keeps the strong count
    /// above one, so a further `try_split_shared` is rejected — the
    /// one-pair SPSC invariant is preserved.
    #[inline]
    pub(crate) fn shared(&self) -> &H {
        &self.ring
    }

    pub fn is_blocked_closing(&self) -> bool {
        !self.ring().has_tx_space() || self.ring().is_tx_closed()
    }

    pub fn write_async<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> WriteAsync<'f, H, B, T> {
        // 尊重 Demand 的 [min, max] 区间：可写空间不足 min 时未来保持 Pending；
        let min_len = demand.min().copied().unwrap_or(0);
        let max_len = demand.max().copied().unwrap_or(usize::MAX);
        WriteAsync::new(self, min_len, max_len)
    }

    /// Borrow up to `length` writable units (no more than `length`).
    ///
    /// The region may wrap around the buffer end; the returned segment is
    /// then a two-piece segment that treats the two physical slices as one
    /// logical segment, so a single borrow can cover the whole free space.
    /// When it drops, the segment commits exactly the amount consumed
    /// (the per-piece reclaim granularity).
    ///
    /// The name carries `_at_most` to tell it apart from the
    /// [`TrBuffTryWrite::try_write`] trait method, which takes a
    /// [`Demand`](abs_buff::Demand) instead of a plain length.
    pub fn try_write_at_most(&mut self, length: usize) -> Result<ReclSliceMut<'_, T>, TxError<usize>> {
        let ring = self.ring();
        let (start, take) = ring.try_write_at(length)?;
        Ok(ring.write_segm(start, take))
    }

    /// Borrow up to `length` writable units in an async manner, waiting for
    /// free space automatically. See [`RingTx::try_write_at_most`] for the
    /// `_at_most` naming (vs the [`TrBuffWrite::write_async`] trait method
    /// which takes a [`Demand`](abs_buff::Demand)).
    pub fn write_at_most_async(&mut self, length: usize) -> WriteAsync<'_, H, B, T> {
        WriteAsync::new(self, 0, length)
    }

    /// Close the tx end: no more data will be written by the user.
    pub fn close(&mut self) {
        self.ring().close_tx();
    }

    pub fn is_closed(&self) -> bool {
        self.ring().is_tx_closed()
    }

    /// The buffer length.
    pub fn capacity(&self) -> usize {
        self.ring().capacity()
    }

    /// The number of buffered items.
    pub fn data_size(&self) -> usize {
        self.ring().data_size()
    }

    /// The number of free slots.
    pub fn free_size(&self) -> usize {
        self.ring().free_size()
    }
}

impl<H, B, T> Drop for RingTx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn drop(&mut self) {
        let ring = self.ring();
        let waiter = unsafe { &*self.waiter.get() };
        ring.deregister_tx_user(waiter);
        ring.close_tx();
    }
}

// ---------------------------------------------------------------------------
// abs_buff traits
// ---------------------------------------------------------------------------

impl<H, B, T> TrBuffWrite<T> for RingTx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type SegmMut<'a> = ReclSliceMut<'a, T> where Self: 'a;
    type Err = TxError<usize>;

    #[inline]
    fn is_blocked_closing(&self) -> bool {
        RingTx::is_blocked_closing(self)
    }

    #[inline]
    fn write_async<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> impl abs_cancel::TrMayCancel<'f, MayCancelOutput =
        SomeOf<Self::SegmMut<'f>, Self::Err>>
    {
        RingTx::write_async(self, demand)
    }
}

impl<H, B, T> TrBuffTryWrite<T> for RingTx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn try_write<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> SomeOf<Self::SegmMut<'f>, Self::Err> {
        // 尊重 Demand 的 [min, max] 区间：可写空间不足 min 时按 Stuffed 处理，
        // 不返回一个不满足下限要求的段；
        let min_len = demand.min().copied().unwrap_or(0);
        let max_len = demand.max().copied().unwrap_or(usize::MAX);
        let ring = self.ring();
        match ring.try_write_at(max_len) {
            Ok((start, take)) => {
                if take < min_len {
                    let e = if ring.is_tx_closed() {
                        TxError::Closing
                    } else {
                        TxError::Stuffed(start)
                    };
                    SomeOf::new_right(e)
                } else {
                    SomeOf::new_left(ring.write_segm(start, take))
                }
            }
            Err(err) => SomeOf::new_right(err),
        }
    }
}

//! The read (rx) half of the ring buffer.

use core::{
    borrow::Borrow,
    cell::UnsafeCell,
    marker::PhantomPinned,
    ops::DerefMut,
};

use anylr::SomeOf;

use abs_buff::{
    x_deps::{anylr, abs_cancel},
    Demand, TrBuffPeek, TrBuffRead, TrBuffTryPeek, TrBuffTryRead,
};
use abs_cancel::TrMayCancel;

use super::{
    error_::RxError,
    futures_::{PeekAsync, ReadAsync},
    reclaim_::{ReclPeekRef, ReclSliceRef},
    state_::{RingBuffer, Waiter},
};

/// To move data out of the ring buffer (the consumer / user side).
///
/// The half holds a shared reference to the ring (`H: Borrow<RingBuffer>`),
/// which may be `&RingBuffer` or `Arc<RingBuffer>`.
pub struct RingRx<H, B, T = u8>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    _pin: PhantomPinned,
    ring: H,
    /// Waker slot used by the poll-based `AsyncRead` implementations.
    pub(super) waiter: UnsafeCell<Waiter>,
    /// Marker tying the element / buffer types.
    _marker: core::marker::PhantomData<(B, T)>,
}

impl<H, B, T> RingRx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    pub(super) fn new(ring: H) -> Self {
        RingRx {
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

    pub fn is_drained_closing(&self) -> bool {
        // "Drained" means no more data will ever be emitted: the rx end is
        // closed *and* the ring holds no buffered data.
        self.ring().data_size() == 0 && self.ring().is_rx_closed()
    }

    pub fn read_async(&mut self, demand: &Demand<usize>) -> ReadAsync<'_, H, B, T> {
        // 尊重 Demand 的 [min, max] 区间：可读数据不足 min 时未来保持 Pending；
        let min_len = demand.min().copied().unwrap_or(0);
        let max_len = demand.max().copied().unwrap_or(usize::MAX);
        ReadAsync::new(self, min_len, max_len)
    }

    /// Borrow up to `length` readable units (no more than `length`).
    ///
    /// The region may wrap around the buffer end; the returned segment is
    /// then a two-piece segment that treats the two physical slices as one
    /// logical segment. When it drops, the segment commits exactly the amount
    /// consumed (the per-piece reclaim granularity).
    ///
    /// The name carries `_at_most` to tell it apart from the
    /// [`TrBuffTryRead::try_read`] trait method, which takes a
    /// [`Demand`](abs_buff::Demand) instead of a plain length.
    pub fn try_read_at_most(&mut self, length: usize) -> Result<ReclSliceRef<'_, T>, RxError<usize>> {
        let ring = self.ring();
        let (start, take) = ring.try_read_at(length)?;
        Ok(ring.read_segm(start, take))
    }

    /// Borrow up to `length` readable units in an async manner, waiting for
    /// data (or closing) automatically. See [`RingRx::try_read_at_most`] for
    /// the `_at_most` naming (vs the [`TrBuffRead::read_async`] trait method
    /// which takes a [`Demand`](abs_buff::Demand)).
    pub fn read_at_most_async(&mut self, length: usize) -> ReadAsync<'_, H, B, T> {
        ReadAsync::new(self, 0, length)
    }

    /// Borrow all contiguous readable units without consuming them.
    pub fn try_peek(&mut self) -> Result<ReclPeekRef<'_, T>, RxError<usize>> {
        let ring = self.ring();
        let (start, take) = ring.try_peek_at()?;
        Ok(ring.peek_segm(start, take))
    }

    /// Borrow all contiguous readable units without consuming them, waiting
    /// for data (or closing) automatically.
    pub fn peek_async(&mut self) -> PeekAsync<'_, H, B, T> {
        PeekAsync::new(self)
    }

    /// Close the rx end: no more data will be read by the user.
    pub fn close(&mut self) {
        self.ring().close_rx();
    }

    pub fn is_closed(&self) -> bool {
        self.ring().is_rx_closed()
    }

    /// The buffer length.
    pub fn capacity(&self) -> usize {
        self.ring().capacity()
    }

    /// The number of buffered items.
    pub fn data_size(&self) -> usize {
        self.ring().data_size()
    }
}

impl<H, B, T> Drop for RingRx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn drop(&mut self) {
        let ring = self.ring();
        let waiter = unsafe { &*self.waiter.get() };
        ring.deregister_rx_user(waiter);
        ring.close_rx();
    }
}

// ---------------------------------------------------------------------------
// abs_buff traits
// ---------------------------------------------------------------------------

impl<H, B, T> TrBuffRead<T> for RingRx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type SegmRef<'a> = ReclSliceRef<'a, T> where Self: 'a;
    type Err = RxError<usize>;

    #[inline]
    fn is_drained_closing(&self) -> bool {
        RingRx::is_drained_closing(self)
    }

    #[inline]
    fn read_async<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> impl TrMayCancel<'f, MayCancelOutput =
        SomeOf<Self::SegmRef<'f>, Self::Err>>
    {
        RingRx::read_async(self, demand)
    }
}

impl<H, B, T> TrBuffTryRead<T> for RingRx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn try_read<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> SomeOf<Self::SegmRef<'f>, Self::Err> {
        // 尊重 Demand 的 [min, max] 区间：可读数据不足 min 时按 Drained 处理，
        // 不返回一个不满足下限要求的段；
        let min_len = demand.min().copied().unwrap_or(0);
        let max_len = demand.max().copied().unwrap_or(usize::MAX);
        let ring = self.ring();
        match ring.try_read_at(max_len) {
            Ok((start, take)) => {
                if take < min_len {
                    let e = if ring.is_rx_closed() {
                        RxError::Closing
                    } else {
                        RxError::Drained(start)
                    };
                    SomeOf::new_right(e)
                } else {
                    SomeOf::new_left(ring.read_segm(start, take))
                }
            }
            Err(err) => SomeOf::new_right(err),
        }
    }
}

impl<H, B, T> TrBuffPeek<T> for RingRx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type SegmPeek<'a> = ReclPeekRef<'a, T> where Self: 'a;
    type Err = RxError<usize>;

    fn peek_async<'f>(
        &'f mut self,
    ) -> impl abs_cancel::TrMayCancel<'f, MayCancelOutput =
        SomeOf<Self::SegmPeek<'f>, Self::Err>>
    {
        PeekAsync::new(self)
    }
}

impl<H, B, T> TrBuffTryPeek<T> for RingRx<H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn try_peek<'f>(&'f mut self) -> SomeOf<Self::SegmPeek<'f>, Self::Err> {
        match RingRx::try_peek(self) {
            Ok(segm) => SomeOf::new_left(segm),
            Err(err) => SomeOf::new_right(err),
        }
    }
}

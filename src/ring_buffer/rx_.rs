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

    /// Borrow up to `length` contiguous readable units. The returned segment
    /// commits its whole borrowed region when it drops.
    pub fn try_read(&mut self, length: usize) -> Result<ReclSliceRef<'_, T>, RxError<usize>> {
        let ring = self.ring();
        let (start, take) = ring.try_read_at(length)?;
        Ok(ring.read_segm(start, take))
    }

    /// Borrow up to `length` contiguous readable units in an async manner,
    /// waiting for data (or closing) automatically.
    pub fn read_async(&mut self, length: usize) -> ReadAsync<'_, H, B, T> {
        ReadAsync::new(self, length)
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

    fn is_drained(&self) -> bool {
        // "Drained" means no more data will ever be emitted: the rx end is
        // closed *and* the ring holds no buffered data.
        self.ring().data_size() == 0 && self.ring().is_rx_closed()
    }

    fn read_async<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> impl abs_cancel::TrMayCancel<
        'f,
        MayCancelOutput = SomeOf<Self::SegmRef<'f>, Self::Err>,
    > {
        let length = demand.max().copied().unwrap_or(usize::MAX);
        self.read_async(length)
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
        let length = demand.max().copied().unwrap_or(usize::MAX);
        match RingRx::try_read(self, length) {
            Ok(segm) => SomeOf::new_left(segm),
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
    ) -> impl abs_cancel::TrMayCancel<
        'f,
        MayCancelOutput = SomeOf<Self::SegmPeek<'f>, Self::Err>,
    > {
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

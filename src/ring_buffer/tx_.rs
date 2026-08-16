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

    /// Borrow up to `length` contiguous writable units.
    ///
    /// The returned segment commits its *whole* borrowed region to the ring
    /// when it drops (the abs_buff per-piece reclaim granularity). When the ring wraps, only the
    /// contiguous part starting at the writer position is returned; call
    /// again to obtain the wrapped part.
    pub fn try_write(&mut self, length: usize) -> Result<ReclSliceMut<'_, T>, TxError<usize>> {
        let ring = self.ring();
        let (start, take) = ring.try_write_at(length)?;
        Ok(ring.write_segm(start, take))
    }

    /// Borrow up to `length` contiguous writable units in an async manner,
    /// waiting for free space automatically.
    pub fn write_async(&mut self, length: usize) -> WriteAsync<'_, H, B, T> {
        WriteAsync::new(self, length)
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

    fn is_blocked(&self) -> bool {
        !self.ring().has_tx_space()
    }

    fn write_async<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> impl abs_cancel::TrMayCancel<
        'f,
        MayCancelOutput = SomeOf<Self::SegmMut<'f>, Self::Err>,
    > {
        let length = demand.max().copied().unwrap_or(usize::MAX);
        self.write_async(length)
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
        let length = demand.max().copied().unwrap_or(usize::MAX);
        match RingTx::try_write(self, length) {
            Ok(segm) => SomeOf::new_left(segm),
            Err(err) => SomeOf::new_right(err),
        }
    }
}

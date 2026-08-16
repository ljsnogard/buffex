//! The `RingBuffer` public API: construction, splitting into halves, and the
//! runtime (kernel) side vectored-IO operations.

use core::{borrow::Borrow, ops::DerefMut};

use super::{
    abs_::TrRingBuffer,
    rx_::RingRx,
    state_::RingBuffer,
    tx_::RingTx,
};

impl<B, T> RingBuffer<B, T>
where
    B: DerefMut<Target = [T]>,
{
    /// Split the ring into a write half and a read half, borrowing the ring
    /// for `'a`.
    pub fn split(&mut self) -> (RingTx<&Self, B, T>, RingRx<&Self, B, T>) {
        let ring: &Self = self;
        (RingTx::new(ring), RingRx::new(ring))
    }

    /// Split off only the write half.
    pub fn split_tx(&mut self) -> RingTx<&Self, B, T> {
        let ring: &Self = self;
        RingTx::new(ring)
    }

    /// Split off only the read half.
    pub fn split_rx(&mut self) -> RingRx<&Self, B, T> {
        let ring: &Self = self;
        RingRx::new(ring)
    }

    /// Split a ring shared through the smart pointer `S` (e.g. `Arc<Self>`)
    /// into a write half and a read half.
    ///
    /// The ring is internally synchronized through atomics, so any number of
    /// references may hold it; the halves only provide the user-side
    /// interfaces.
    pub fn try_split_shared<S>(ring_buff: S) -> (RingTx<S, B, T>, RingRx<S, B, T>)
    where
        S: Borrow<Self> + Clone + Send + Sync,
    {
        (RingTx::new(ring_buff.clone()), RingRx::new(ring_buff))
    }

    /// Split off only the write half from a shared ring.
    pub fn try_split_shared_tx<S>(ring_buff: S) -> RingTx<S, B, T>
    where
        S: Borrow<Self> + Send + Sync,
    {
        RingTx::new(ring_buff)
    }

    /// Split off only the read half from a shared ring.
    pub fn try_split_shared_rx<S>(ring_buff: S) -> RingRx<S, B, T>
    where
        S: Borrow<Self> + Send + Sync,
    {
        RingRx::new(ring_buff)
    }

    // ------------------------------------------------------------------
    // Runtime (kernel) side waiting
    // ------------------------------------------------------------------

    /// Wait until buffered data is available for a kernel `writev` (or the tx
    /// end is closed).
    pub fn wait_flushed(&self) -> super::futures_::WaitFlushed<'_, B, T> {
        super::futures_::WaitFlushed::new(self)
    }

    /// Wait until free space is available for a kernel `readv` (or the rx end
    /// is closed).
    pub fn wait_rx_idle(&self) -> super::futures_::WaitRxIdle<'_, B, T> {
        super::futures_::WaitRxIdle::new(self)
    }
}

impl<B, T> TrRingBuffer<T> for RingBuffer<B, T>
where
    B: DerefMut<Target = [T]>,
{
    type Tx<'a> = RingTx<&'a Self, B, T> where Self: 'a;
    type Rx<'a> = RingRx<&'a Self, B, T> where Self: 'a;

    #[inline]
    fn capacity(&self) -> usize {
        RingBuffer::capacity(self)
    }

    #[inline]
    fn data_size(&self) -> usize {
        RingBuffer::data_size(self)
    }

    fn try_split_io(&mut self) -> Option<(Self::Tx<'_>, Self::Rx<'_>)> {
        let ring: &Self = self;
        Option::Some((RingTx::new(ring), RingRx::new(ring)))
    }
}

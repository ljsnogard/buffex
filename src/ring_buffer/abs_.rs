use core::ops::DerefMut;

use abs_buff::{TrBuffTryPeek, TrBuffTryRead, TrBuffTryWrite};

use super::{
    rx_::RingRx,
    state_::RingBuffer,
    tx_::RingTx,
};

/// A full-duplex ring buffer that serves a pair of producer and consumer,
/// offering a conceptually infinite buffer by cycling the owned heap buffers
/// between the user side and the kernel (runtime) side.
///
/// The tx end is the *write* side: the user fills segments (abs_buff
/// compatible) which are flushed and handed to the runtime for kernel
/// submission. The rx end is the *read* side: the runtime fills buffers from
/// the kernel, and the user drains them through segment borrows.
pub trait TrRingBuffer<T = u8> {
    type Tx<'a>: 'a + TrBuffTryWrite<T> where Self: 'a;
    type Rx<'a>: 'a + TrBuffTryRead<T> + TrBuffTryPeek<T> where Self: 'a;

    /// The total number of units that the ring is capable of holding.
    fn capacity(&self) -> usize;

    /// A snapshot of the number of units that are currently buffered and
    /// readable at the rx end.
    fn data_size(&self) -> usize;

    /// Try to split the ring into a write half and a read half.
    ///
    /// Returns `None` for a write-only ring (built from 2 buffers).
    fn try_split_io(
        &mut self,
    ) -> Option<(Self::Tx<'_>, Self::Rx<'_>)>;
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

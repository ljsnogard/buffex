use abs_buff::{TrBuffIterTryRead, TrBuffIterTryWrite};
use segm_buff::x_deps::abs_buff;

/// A fixed size buffer that serves a pair of producer and a consumer, offering
/// a conceptually infinite sized buffer, by internally linking the head and the
/// tail of the buffer.
pub trait TrRingBuffer<T = u8> {
    type Tx<'a>: 'a + TrBuffIterTryWrite<T> where Self: 'a;
    type Rx<'a>: 'a + TrBuffIterTryRead<T> where Self: 'a;

    /// The number of units that the buffer is capable of.
    fn capacity(&self) -> usize;

    /// A snapshot of the number of units that the buffer currently stored.
    fn data_size(&self) -> usize;

    /// Try to split the buffer into a write half and a read half.
    fn try_split_io(
        &mut self,
    ) -> Option<(Self::Tx<'_>, Self::Rx<'_>)>;
}

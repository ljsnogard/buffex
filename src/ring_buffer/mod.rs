//! This mod contains the implementation of a ring buffer (or circular buffer)
//! that works with `no-std`, no internal heap-alloc, async-runtime agnostic.

mod abs_;
mod buffer_;
mod reclaim_;
mod peek_;
mod sync_;
mod rx_;
mod tx_;

#[cfg(test)]
mod tests_;

pub use abs_::TrRingBuffer;
pub use buffer_::{RingBuffer, RxError, TxError};
pub use peek_::{BuffPeek, PeekAsync};
pub use rx_::{BuffRx, ReadAsync};
pub use tx_::{BuffTx, WriteAsync};

pub(super) type Dual<T> = smallvec::SmallVec<[T; 2]>;

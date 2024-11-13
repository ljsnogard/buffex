mod abs_;
mod buffer_;
mod reclaim_;
mod peek_;
mod read_;
mod sync_;
mod write_;

#[cfg(test)]
mod chunk_;

#[cfg(test)]
mod tests_;

pub use abs_::TrRingBuffer;
pub use buffer_::{IoCtx, RingBuffer, RxError, TxError};
pub use peek_::{BuffPeek, PeekAsync};
pub use read_::{BuffRead, ReadAsync};
pub use write_::{BuffWrite, WriteAsync};

pub(super) type Dual<T> = smallvec::SmallVec<[T; 2]>;

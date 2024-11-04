mod abs_;
mod buffer_;
mod reclaim_;
mod peeker_;
mod reader_;
mod sync_;
mod writer_;

#[cfg(test)]
mod chunk_;

pub use abs_::TrRingBuffer;
pub use buffer_::{RingBuffer, RxError, TxError};
pub use peeker_::{BuffPeek, PeekAsync};
pub use reader_::{BuffRead, ReadAsync};
pub use writer_::{BuffWrite, WriteAsync};

pub(super) type Dual<T> = smallvec::SmallVec<[T; 2]>;

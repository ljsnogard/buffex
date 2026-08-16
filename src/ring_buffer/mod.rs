//! A single-buffer, lock-free ring buffer between a user thread and a runtime
//! (kernel) side.
//!
//! # Design
//!
//! `RingBuffer` exclusively owns **one** heap-allocated `[T]` buffer. The
//! storage type is generic and only requires `DerefMut<Target = [T]>`, so any
//! heap pointer such as `Box<[T]>` works:
//!
//! ```ignore
//! let ring = RingBuffer::<Box<[u8]>>::try_new(Box::from([0u8; 4096])).unwrap();
//! ```
//!
//! All shared state lives in a single `AtomicUsize`: the reader position
//! `rp`, the writer position `wp` and the four state flags (`tx_closed`,
//! `rx_closed`, `send_in_flight`, `recv_in_flight`) are packed into one word,
//! so a single atomic load observes everything and every transition is one
//! spin compare-exchange loop (the `atomic_sync` way of handling packed
//! flags). The ring is full when the writer position is immediately behind
//! the reader position (one slot is always left unused):
//!
//! * `data = (wp - rp) mod cap`
//! * `free = cap - 1 - data`
//!
//! The buffer length is limited to [`state_::MAX_CAPACITY`] (e.g. `2^30 - 1`
//! on 64-bit targets, where the flags take the top 4 bits) — exactly the
//! range of the native iovec length field.
//!
//! Two usage modes are supported on the same core:
//!
//! * **User pipe (tokio / smol poll mode)**: the user writes through
//!   [`RingTx`] (abs_buff segments, see below) and reads through [`RingRx`].
//!   This is the classic SPSC channel: one writer thread, one reader thread,
//!   no locks, no runtime dependency.
//! * **Kernel handoff (compio mode, scatter/gather)**: the runtime side takes
//!   the readable / writable region of the ring as an iovec pair
//!   ([`RingBuffer::take_send_iovecs`] / [`RingBuffer::take_recv_iovecs`])
//!   and submits it to the kernel with a single `writev` / `readv` syscall.
//!   The wrapped region becomes two slices packed into one iovec array; the
//!   kernel handles them in order. The region is returned with
//!   [`RingBuffer::put_back_send`] / [`RingBuffer::put_back_recv`].
//!
//! # Segments (abs_buff compatibility)
//!
//! The user-side borrows are `abs_buff::buffer` segments ([`SegmMut`] /
//! [`SegmRef`], re-exported as [`ReclSliceMut`] / [`ReclSliceRef`]) whose
//! buffer **is the ring's own memory** — produced / consumed data is written
//! directly into the ring, with no intermediate copy. Segments use the
//! `abs_buff` *per-piece reclaim granularity*: when a segment drops it
//! commits to the ring exactly the amount it consumed (the writer position
//! advances by the units handed over, the reader position by the units
//! taken), so a mid-transfer cancellation leaves the positions right after
//! the transferred data — no duplication, no loss. Peeking uses
//! [`ReclPeekRef`], a segment whose drop does not move the reader.
//!
//! # Async framework support
//!
//! The core is async-runtime agnostic. On top of it, `AsyncRead` /
//! `AsyncWrite` implementations are provided for:
//!
//! * compio (default feature `compio`): `compio::io::AsyncRead` /
//!   `compio::io::AsyncWrite`, plus the vectored kernel-handoff mode above;
//! * tokio (feature `tokio`): `tokio::io::AsyncRead` / `tokio::io::AsyncWrite`;
//! * smol & friends (feature `smol`): `futures_io::AsyncRead` /
//!   `futures_io::AsyncWrite`.
//!
//! # Safety
//!
//! A region handed to the runtime is referenced by `&'static` slices; it must
//! be returned via `put_back_send` / `put_back_recv` before the last
//! reference to the ring is dropped (asserted in debug builds). Likewise, a
//! live segment must not overlap a runtime reservation or another segment
//! (SPSC: one producer, one consumer).

mod abs_;
mod buffer_;
mod error_;
mod futures_;
mod reclaim_;
mod rx_;
mod state_;
mod tx_;

#[cfg(test)]
mod tests_;

pub use abs_::TrRingBuffer;
pub use error_::{RxError, TxError};
pub use futures_::{
    PeekAsync, PeekFuture, ReadAsync, ReadFuture, WaitFlushed, WaitFlushedFuture,
    WaitRxIdle, WaitRxIdleFuture, WaitTxIdle, WaitTxIdleFuture, WriteAsync, WriteFuture,
};
pub use reclaim_::{ReclPeekRef, ReclSliceMut, ReclSliceRef};
pub use rx_::RingRx;
pub use state_::{MAX_CAPACITY, RingBuffer};
pub use tx_::RingTx;

#[cfg(feature = "compio")]
mod compio_;

#[cfg(feature = "compio")]
pub use compio_::{RecvSlices, SendSlices};

#[cfg(feature = "tokio")]
mod tokio_;

#[cfg(feature = "smol")]
mod smol_;

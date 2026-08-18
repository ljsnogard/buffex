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
//!   (`RingBuffer::take_send_iovecs` / `RingBuffer::take_recv_iovecs`) and
//!   submits it to the kernel with a single `writev` / `readv` syscall; the
//!   region is returned with `put_back_send` / `put_back_recv`. This mode is
//!   currently **crate-internal** (used by `crate::unix_stream::BufferedUnixStream`,
//!   a compio adapter): the reservation hands out `&'static` slices over the
//!   ring's memory, so it is kept out of the public API until that ownership
//!   is reworked into a lifetime-bound guard.
//!
//! # Segments (abs_buff compatibility)
//!
//! The user-side borrows are **RingBuffer-specific** segments ([`ReclSliceMut`]
//! / [`ReclSliceRef`]) that implement the `abs_buff` traits (`TrBuffSegmView`,
//! `TrBuffSegmMut`, `TrBuffSegmRef`), so they work with the `abs_buff` pipe
//! machinery. Unlike the plain abs_buff segments, their internal representation
//! is an enum over **one or two physical slices**: when the writable/readable
//! region wraps around the buffer end it is handed out as two slices that are
//! logically one segment — so a producer asking for the whole free space in one
//! go is satisfied even though the space is physically fragmented.
//!
//! The segments' buffer **is the ring's own memory** — produced / consumed
//! data is written directly into the ring, with no intermediate copy. Segments
//! use the `abs_buff` *per-piece reclaim granularity*: when a segment drops it
//! commits to the ring exactly the amount it consumed (the writer position
//! advances by the units handed over, the reader position by the units taken),
//! so a mid-transfer cancellation leaves the positions right after the
//! transferred data — no duplication, no loss. Peeking uses [`ReclPeekRef`], a
//! segment whose drop does not move the reader.
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
//! # Public API surface
//!
//! The public methods of [`RingBuffer`] are deliberately minimal: `try_new`,
//! `capacity` / `data_size` / `free_size`, `is_tx_closed` / `is_rx_closed`
//! and `try_split_shared`. Everything that mutates the ring state (position
//! advancement, borrow primitives, kernel reservations, closing) is
//! crate-internal — users go through the halves ([`RingTx`] / [`RingRx`]) or
//! the async adapters, which carry the state-machine invariants. The raw
//! position / reservation primitives are kept out of the public API so they
//! cannot be misused to corrupt the ring.
//!
//! `try_split_shared` is guarded by a strong-reference-count check: the
//! caller must hold the sole reference (`strong_count == 1`) so that at most
//! one producer/consumer pair can ever be created from a shared handle —
//! cloning the handle before splitting could otherwise produce several pairs
//! and turn the SPSC ring into an unsound MPMC one. After a successful split,
//! an additional handle for a runtime-side task can be obtained from a half
//! via `RingTx::shared()` / `RingRx::shared()` (crate-internal).
//!
//! # Safety
//!
//! The internal borrow primitives hand out `&mut` views over the ring's
//! buffer memory (segments, iovec reservations) from a shared `&self`. The
//! ring does **not** enforce exclusivity between a live user segment and a
//! runtime reservation; the SPSC ownership protocol is a caller obligation,
//! documented at each primitive. A region handed to the runtime is referenced
//! by `&'static` slices and must be returned via `put_back_send` /
//! `put_back_recv` before the last reference to the ring is dropped
//! (asserted in debug builds).

mod abs_;
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

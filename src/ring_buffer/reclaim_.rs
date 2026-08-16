//! Reclaim functions for the segments borrowed from the ring buffer.
//!
//! A segment borrowed from the ring is an `abs_buff` segment ([`SegmMut`] /
//! [`SegmRef`]) whose buffer is the ring's own memory — no extra copies. When
//! the segment drops it reports the amount it actually *consumed* (the
//! `abs_buff` per-piece granularity), and the reclaim advances the ring
//! position by exactly that amount:
//!
//! * the writer reclaim ([`WriterReclaim`]) advances the writer position, so
//!   only the data the producer really handed over becomes visible to the
//!   reader (a mid-transfer cancellation leaves the writer position right
//!   after the transferred bytes);
//! * the reader reclaim ([`ReaderReclaim`]) advances the reader position, so
//!   only the data the consumer really took is freed up for the producer.
//!
//! Both reclaims hold a `&RingCore` (the packed positions + flags and the
//! waker slots) plus a `usize` copy of the capacity. `RingCore` is made only
//! of atomics, so the reclaims are unconditionally `Send + Sync` and satisfy
//! `abs_buff::buffer::TrReclaim`'s super-trait without restricting the ring's
//! element type.

use abs_buff::buffer::{SegmMut, SegmRef, TrReclaim};

use super::state_::RingCore;

/// A write segment borrowed from the ring buffer.
pub type ReclSliceMut<'a, T> = SegmMut<'a, T, WriterReclaim<'a>>;

/// A read segment borrowed from the ring buffer.
pub type ReclSliceRef<'a, T> = SegmRef<'a, T, ReaderReclaim<'a>>;

/// A peek segment borrowed from the ring buffer; dropping it does not move
/// the reader position.
pub type ReclPeekRef<'a, T> = SegmRef<'a, T, NoReclaim>;

/// Advances the writer position when a borrowed write segment drops.
pub struct WriterReclaim<'a> {
    core: &'a RingCore,
    cap: usize,
}

impl<'a> WriterReclaim<'a> {
    pub(super) const fn new(core: &'a RingCore, cap: usize) -> Self {
        WriterReclaim { core, cap }
    }
}

impl TrReclaim for WriterReclaim<'_> {
    fn reclaim(&self, amount: usize) -> usize {
        self.core.advance_write(self.cap, amount);
        0
    }
}

/// Advances the reader position when a borrowed read segment drops.
pub struct ReaderReclaim<'a> {
    core: &'a RingCore,
    cap: usize,
}

impl<'a> ReaderReclaim<'a> {
    pub(super) const fn new(core: &'a RingCore, cap: usize) -> Self {
        ReaderReclaim { core, cap }
    }
}

impl TrReclaim for ReaderReclaim<'_> {
    fn reclaim(&self, amount: usize) -> usize {
        self.core.advance_read(self.cap, amount);
        0
    }
}

/// A no-op reclaim used by peek segments: peeking borrows the readable region
/// without consuming it, so dropping the segment must not move the reader.
pub struct NoReclaim;

impl TrReclaim for NoReclaim {
    fn reclaim(&self, _amount: usize) -> usize {
        0
    }
}

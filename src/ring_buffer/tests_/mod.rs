//! Test suite for the ring buffer.
//!
//! * [`sync_`] — abs_buff segment semantics (partial segment borrows,
//!   reclaim-on-drop, wrap-around, peek, errors, closing, the `TrRingBuffer`
//!   trait), the vectored-IO kernel handoff, and the multithreaded SPSC pipe
//!   with no async runtime.
//! * [`scenario_`] — the *shared* pipe / kernel scenarios written only
//!   against the framework-agnostic core API, run under every selected
//!   framework's executor.
//! * [`frameworks_`] — per-framework tests of the `AsyncRead` / `AsyncWrite`
//!   trait implementations (compio default, tokio, smol) and a real compio
//!   kernel-IO test using the iovec (scatter/gather) handoff.

#[cfg(test)]
extern crate std;

#[cfg(test)]
use std::{boxed::Box, vec};

#[cfg(all(feature = "compio", unix))]
mod unix_stream_;

mod frameworks_;
mod mini_exec;
mod pipe_retry_;
mod smoke_tests_;
mod scenario_;
mod sync_;

use std::mem::MaybeUninit;
use std::sync::Arc;
use std::vec::Vec;

use crate::ring_buffer::{ReclSliceMut, ReclSliceRef, RingBuffer, RingRx, RingTx};

/// The byte written by the producer at position `i`.
#[inline]
pub(super) fn seq_byte(i: usize) -> u8 {
    (i % 256) as u8
}

/// The byte the kernel-sim fills at rx position `i`.
#[inline]
pub(super) fn pat_byte(i: usize) -> u8 {
    ((i * 7) % 256) as u8
}

/// A 16-byte ring buffer.
pub(super) const RING_CAP: usize = 16;

pub(super) type SharedRing = Arc<RingBuffer<Box<[u8]>>>;
pub(super) type SharedTx = RingTx<SharedRing, Box<[u8]>>;
pub(super) type SharedRx = RingRx<SharedRing, Box<[u8]>>;

/// Create a full-duplex-in-time ring (16-byte buffer) with the halves.
///
/// 设计思路：`try_split_shared` 要求以"唯一持有者"身份拆分（引用计数 == 1），
/// 否则已存在的其它 clone 可能被再次拆分，产生多对生产者/消费者，破坏 SPSC。
/// 因此这里把新建的 `Arc`（计数为 1）直接移入拆分；拆分成功后，调用方（例如
/// 运行时侧驱动场景）仍需要一个 `Arc`，从写半区的共享句柄 clone 得到——此时
/// 计数 >= 2，任何第二次拆分都会被拒绝，SPSC 依然成立。
pub(super) fn make_ring() -> (SharedRing, SharedTx, SharedRx) {
    let ring = Arc::new(
        RingBuffer::<Box<[u8]>>::try_new(vec![0u8; RING_CAP].into_boxed_slice()).unwrap(),
    );
    let (tx, rx) =
        RingBuffer::try_split_shared(
            ring,
            std::sync::Arc::strong_count,
            std::sync::Arc::weak_count,
        )
        .expect("新建 ring 的引用计数为 1，拆分必须成功");
    let ring = tx.shared().clone();
    (ring, tx, rx)
}

/// Create a shared ring without splitting (for the kernel-mode drivers).
pub(super) fn make_ring_shared() -> SharedRing {
    Arc::new(
        RingBuffer::<Box<[u8]>>::try_new(vec![0u8; RING_CAP].into_boxed_slice()).unwrap(),
    )
}

// ---------------------------------------------------------------------------
// segment consumption helpers
//
// The ring's segments are `abs_buff` segments with per-piece reclaim
// granularity: a segment commits to the ring exactly the amount that was
// *consumed* when it drops (the writer position advances by the units handed
// over, the reader position by the units taken). These helpers write / read
// through the move primitives so the consumed amount matches the data length.
// ---------------------------------------------------------------------------

/// Write `data` into a write segment — the segment's buffer is the ring's own
/// memory — and commit exactly `data.len()` units (the ring's writer position
/// advances by that amount when the segment drops).
pub(super) fn fill_segm(segm: &mut ReclSliceMut<'_, u8>, data: &[u8]) {
    assert!(
        data.len() <= segm.least_count(),
        "fill: len({}) > segm({})",
        data.len(),
        segm.least_count()
    );
    let mut staging: Vec<MaybeUninit<u8>> =
        data.iter().map(|&b| MaybeUninit::new(b)).collect();
    // SAFETY: the staging items are moved into the ring segment (bitwise
    // copies of `u8`), so nothing remains to drop in the staging buffer.
    let moved = unsafe { segm.move_items_from_buff(&mut staging) };
    assert_eq!(moved, data.len());
}

/// Consume `len` units from a read segment into a fresh `Vec<u8>`. The ring's
/// reader position advances by `len` when the segment drops.
pub(super) fn take_segm(segm: &mut ReclSliceRef<'_, u8>, len: usize) -> Vec<u8> {
    assert!(
        len <= segm.least_count(),
        "take: len({}) > segm({})",
        len,
        segm.least_count()
    );
    let mut dst: Vec<MaybeUninit<u8>> = Vec::with_capacity(len);
    dst.resize_with(len, MaybeUninit::uninit);
    // SAFETY: the items moved out of the ring segment are plain `u8`
    // (bitwise copies); the destination buffer owns them afterwards.
    let moved = unsafe { segm.move_items_to_buff(&mut dst) };
    assert_eq!(moved, len);
    dst[..len].iter().map(|m| unsafe { m.assume_init_read() }).collect()
}

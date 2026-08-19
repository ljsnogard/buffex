//! Smoke tests that exercise the ring buffer exclusively through its
//! user-facing public API.
//!
//! These tests do not inspect internal positions or call crate-internal
//! helpers. They construct a ring with the public `try_new` /
//! `try_split_shared` entry points, drive producers/consumers through the
//! public `RingTx` / `RingRx` methods, and verify only externally visible
//! effects: written bytes come back in order, the ring reports full/empty
//! correctly, and multi-threaded smoke runs terminate without corruption.

use std::{boxed::Box, mem::MaybeUninit, sync::Arc, thread, vec, vec::Vec};

use crate::ring_buffer::{
    ReclSliceMut, ReclSliceRef, RingBuffer, RingRx, RingTx, RxError, TxError,
};

type Tx = RingTx<Arc<RingBuffer<Box<[u8]>>>, Box<[u8]>, u8>;
type Rx = RingRx<Arc<RingBuffer<Box<[u8]>>>, Box<[u8]>, u8>;

/// Create a ring through the public construction/split API.
fn make_ring(cap: usize) -> (Tx, Rx) {
    let ring = Arc::new(
        RingBuffer::<Box<[u8]>>::try_new(vec![0u8; cap].into_boxed_slice())
            .expect("valid ring capacity"),
    );
    RingBuffer::try_split_shared(ring, Arc::strong_count, Arc::weak_count)
        .expect("fresh Arc must split exactly once")
}

/// Wait until at least `len` free units are available, then borrow them.
fn wait_write(tx: &mut Tx, len: usize) -> ReclSliceMut<'_, u8> {
    loop {
        if tx.free_size() >= len {
            return tx.try_write_at_most(len).unwrap_or_else(|e| {
                panic!("expected writable space, got {e:?}")
            });
        }
        thread::yield_now();
    }
}

/// Wait until at least `len` readable units are available, then borrow them.
fn wait_read(rx: &mut Rx, len: usize) -> ReclSliceRef<'_, u8> {
    loop {
        if rx.data_size() >= len {
            return rx.try_read_at_most(len).unwrap_or_else(|e| {
                panic!("expected readable data, got {e:?}")
            });
        }
        thread::yield_now();
    }
}

/// Fill a write segment using only public segment APIs.
fn fill_segm(segm: &mut ReclSliceMut<'_, u8>, data: &[u8]) {
    assert!(
        data.len() <= segm.least_count(),
        "fill: len({}) > segm({})",
        data.len(),
        segm.least_count()
    );
    let mut staging: Vec<MaybeUninit<u8>> =
        data.iter().map(|&b| MaybeUninit::new(b)).collect();
    // SAFETY: moving plain `u8` bytes out of the staging buffer is a bitwise
    // copy; nothing in the staging buffer needs dropping afterwards.
    let moved = unsafe { segm.move_items_from_buff(&mut staging) };
    assert_eq!(moved, data.len());
}

/// Read exactly `len` bytes out of a read segment using public segment APIs.
fn take_segm(segm: &mut ReclSliceRef<'_, u8>, len: usize) -> Vec<u8> {
    assert!(
        len <= segm.least_count(),
        "take: len({}) > segm({})",
        len,
        segm.least_count()
    );
    let mut dst: Vec<MaybeUninit<u8>> = Vec::with_capacity(len);
    dst.resize_with(len, MaybeUninit::uninit);
    // SAFETY: the bytes moved out of the ring segment are plain `u8`; the
    // destination buffer owns them afterwards.
    let moved = unsafe { segm.move_items_to_buff(&mut dst) };
    assert_eq!(moved, len);
    dst[..len]
        .iter()
        .map(|m| unsafe { m.assume_init_read() })
        .collect()
}

/// Write a whole slice to the ring, waiting for space as needed.
fn write_all(tx: &mut Tx, data: &[u8]) {
    let mut off = 0usize;
    while off < data.len() {
        let mut segm = wait_write(tx, data.len() - off);
        let n = segm.least_count();
        assert!(n > 0);
        fill_segm(&mut segm, &data[off..off + n]);
        off += n;
    }
}

/// Read exactly `len` bytes from the ring, waiting for data as needed.
fn read_all(rx: &mut Rx, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let mut segm = wait_read(rx, len - out.len());
        let n = segm.least_count();
        assert!(n > 0);
        out.extend(take_segm(&mut segm, n));
    }
    out
}

fn seq_byte(i: usize) -> u8 {
    (i % 256) as u8
}

/// Producer writes `N, N-1, ..., 1`; consumer reads `1, 2, ..., N`.
///
/// This is the canonical external-API smoke test: two threads, no internal
/// positions, and the final verification is simply that all bytes arrive in
/// the original order.
#[test]
fn producer_decreasing_consumer_increasing_threaded() {
    const N: usize = 16;
    const CAP: usize = 64;
    let total = N * (N + 1) / 2;

    let (mut tx, mut rx) = make_ring(CAP);

    let writer = thread::spawn(move || {
        let mut off = 0usize;
        for k in (1..=N).rev() {
            let data: Vec<u8> = (off..off + k).map(seq_byte).collect();
            let mut segm = wait_write(&mut tx, k);
            assert_eq!(
                segm.least_count(),
                k,
                "producer should get exactly {k}"
            );
            fill_segm(&mut segm, &data);
            drop(segm);
            off += k;
        }
        tx.close();
    });

    let reader = thread::spawn(move || {
        let mut off = 0usize;
        for k in 1..=N {
            let mut segm = wait_read(&mut rx, k);
            assert_eq!(
                segm.least_count(),
                k,
                "consumer should get exactly {k}"
            );
            let got = take_segm(&mut segm, k);
            for (i, b) in got.iter().enumerate() {
                assert_eq!(*b, seq_byte(off + i), "data mismatch at {off}+{i}");
            }
            off += k;
        }
        assert_eq!(off, total);
        rx.close();
    });

    writer.join().expect("producer thread panicked");
    reader.join().expect("consumer thread panicked");
}

/// Single-element round trip through the public API.
#[test]
fn single_byte_roundtrip() {
    let (mut tx, mut rx) = make_ring(4);

    let mut segm = tx.try_write_at_most(1).expect("write one");
    fill_segm(&mut segm, &[42]);
    drop(segm);

    assert_eq!(rx.data_size(), 1);
    let mut segm = rx.try_read_at_most(1).expect("read one");
    let got = take_segm(&mut segm, 1);
    drop(segm);

    assert_eq!(got, vec![42]);
    assert!(matches!(rx.try_read_at_most(1), Err(RxError::Drained(_))));
}

/// Full ring rejects writes; reading frees space and allows the next write.
#[test]
fn full_then_read_then_write_again() {
    let (mut tx, mut rx) = make_ring(4);
    write_all(&mut tx, &[1, 2, 3]);
    assert_eq!(tx.free_size(), 0);
    assert!(matches!(tx.try_write_at_most(1), Err(TxError::Stuffed(_))));

    let first = read_all(&mut rx, 1);
    assert_eq!(first, vec![1]);
    assert_eq!(tx.free_size(), 1);

    let mut segm = tx.try_write_at_most(1).expect("space after read");
    fill_segm(&mut segm, &[4]);
    drop(segm);

    let rest = read_all(&mut rx, 3);
    assert_eq!(rest, vec![2, 3, 4]);
}

/// The ring's wrapped write/read path: after a partial read, a write crosses
/// the end of the backing buffer and the reader still observes linear order.
#[test]
fn wrapped_write_read_smoke() {
    let (mut tx, mut rx) = make_ring(8);

    write_all(&mut tx, &[0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(tx.data_size(), 7);

    let first = read_all(&mut rx, 2);
    assert_eq!(first, vec![0, 1]);
    assert_eq!(tx.data_size(), 5);

    // This write should wrap around the end of the buffer.
    let mut segm = tx.try_write_at_most(2).expect("wrapped write");
    assert_eq!(segm.least_count(), 2);
    fill_segm(&mut segm, &[7, 8]);
    drop(segm);

    let rest = read_all(&mut rx, 7);
    assert_eq!(rest, vec![2, 3, 4, 5, 6, 7, 8]);
    assert!(matches!(rx.try_read_at_most(1), Err(RxError::Drained(_))));
}

/// After the producer closes and the consumer drains the ring, the public
/// drained-closing state becomes visible and further reads report closing.
#[test]
fn close_and_drain_smoke() {
    let (mut tx, mut rx) = make_ring(4);
    write_all(&mut tx, &[1, 2, 3]);
    tx.close();

    let got = read_all(&mut rx, 3);
    assert_eq!(got, vec![1, 2, 3]);

    rx.close();
    assert!(rx.is_drained_closing());
    assert!(matches!(rx.try_read_at_most(1), Err(RxError::Closing)));
}

/// Peeking is public and must not consume data.
#[test]
fn peek_does_not_consume_external_api() {
    let (mut tx, mut rx) = make_ring(8);
    write_all(&mut tx, &[10, 11, 12]);

    let peeked = rx.try_peek().expect("peek should work");
    let slices: Vec<&[u8]> = peeked.iter_slices().collect();
    assert_eq!(slices, vec![&[10u8, 11, 12][..]]);
    drop(peeked);

    assert_eq!(rx.data_size(), 3, "peek must not consume");
    let got = read_all(&mut rx, 3);
    assert_eq!(got, vec![10, 11, 12]);
}

/// A slightly larger multi-threaded load with varying chunk sizes.
#[test]
fn varying_chunk_multithreaded_load() {
    const TOTAL: usize = 2000;
    const CAP: usize = 64;

    let (mut tx, mut rx) = make_ring(CAP);

    let writer = thread::spawn(move || {
        let mut off = 0usize;
        let mut k = 1usize;
        while off < TOTAL {
            let n = core::cmp::min(k, TOTAL - off);
            let data: Vec<u8> = (off..off + n).map(seq_byte).collect();
            let mut segm = wait_write(&mut tx, n);
            assert!(segm.least_count() >= n);
            fill_segm(&mut segm, &data);
            drop(segm);
            off += n;
            k = if k == 17 { 1 } else { k + 1 };
        }
        tx.close();
    });

    let reader = thread::spawn(move || {
        let mut off = 0usize;
        let mut k = 13usize;
        while off < TOTAL {
            let n = core::cmp::min(k, TOTAL - off);
            let mut segm = wait_read(&mut rx, n);
            let len = segm.least_count();
            let got = take_segm(&mut segm, len);
            for (i, b) in got.iter().enumerate() {
                assert_eq!(*b, seq_byte(off + i), "load mismatch at {off}+{i}");
            }
            off += len;
            k = if k == 29 { 1 } else { k + 1 };
        }
        rx.close();
    });

    writer.join().expect("load producer panicked");
    reader.join().expect("load consumer panicked");
}

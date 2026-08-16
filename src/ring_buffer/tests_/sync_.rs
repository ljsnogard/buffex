//! Synchronous tests: abs_buff segment semantics, wrap-around, error
//! handling, the vectored-IO kernel handoff, the `TrRingBuffer` trait, and
//! the multithreaded SPSC pipe without any runtime.

use std::{
    boxed::Box,
    sync::Arc,
    vec,
    vec::Vec,
};

use abs_buff::Demand;

use crate::ring_buffer::{RxError, TrRingBuffer, TxError};

use super::{fill_segm, make_ring, make_ring_shared, pat_byte, seq_byte, take_segm, RING_CAP};

/// Write `[0..8)` through partial contiguous borrows and read them back,
/// including across the wrap-around.
#[test]
fn segm_borrow_roundtrip_and_wrap() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (_ring, mut tx, mut rx) = make_ring();

    // partial writes: 3 then 5 (wp: 0 -> 3 -> 8)
    let mut segm = tx.try_write(3).expect("write 3");
    assert_eq!(segm.least_count(), 3);
    fill_segm(&mut segm, &(0..3).map(seq_byte).collect::<Vec<_>>());
    drop(segm);
    assert_eq!(tx.data_size(), 3);

    let mut segm = tx.try_write(5).expect("write 5");
    assert_eq!(segm.least_count(), 5);
    fill_segm(&mut segm, &(3..8).map(seq_byte).collect::<Vec<_>>());
    drop(segm);
    assert_eq!(tx.data_size(), 8);

    // partial reads: 4 then 4 (rp: 0 -> 4 -> 8)
    let mut segm = rx.try_read(4).expect("read 4");
    assert_eq!(segm.least_count(), 4);
    let got = take_segm(&mut segm, 4);
    for (i, b) in got.iter().enumerate() {
        assert_eq!(*b, seq_byte(i));
    }
    drop(segm);

    let mut segm = rx.try_read(4).expect("read 4 more");
    assert_eq!(segm.least_count(), 4);
    let got = take_segm(&mut segm, 4);
    for (i, b) in got.iter().enumerate() {
        assert_eq!(*b, seq_byte(4 + i));
    }
    drop(segm);
    assert_eq!(tx.data_size(), 0);

    // Now force the writer position to wrap: fill the ring again so that
    // `wp` wraps past the end of the buffer. The writable region is
    // contiguous, so the wrap is reached through repeated borrows.
    let mut total = 0usize;
    while total < RING_CAP - 1 {
        let mut segm = tx.try_write(RING_CAP - 1 - total).expect("fill");
        assert!(segm.least_count() > 0);
        let len = segm.least_count();
        fill_segm(&mut segm, &(0..len).map(|i| seq_byte(100 + total + i)).collect::<Vec<_>>());
        drop(segm);
        total += len;
    }
    assert!(tx.ring().writer_pos() < RING_CAP);
    assert_eq!(tx.data_size(), RING_CAP - 1);
    // full: one slot gap
    assert!(matches!(tx.try_write(1), Err(TxError::Stuffed(_))));

    // read everything back (this may wrap at the reader side too)
    let mut off = 0usize;
    loop {
        let segm = match rx.try_read(7) {
            Ok(s) => s,
            Err(RxError::Drained(_)) => break,
            Err(e) => panic!("read failed: {e:?}"),
        };
        let len = segm.least_count();
        let mut segm = segm;
        let got = take_segm(&mut segm, len);
        for (i, b) in got.iter().enumerate() {
            assert_eq!(*b, seq_byte(100 + off + i));
        }
        drop(segm);
        off += len;
    }
    assert_eq!(off, RING_CAP - 1);
}

/// `try_peek` borrows data without consuming it.
#[test]
fn peek_does_not_consume() {
    let (_ring, mut tx, mut rx) = make_ring();
    let mut segm = tx.try_write(4).expect("write");
    fill_segm(&mut segm, &[10, 11, 12, 13]);
    drop(segm);

    let segm = rx.try_peek().expect("peek");
    let slice = segm.iter_slices().expect("peek data");
    assert_eq!(slice, &[10u8, 11, 12, 13]);
    drop(segm); // no reclaim

    // still fully readable
    let segm = rx.try_peek().expect("peek again");
    assert_eq!(segm.least_count(), 4);
    drop(segm);

    let mut segm = rx.try_read(16).expect("read all");
    assert_eq!(segm.least_count(), 4);
    let got = take_segm(&mut segm, 4);
    assert_eq!(got, vec![10, 11, 12, 13]);
    drop(segm);

    // now drained
    assert!(matches!(rx.try_read(1), Err(RxError::Drained(_))));
}

/// Error semantics: `Stuffed` when full, `Drained` when empty, `Closing`
/// after close.
#[test]
fn error_semantics() {
    let (_ring, mut tx, mut rx) = make_ring();

    // empty
    assert!(matches!(rx.try_read(1), Err(RxError::Drained(_))));

    // fill to capacity - 1
    let mut segm = tx.try_write(RING_CAP).expect("fill");
    let n = segm.least_count();
    assert_eq!(n, RING_CAP - 1, "one slot is always unused");
    fill_segm(&mut segm, &vec![0u8; n]);
    drop(segm);
    assert!(matches!(tx.try_write(1), Err(TxError::Stuffed(_))));

    // drain
    let mut segm = rx.try_read(RING_CAP).expect("read all");
    let n = segm.least_count();
    assert_eq!(n, RING_CAP - 1);
    take_segm(&mut segm, n);
    drop(segm);
    assert!(matches!(rx.try_read(1), Err(RxError::Drained(_))));

    // closing
    rx.close();
    assert!(matches!(rx.try_read(1), Err(RxError::Closing)));
    assert!(matches!(rx.try_peek(), Err(RxError::Closing)));

    tx.close();
    // The `Closing` error is only reported when the ring is full as well
    // (matching the documented semantics: "the output end has closed and the
    // buffer is already full").
    let mut segm = tx.try_write(1).expect("write while closing still has space");
    fill_segm(&mut segm, &[0u8]);
    drop(segm);
}

/// The `TrRingBuffer` trait: the ring is a direct user pipe (write through
/// the tx half, read through the rx half).
#[test]
fn tr_ring_buffer_trait() {
    use abs_buff::{TrBuffTryPeek, TrBuffTryRead, TrBuffTryWrite};

    let mut ring = crate::ring_buffer::RingBuffer::<Box<[u8]>>::try_new(
        vec![0u8; RING_CAP].into_boxed_slice(),
    )
    .unwrap();

    assert_eq!(ring.capacity(), RING_CAP);
    assert_eq!(ring.data_size(), 0);

    let Some((mut tx, mut rx)) = ring.try_split_io() else {
        panic!("try_split_io returned None");
    };

    // write through the abs_buff trait interface
    let demand = Demand::less_than(4);
    let some = TrBuffTryWrite::try_write(&mut tx, &demand);
    let Some(mut segm) = some.pick_left() else {
        panic!("TrBuffTryWrite::try_write failed")
    };
    fill_segm(&mut segm, &(0..4).map(seq_byte).collect::<Vec<_>>());
    drop(segm);
    assert_eq!(tx.data_size(), 4);

    // read through the abs_buff trait interface
    let demand = Demand::less_than(16);
    let some = TrBuffTryRead::try_read(&mut rx, &demand);
    let Some(segm) = some.pick_left() else {
        panic!("TrBuffTryRead::try_read failed")
    };
    let n = segm.least_count();
    let mut segm = segm;
    let got: Vec<u8> = take_segm(&mut segm, n);
    drop(segm);
    assert_eq!(got, vec![0, 1, 2, 3]);

    // write 4 more and peek through the abs_buff trait interface
    let demand = Demand::less_than(4);
    let some = TrBuffTryWrite::try_write(&mut tx, &demand);
    let Some(mut segm) = some.pick_left() else {
        panic!("write 4 more failed")
    };
    fill_segm(&mut segm, &[4, 5, 6, 7]);
    drop(segm);
    let some = TrBuffTryPeek::try_peek(&mut rx);
    let Some(segm) = some.pick_left() else {
        panic!("TrBuffTryPeek::try_peek failed")
    };
    let slice = segm.iter_slices().expect("peek data");
    assert_eq!(slice[0], 4);
    drop(segm);

    drop(tx);
    drop(rx);
    assert_eq!(ring.capacity(), RING_CAP);
}

/// The vectored-IO kernel handoff: contiguous and wrapped iovec pairs.
#[test]
fn iovec_take_put() {
    let ring = make_ring_shared();
    let mut tx = RingTxShim(&ring);

    // write 6 bytes
    let mut segm = tx.try_write(6).unwrap();
    let n = segm.least_count();
    fill_segm(&mut segm, &(0..n).map(|i| (10 + i) as u8).collect::<Vec<_>>());
    drop(segm);

    // take the send iovecs (contiguous: one non-empty slice)
    let (a, b) = ring.take_send_iovecs().expect("send iovecs");
    assert_eq!(a, &[10, 11, 12, 13, 14, 15]);
    assert!(b.is_empty());
    ring.put_back_send(6);
    assert_eq!(ring.data_size(), 0);

    // fill the ring to force a wrap, then take the wrapped send iovecs
    let mut off = 0usize;
    while off < RING_CAP - 1 {
        let mut segm = tx.try_write(RING_CAP - off).unwrap();
        let len = segm.least_count();
        fill_segm(&mut segm, &(0..len).map(|i| (20 + off + i) as u8).collect::<Vec<_>>());
        drop(segm);
        off += len;
    }
    let (a, b) = ring.take_send_iovecs().expect("wrapped send iovecs");
    assert!(!a.is_empty() && !b.is_empty(), "the region must wrap");
    // the two slices concatenate to the whole readable region
    let mut all = Vec::new();
    all.extend_from_slice(a);
    all.extend_from_slice(b);
    assert_eq!(all.len(), RING_CAP - 1);
    for (i, x) in all.iter().enumerate() {
        assert_eq!(*x, 20 + i as u8);
    }
    ring.put_back_send(all.len());

    // take the recv iovecs (writable region), fill them, put back
    let (a, b) = ring.take_recv_iovecs().expect("recv iovecs");
    let n = a.len() + b.len();
    assert_eq!(n, RING_CAP - 1);
    for (i, slot) in a.iter_mut().enumerate() {
        *slot = pat_byte(i);
    }
    for (i, slot) in b.iter_mut().enumerate() {
        *slot = pat_byte(a.len() + i);
    }
    ring.put_back_recv(n);
    assert_eq!(ring.data_size(), n);
    let (c, _) = ring.take_send_iovecs().unwrap();
    assert_eq!(&c[..4], &[0, 7, 14, 21]);
    ring.put_back_send(0);
}

/// The runtime reservation blocks the opposite user end (kernel mode).
#[test]
fn kernel_reservation_blocks_user() {
    let ring = make_ring_shared();
    let mut tx = RingTxShim(&ring);
    let mut rx = RingRxShim(&ring);

    let mut segm = tx.try_write(4).unwrap();
    fill_segm(&mut segm, &[1, 2, 3, 4]);
    drop(segm);

    let (_a, _b) = ring.take_send_iovecs().unwrap();
    // the user reader is blocked while the kernel owns the region
    assert!(matches!(rx.try_read(1), Err(RxError::Drained(_))));
    ring.put_back_send(4);
    // the kernel wrote the data out: the ring is drained again
    assert!(matches!(rx.try_read(1), Err(RxError::Drained(_))));

    // the runtime reserves the writable region for a kernel read; the user
    // writer is blocked meanwhile
    let (a, _b) = ring.take_recv_iovecs().unwrap();
    a[0] = 42;
    a[1] = 43;
    a[2] = 44;
    assert!(matches!(tx.try_write(1), Err(TxError::Stuffed(_))));
    ring.put_back_recv(3);

    // now the user can read the received data
    let mut segm = rx.try_read(3).unwrap();
    let got = take_segm(&mut segm, 3);
    assert_eq!(got, vec![42, 43, 44]);
    drop(segm);
}

/// A write-only ring: the tx half alone (used by the kernel-mode drivers).
#[test]
fn split_borrowed_halves() {
    let mut ring = crate::ring_buffer::RingBuffer::<Box<[u8]>>::try_new(
        vec![0u8; RING_CAP].into_boxed_slice(),
    )
    .unwrap();
    let (mut tx, mut rx) = ring.split();
    let mut segm = tx.try_write(2).unwrap();
    fill_segm(&mut segm, &[1, 2]);
    drop(segm);
    let mut segm = rx.try_read(2).unwrap();
    let got = take_segm(&mut segm, 2);
    assert_eq!(got, vec![1, 2]);
    drop(segm);
}

/// The ring is `Send + Sync` when the storage and element are.
#[test]
fn ring_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<crate::ring_buffer::RingBuffer<Box<[u8]>>>>();
}

/// Multithreaded SPSC pipe: one writer thread, one reader thread, no runtime.
#[test]
fn spsc_multithread() {
    let _ = env_logger::builder().is_test(true).try_init();
    const TOTAL: usize = 500;

    let (_ring, mut tx, mut rx) = make_ring();

    let writer = std::thread::spawn(move || {
        let mut off = 0usize;
        while off < TOTAL {
            let res = tx.try_write(5);
            let mut progressed = false;
            match res {
                Ok(mut segm) => {
                    let len = segm.least_count();
                    fill_segm(&mut segm, &(0..len).map(|i| seq_byte(off + i)).collect::<Vec<_>>());
                    drop(segm);
                    off += len;
                    progressed = true;
                }
                Err(_) => {}
            }
            if !progressed {
                std::thread::yield_now();
            }
        }
        tx.close();
    });

    let reader = std::thread::spawn(move || {
        let mut off = 0usize;
        loop {
            if off >= TOTAL {
                break;
            }
            match rx.try_read(9) {
                Ok(segm) => {
                    let len = segm.least_count();
                    let mut segm = segm;
                    let got = take_segm(&mut segm, len);
                    for (i, b) in got.iter().enumerate() {
                        assert_eq!(*b, seq_byte(off + i), "reader mismatch at {off}+{i}");
                    }
                    off += len;
                    drop(segm);
                }
                Err(_) => std::thread::yield_now(),
            }
        }
        rx.close();
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

/// Thin shims so the kernel-mode tests can write/read through the shared
/// ring without holding halves.
struct RingTxShim<'a>(&'a super::SharedRing);
impl<'a> RingTxShim<'a> {
    fn try_write(&mut self, n: usize) -> Result<crate::ring_buffer::ReclSliceMut<'_, u8>, TxError<usize>> {
        self.0.try_write_at(n).map(|(s, t)| self.0.write_segm(s, t))
    }
}
struct RingRxShim<'a>(&'a super::SharedRing);
impl<'a> RingRxShim<'a> {
    fn try_read(&mut self, n: usize) -> Result<crate::ring_buffer::ReclSliceRef<'_, u8>, RxError<usize>> {
        self.0.try_read_at(n).map(|(s, t)| self.0.read_segm(s, t))
    }
}

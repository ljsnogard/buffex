//! Cancellation-retry semantics of `abs_buff::pipelining::Pipe` with the
//! per-piece reclaim granularity of the `abs_buff` segments: a mid-transfer cancellation
//! must leave the reader position exactly after the data that was actually
//! written, so retrying transfers the rest — no duplication, no loss.

use std::{
    boxed::Box,
    future::Future,
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    task::{Context, Poll, Waker},
    vec,
    vec::Vec,
};

use abs_buff::{
    pipelining::{PipeJoin, PipeJoinIoResult},
    x_deps::abs_cancel,
};

use abs_cancel::{TrCancellationToken, TrMayCancel};

use crate::ring_buffer::{RingBuffer, RingRx, RingTx};

use super::{mini_exec::MiniExec, fill_segm, take_segm};

type SharedRing = Arc<RingBuffer<Box<[u8]>>>;
type SharedTx = RingTx<SharedRing, Box<[u8]>>;
type SharedRx = RingRx<SharedRing, Box<[u8]>>;

/// A cancellation token whose flag is set in advance.
#[derive(Clone)]
struct FlagToken(Arc<AtomicBool>);

impl FlagToken {
    fn new(cancelled: bool) -> Self {
        FlagToken(Arc::new(AtomicBool::new(cancelled)))
    }
}

impl TrCancellationToken for FlagToken {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn can_be_cancelled(&self) -> bool {
        true
    }
    fn try_spawn_child_token(&mut self) -> impl core::ops::Try<Output: TrCancellationToken> {
        Option::Some(self.clone())
    }
    fn cancellation(&mut self) -> impl core::future::IntoFuture {
        core::future::pending::<()>()
    }
}

/// 创建一个容量为 `cap` 的 ring，并拆出写/读半区。
///
/// 设计思路：`try_split_shared` 要求以唯一持有者身份拆分（引用计数 == 1），
/// 否则已有 clone 可能被再次拆分出第二对生产者/消费者，破坏 SPSC。这里把
/// 新建的 `Arc`（计数 1）直接移入拆分；调用方（如读取侧校验）仍需要的
/// `Arc` 在拆分后从写半区 clone 得到（计数 >= 2，第二次拆分会被拒绝）。
fn make_ring(cap: usize) -> (SharedRing, SharedTx, SharedRx) {
    let ring =
        Arc::new(RingBuffer::<Box<[u8]>>::try_new(vec![0u8; cap].into_boxed_slice()).unwrap());
    let (tx, rx) = RingBuffer::
        try_split_shared(ring, Arc::strong_count, Arc::weak_count)
        .expect("新建 ring 的引用计数为 1，拆分必须成功");
    let ring = tx.shared().clone();
    (ring, tx, rx)
}

/// Fill the tx end with `(i % 256) as u8`, committing each borrow fully.
fn fill_pattern(tx: &mut SharedTx, total: usize) {
    let mut off = 0usize;
    while off < total {
        // "borrow N commits N": never demand more than what remains to write
        let mut segm = tx
            .try_write_at_most(core::cmp::min(1009, total - off))
            .expect("fill try_write");
        let len = segm.least_count();
        fill_segm(
            &mut segm,
            &(0..len).map(|i| (off + i) as u8).collect::<Vec<_>>(),
        );
        drop(segm);
        off += len;
    }
}

/// Drain everything currently readable at the rx end into `collected`.
fn drain_available(rx: &mut SharedRx, collected: &mut Vec<u8>) {
    while let Ok(segm) = rx.try_read_at_most(1009) {
        let len = segm.least_count();
        let mut segm = segm;
        let got = take_segm(&mut segm, len);
        collected.extend_from_slice(&got);
        drop(segm);
    }
}

/// Run one Pipe round with the given token on the hand-rolled executor.
fn run_pipe_round(
    write_tx: &mut SharedTx,
    read_rx: &mut SharedRx,
    token: &mut FlagToken,
) -> PipeJoinIoResult<SharedTx, SharedRx, u8> {
    let mut exec = MiniExec::new();
    exec.block_on(async {
        let mut pipe = PipeJoin::new(write_tx, read_rx);
        pipe.pipe_async().may_cancel_with(token).await
    })
}

/// A mid-transfer cancellation followed by retries must transfer the whole
/// payload exactly once: the reader position after each failure lands right
/// after the bytes that were actually written.
#[test]
fn pipe_cancel_retry_no_dup_no_loss() {
    const TOTAL: usize = 100;

    // the read ring must hold the whole payload; the write ring is small so
    // the pipe fills it after one piece and the cancellation fires
    let (read_ring, mut read_tx, mut read_rx) = make_ring(128);
    fill_pattern(&mut read_tx, TOTAL);
    drop(read_tx);
    // the read side is finished after filling: no more data will ever come
    read_ring.close_rx();

    let (_write_ring, mut write_tx, mut write_rx) = make_ring(16);

    let mut collected = Vec::new();
    let mut transferred = 0usize;
    let mut rounds = 0usize;
    loop {
        let mut token = FlagToken::new(true); // cancelled before the round
        let result = run_pipe_round(&mut write_tx, &mut read_rx, &mut token);
        match result {
            PipeJoinIoResult::TxErr { count, .. } => {
                assert!(count > 0, "round {rounds} transferred nothing");
                transferred += count;
            }
            PipeJoinIoResult::RxDrained(count) => {
                transferred += count;
                break;
            }
            _ => panic!("unexpected pipe result in round {rounds}"),
        }
        drain_available(&mut write_rx, &mut collected);
        rounds += 1;
        assert!(rounds < 32, "too many rounds");
    }
    drain_available(&mut write_rx, &mut collected);

    assert_eq!(
        transferred, TOTAL,
        "the pipe must report exactly {TOTAL} bytes"
    );
    assert_eq!(
        collected.len(),
        TOTAL,
        "exactly {TOTAL} bytes must be delivered"
    );
    for (i, b) in collected.iter().enumerate() {
        assert_eq!(*b, (i % 256) as u8, "payload mismatch at {i}");
    }
}

/// The same scenario, but the failure is a write-side close instead of a
/// cancellation: a close arriving mid-segment must report exactly the bytes
/// transferred before the close, leave the reader right after them, and keep
/// the untransferred data intact for a retry on a fresh write ring.
#[test]
fn pipe_close_midway_retry_no_dup_no_loss() {
    const TOTAL: usize = 100;

    let (read_ring, mut read_tx, mut read_rx) = make_ring(128);
    fill_pattern(&mut read_tx, TOTAL);
    drop(read_tx);
    read_ring.close_rx();

    let (write_ring, mut write_tx, mut write_rx) = make_ring(16);
    let mut collected = Vec::new();

    // round 1: a live token. Drive the pipe by hand: the first poll writes
    // one piece (15 bytes) and parks on the full writer; then close the
    // write ring while it is parked. The next poll must fail with `TxErr`
    // reporting exactly the transferred piece.
    let count = {
        let mut token = FlagToken::new(false);
        let mut pipe = PipeJoin::new(&mut write_tx, &mut read_rx);
        let fut = pipe.pipe_async().may_cancel_with(&mut token);
        let mut fut = Box::pin(fut.into_future());
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "the pipe must park on the full writer after one piece"
        );
        write_ring.close_tx();
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(PipeJoinIoResult::TxErr { count, .. }) => count,
            Poll::Ready(_) => panic!("expected TxErr after close_tx"),
            Poll::Pending => panic!("the pipe must not stay parked after close_tx"),
        }
    };
    assert_eq!(count, 15, "exactly one write piece must be transferred");
    assert_eq!(
        read_ring.data_size() + count,
        TOTAL,
        "the untransferred data must remain readable for a retry"
    );
    drain_available(&mut write_rx, &mut collected);
    assert_eq!(collected.len(), count);
    for (i, b) in collected.iter().enumerate() {
        assert_eq!(*b, (i % 256) as u8, "payload mismatch at {i}");
    }

    // retry on a fresh write ring: the reader resumes after the transferred
    // piece, so the remaining payload arrives exactly once
    let (_write_ring2, mut write_tx2, mut write_rx2) = make_ring(16);
    let mut collected2 = Vec::new();
    let mut transferred = count;
    let mut rounds = 0usize;
    loop {
        let mut token = FlagToken::new(true); // cancelled: one piece per round
        let result = run_pipe_round(&mut write_tx2, &mut read_rx, &mut token);
        match result {
            PipeJoinIoResult::TxErr { count, .. } => {
                assert!(count > 0, "round {rounds} transferred nothing");
                transferred += count;
            }
            PipeJoinIoResult::RxDrained(count) => {
                transferred += count;
                break;
            }
            _ => panic!("unexpected pipe result in round {rounds}"),
        }
        drain_available(&mut write_rx2, &mut collected2);
        rounds += 1;
        assert!(rounds < 32, "too many rounds");
    }
    drain_available(&mut write_rx2, &mut collected2);

    assert_eq!(
        transferred, TOTAL,
        "the pipe must report exactly {TOTAL} bytes in total"
    );
    assert_eq!(collected2.len(), TOTAL - count, "no duplication, no loss");
    for (i, b) in collected2.iter().enumerate() {
        assert_eq!(
            *b,
            ((count + i) % 256) as u8,
            "payload mismatch at {count}+{i}"
        );
    }
}

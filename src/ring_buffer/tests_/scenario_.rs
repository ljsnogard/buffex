//! The *shared* scenarios used to prove that the same logic runs under every
//! selected async framework. They only use the framework-agnostic core API
//! ([`RingTx::write_async`], [`RingRx::read_async`], the vectored kernel
//! handoff), so the only framework-specific part is the executor
//! (block_on + spawn / spawn_blocking).

use std::{
    boxed::Box,
    pin::Pin,
    sync::Arc,
    vec::Vec,
};

use super::{
    fill_segm, make_ring, make_ring_shared, pat_byte, seq_byte, take_segm, SharedRing, SharedRx,
    SharedTx,
};

/// The producer of the pipe scenario: writes `seq_byte`-data into the tx end
/// in small chunks, then closes the tx end.
pub(super) async fn producer_core(mut tx: SharedTx, total: usize) {
    let mut off = 0usize;
    while off < total {
        let take = core::cmp::min(7, total - off);
        let x = tx.write_at_most_async(take).await;
        let Some(mut segm) = x.pick_left() else {
            panic!("[producer] write_async failed");
        };
        let segm_len = segm.least_count();
        assert!(segm_len <= take, "segm_len({segm_len}) > take({take})");
        fill_segm(&mut segm, &(0..segm_len).map(|i| seq_byte(off + i)).collect::<Vec<_>>());
        drop(segm);
        off += segm_len;
    }
    tx.close();
}

/// The consumer of the pipe scenario: reads data from the rx end until
/// `expected` bytes are consumed, then closes the rx end. The expected byte
/// is given by `byte_at`.
pub(super) async fn consumer_core(
    mut rx: SharedRx,
    expected: usize,
    byte_at: fn(usize) -> u8,
) {
    let mut off = 0usize;
    loop {
        if off >= expected {
            break;
        }
        let take = core::cmp::min(11, expected - off);
        let x = rx.read_at_most_async(take).await;
        let Some(segm) = x.pick_left() else {
            panic!("[consumer] read_async failed");
        };
        let len = segm.least_count();
        let mut segm = segm;
        let got = take_segm(&mut segm, len);
        for (i, b) in got.iter().enumerate() {
            assert_eq!(*b, byte_at(off + i), "[consumer] mismatch at {off}+{i}");
        }
        off += len;
        drop(segm);
    }
    rx.close();
}

/// The kernel simulation for the *send* direction: takes the readable region
/// (iovec pair), verifies the bytes, and returns it with `put_back_send`.
pub(super) fn send_driver_core(ring: SharedRing, total: usize) {
    let mut verified = 0usize;
    loop {
        if let Some((a, b)) = ring.take_send_iovecs() {
            for x in a.iter().chain(b.iter()) {
                assert_eq!(*x, seq_byte(verified), "[send driver] mismatch");
                verified += 1;
            }
            ring.put_back_send(a.len() + b.len());
        } else {
            if ring.is_tx_closed() && verified == total {
                break;
            }
            std::thread::yield_now();
        }
    }
    assert_eq!(verified, total, "[send driver] not fully verified");
}

/// The kernel simulation for the *receive* direction: takes the writable
/// region (iovec pair), fills it with the pattern, and returns it with
/// `put_back_recv`. Stops when the rx end is closed.
pub(super) fn recv_driver_core(ring: SharedRing) {
    let mut filled = 0usize;
    loop {
        if let Some((a, b)) = ring.take_recv_iovecs() {
            for (i, slot) in a.iter_mut().enumerate() {
                *slot = pat_byte(filled + i);
            }
            for (i, slot) in b.iter_mut().enumerate() {
                *slot = pat_byte(filled + a.len() + i);
            }
            let n = a.len() + b.len();
            ring.put_back_recv(n);
            filled += n;
        } else if ring.is_rx_closed() {
            break;
        } else {
            std::thread::yield_now();
        }
    }
}

/// Cooperative variants of the kernel drivers (yield between iterations), for
/// single-threaded executors without `spawn_blocking`.
pub(super) async fn send_driver_core_async(ring: SharedRing, total: usize) {
    let mut verified = 0usize;
    loop {
        if let Some((a, b)) = ring.take_send_iovecs() {
            for x in a.iter().chain(b.iter()) {
                assert_eq!(*x, seq_byte(verified), "[send driver async] mismatch");
                verified += 1;
            }
            ring.put_back_send(a.len() + b.len());
        } else if ring.is_tx_closed() && verified == total {
            break;
        } else {
            futures_lite::future::yield_now().await;
        }
    }
    assert_eq!(verified, total, "[send driver async] not fully verified");
}

pub(super) async fn recv_driver_core_async(ring: SharedRing) {
    let mut filled = 0usize;
    loop {
        if let Some((a, b)) = ring.take_recv_iovecs() {
            for (i, slot) in a.iter_mut().enumerate() {
                *slot = pat_byte(filled + i);
            }
            for (i, slot) in b.iter_mut().enumerate() {
                *slot = pat_byte(filled + a.len() + i);
            }
            let n = a.len() + b.len();
            ring.put_back_recv(n);
            filled += n;
        } else if ring.is_rx_closed() {
            break;
        } else {
            futures_lite::future::yield_now().await;
        }
    }
}

type BoxedFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
type BoxedFnOnce = Box<dyn FnOnce() + Send + 'static>;

/// The shared **pipe** scenario: the user writes into the tx end and reads
/// from the rx end of the *same* ring (a direct SPSC pipe, no kernel).
pub(super) async fn run_pipe_scenario(
    spawn: impl Fn(BoxedFuture) -> BoxedFuture + Copy,
    _spawn_blocking: impl FnOnce(BoxedFnOnce) -> BoxedFuture,
) {
    const TOTAL: usize = 200;

    let (_ring, tx, rx) = make_ring();
    let producer = spawn(Box::pin(producer_core(tx, TOTAL)));
    let consumer = spawn(Box::pin(consumer_core(rx, TOTAL, seq_byte)));

    // Join both concurrently: on a single-threaded executor, awaiting the
    // producer alone would starve the consumer (the producer parks when the
    // ring is full, and only the consumer can drain it).
    futures_lite::future::zip(producer, consumer).await;
}

/// The shared **kernel** scenario: two independent rings, each connecting the
/// user with a kernel simulation through the vectored-IO handoff.
pub(super) async fn run_kernel_scenario(
    spawn: impl Fn(BoxedFuture) -> BoxedFuture + Copy,
    spawn_blocking: impl Fn(BoxedFnOnce) -> BoxedFuture + Copy,
) {
    const TOTAL: usize = 200;

    // ring_out: user writes -> kernel writev
    //
    // 设计思路：`try_split_shared` 要求唯一持有者拆分（引用计数 == 1），
    // 所以先把新建的 Arc 移入拆分；驱动任务所需的 Arc 在拆分之后从写半区
    // clone 得到（计数 >= 2，第二次拆分会被拒绝，SPSC 不破坏）。
    let ring_out = make_ring_shared();
    let (tx_out, _) = super::RingBuffer::try_split_shared(ring_out, Arc::strong_count, Arc::weak_count)
        .expect("ring_out 拆分必须成功");
    let driver_out_ring = tx_out.shared().clone();
    let driver_out = spawn_blocking(Box::new(move || send_driver_core(driver_out_ring, TOTAL)));
    let producer = spawn(Box::pin(producer_core(tx_out, TOTAL)));

    // ring_in: kernel readv -> user reads
    let ring_in = make_ring_shared();
    let (_, rx_in) = super::RingBuffer::try_split_shared(ring_in, std::sync::Arc::strong_count, std::sync::Arc::weak_count)
        .expect("ring_in 拆分必须成功");
    let driver_in_ring = rx_in.shared().clone();
    let driver_in = spawn_blocking(Box::new(move || recv_driver_core(driver_in_ring)));
    let consumer = spawn(Box::pin(consumer_core(rx_in, TOTAL, pat_byte)));

    futures_lite::future::zip(producer, driver_out).await;
    futures_lite::future::zip(consumer, driver_in).await;
}

/// The same pipe scenario, driven entirely from `std::thread`s with only the
/// synchronous API (no executor involved).
pub(super) fn run_pipe_scenario_sync() {
    const TOTAL: usize = 200;

    let (_ring, mut tx, mut rx) = make_ring();

    let writer = std::thread::spawn(move || {
        let mut off = 0usize;
        while off < TOTAL {
            let mut progressed = false;
            {
                let res = tx.try_write_at_most(5);
                if let Ok(mut segm) = res {
                    let len = segm.least_count();
                    fill_segm(&mut segm, &(0..len).map(|i| seq_byte(off + i)).collect::<Vec<_>>());
                    drop(segm);
                    off += len;
                    progressed = true;
                }
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
            match rx.try_read_at_most(9) {
                Ok(segm) => {
                    let len = segm.least_count();
                    let mut segm = segm;
                    let got = take_segm(&mut segm, len);
                    for (i, b) in got.iter().enumerate() {
                        assert_eq!(*b, seq_byte(off + i), "[sync pipe] mismatch");
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

/// Run the pipe + kernel scenarios on the hand-rolled [`super::mini_exec::MiniExec`].
pub(super) fn run_scenarios_mini(exec: &mut super::mini_exec::MiniExec) {
    const TOTAL: usize = 200;

    // pipe scenario (no kernel)
    {
        let (_ring, tx, rx) = make_ring();
        exec.spawn(producer_core(tx, TOTAL));
        exec.spawn(consumer_core(rx, TOTAL, seq_byte));
    }
    // kernel scenario (cooperative drivers)
    //
    // 设计思路：与 run_kernel_scenario 相同——以唯一持有者身份拆分，驱动侧
    // 所需 Arc 在拆分后从半区 clone 得到，避免产生第二对生产者/消费者。
    {
        let ring_out = make_ring_shared();
        let (tx_out, _) = super::RingBuffer::try_split_shared(ring_out, std::sync::Arc::strong_count, std::sync::Arc::weak_count)
            .expect("ring_out 拆分必须成功");
        let driver_out_ring = tx_out.shared().clone();
        exec.spawn(send_driver_core_async(driver_out_ring, TOTAL));
        exec.spawn(producer_core(tx_out, TOTAL));

        let ring_in = make_ring_shared();
        let (_, rx_in) = super::RingBuffer::try_split_shared(ring_in, std::sync::Arc::strong_count, std::sync::Arc::weak_count)
            .expect("ring_in 拆分必须成功");
        let driver_in_ring = rx_in.shared().clone();
        exec.spawn(recv_driver_core_async(driver_in_ring));
        exec.spawn(consumer_core(rx_in, TOTAL, pat_byte));
    }
}

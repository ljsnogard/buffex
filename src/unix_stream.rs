//! A compio unix-socket adapter implementing the abs_buff buffer traits.
//!
//! [`BufferedUnixStream`] wraps a `compio::net::UnixStream` with two ring
//! buffers (from [`crate::ring_buffer`]):
//!
//! * the **write ring** — the user borrows writable segments through
//!   `TrBuffWrite` / `TrBuffTryWrite`; a background *flush* task takes the
//!   committed region as an iovec pair and submits it to the socket with a
//!   single `writev` syscall;
//! * the **read ring** — a background *fill* task submits the writable region
//!   to the socket with a single `readv` syscall; the user drains the
//!   received segments through `TrBuffRead` / `TrBuffTryRead`.
//!
//! Both background tasks are spawned on the current compio runtime, so the
//! constructor must be called inside a compio runtime context (e.g. inside
//! `Runtime::block_on`).

extern crate std;

use std::{
    boxed::Box,
    io,
    sync::{Arc, Mutex},
    vec,
};

use compio::{
    buf::BufResult,
    io::{AsyncRead, AsyncWrite},
    net::UnixStream,
    runtime::{spawn, JoinHandle},
};

use abs_buff::{
    x_deps::{abs_cancel, anylr},
    Demand, TrBuffRead, TrBuffTryRead, TrBuffTryWrite, TrBuffWrite,
};

use abs_cancel::TrMayCancel;
use anylr::SomeOf;



use crate::ring_buffer::{
    RecvSlices, RingBuffer, RingRx, RingTx, RxError, SendSlices, TxError,
};

pub type SharedWriteRing = RingTx<Arc<RingBuffer<Box<[u8]>>>, Box<[u8]>>;
pub type SharedReadRing = RingRx<Arc<RingBuffer<Box<[u8]>>>, Box<[u8]>>;

/// A compio unix stream adapted to the abs_buff buffered-IO traits.
pub struct BufferedUnixStream {
    tx: SharedWriteRing,
    rx: SharedReadRing,
    /// The stream, held so `shutdown` can unblock the background tasks.
    stream: Arc<UnixStream>,
    /// The error reported by a background task, if any.
    error: Arc<Mutex<Option<io::Error>>>,
    /// The flush task handle; detached on drop.
    _flush: JoinHandle<()>,
    /// The fill task handle; detached on drop.
    _fill: JoinHandle<()>,
}

impl BufferedUnixStream {
    /// Wrap `stream` with `cap`-byte rings for both directions.
    ///
    /// ## Panics
    ///
    /// Panics if called outside of a compio runtime context (the background
    /// tasks are spawned on the current runtime).
    pub fn new(stream: UnixStream, cap: usize) -> Self {
        let stream = Arc::new(stream);

        let write_ring = Arc::new(
            RingBuffer::<Box<[u8]>>::try_new(vec![0u8; cap].into_boxed_slice())
                .expect("write ring"),
        );
        let read_ring = Arc::new(
            RingBuffer::<Box<[u8]>>::try_new(vec![0u8; cap].into_boxed_slice())
                .expect("read ring"),
        );
        let (tx, _) = RingBuffer::try_split_shared(write_ring, Arc::strong_count, Arc::weak_count)
            .expect("write ring must be sole-owned at split");
        let (_, rx) = RingBuffer::try_split_shared(read_ring, Arc::strong_count, Arc::weak_count)
            .expect("read ring must be sole-owned at split");
        // 拆分要求调用方持有唯一引用（引用计数 == 1）：拆分内部会把 Arc clone 进
        // 两个半区，若调用前已有其它 clone，就可能拆出第二对生产者/消费者，
        // 破坏 SPSC。两个 ring 都是这里新建的，计数为 1，拆分必然成功。
        // 拆分之后，驱动任务所需的 Arc 从半区 clone 得到（此时计数 >= 2，
        // 不会再允许第二次拆分，SPSC 依然成立）。
        let flush_ring = tx.shared().clone();
        let fill_ring = rx.shared().clone();

        let error = Arc::new(Mutex::new(None::<io::Error>));

        let flush = spawn(flush_task(stream.clone(), flush_ring, error.clone()));
        let fill = spawn(fill_task(stream.clone(), fill_ring, error.clone()));

        BufferedUnixStream {
            tx,
            rx,
            stream,
            error,
            _flush: flush,
            _fill: fill,
        }
    }

    /// The write half (implements `TrBuffWrite`).
    pub fn tx(&mut self) -> &mut SharedWriteRing {
        &mut self.tx
    }

    /// The read half (implements `TrBuffRead`).
    pub fn rx(&mut self) -> &mut SharedReadRing {
        &mut self.rx
    }

    /// Borrow the write half and the read half together (e.g. for
    /// `abs_buff::chaining::Chain`).
    pub fn split(&mut self) -> (&mut SharedWriteRing, &mut SharedReadRing) {
        (&mut self.tx, &mut self.rx)
    }

    /// The error reported by a background task, if any.
    pub fn take_error(&self) -> Option<io::Error> {
        self.error.lock().ok().and_then(|mut g| g.take())
    }

    /// Close both ends: buffered data is flushed, then the socket is shut
    /// down once the background tasks notice the closure.
    pub fn close(&mut self) {
        self.tx.close();
        self.rx.close();
    }

    /// Close both ends and wait for the background tasks to drain and exit.
    ///
    /// Prefer this over dropping the wrapper in long-lived code: it
    /// guarantees every buffer region is returned to the rings before the
    /// underlying buffers are freed.
    ///
    /// Ordering: the flush task is awaited first so all committed data is
    /// sent, then the socket is shut down (which unblocks the fill task's
    /// pending `readv` with an EOF), then the fill task is awaited.
    pub async fn shutdown(mut self) {
        self.close();
        let _ = (&mut self._flush).await;
        let _ = compio::io::AsyncWrite::shutdown(&mut &*self.stream).await;
        let _ = (&mut self._fill).await;
    }
}

impl Drop for BufferedUnixStream {
    fn drop(&mut self) {
        // Closing the rings makes the background tasks drain and exit; they
        // own `Arc` clones of the stream, so nothing dangles after detach.
        self.tx.close();
        self.rx.close();
    }
}

// ---------------------------------------------------------------------------
// abs_buff traits
// ---------------------------------------------------------------------------

impl TrBuffWrite<u8> for BufferedUnixStream {
    type SegmMut<'a> = <SharedWriteRing as TrBuffWrite<u8>>::SegmMut<'a>
    where
        Self: 'a;
    type Err = TxError<usize>;

    fn is_blocked_closing(&self) -> bool {
        self.tx.is_blocked_closing()
    }

    fn write_async<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> impl TrMayCancel<
        'f,
        MayCancelOutput = SomeOf<Self::SegmMut<'f>, Self::Err>,
    > {
        <SharedWriteRing as TrBuffWrite<u8>>::write_async(&mut self.tx, demand)
    }
}

impl TrBuffTryWrite<u8> for BufferedUnixStream {
    fn try_write<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> SomeOf<Self::SegmMut<'f>, Self::Err> {
        <SharedWriteRing as TrBuffTryWrite<u8>>::try_write(&mut self.tx, demand)
    }
}

impl TrBuffRead<u8> for BufferedUnixStream {
    type SegmRef<'a> = <SharedReadRing as TrBuffRead<u8>>::SegmRef<'a>
    where
        Self: 'a;
    type Err = RxError<usize>;

    fn is_drained_closing(&self) -> bool {
        self.rx.is_drained_closing()
    }

    fn read_async<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> impl TrMayCancel<
        'f,
        MayCancelOutput = SomeOf<Self::SegmRef<'f>, Self::Err>,
    > {
        <SharedReadRing as TrBuffRead<u8>>::read_async(&mut self.rx, demand)
    }
}

impl TrBuffTryRead<u8> for BufferedUnixStream {
    fn try_read<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> SomeOf<Self::SegmRef<'f>, Self::Err> {
        <SharedReadRing as TrBuffTryRead<u8>>::try_read(&mut self.rx, demand)
    }
}

// ---------------------------------------------------------------------------
// background tasks
// ---------------------------------------------------------------------------

/// Take committed data from the write ring and submit it to the socket with
/// `writev`, until the tx end is closed.
async fn flush_task(
    stream: Arc<UnixStream>,
    ring: Arc<RingBuffer<Box<[u8]>>>,
    error: Arc<Mutex<Option<io::Error>>>,
) {
    loop {
        ring.wait_flushed().await;
        while let Some((a, b)) = ring.take_send_iovecs() {
            let BufResult(res, _) = (&*stream).write_vectored(SendSlices(a, b)).await;
            match res {
                Ok(n) => ring.put_back_send(n),
                Err(e) => {
                    if let Ok(mut guard) = error.lock() {
                        *guard = Some(e);
                    }
                    ring.close_tx();
                    return;
                }
            }
        }
        if ring.is_tx_closed() {
            return;
        }
    }
}

/// Receive data from the socket into the read ring with `readv`, until the
/// rx end is closed (or EOF).
async fn fill_task(
    stream: Arc<UnixStream>,
    ring: Arc<RingBuffer<Box<[u8]>>>,
    error: Arc<Mutex<Option<io::Error>>>,
) {
    loop {
        ring.wait_rx_idle().await;
        while let Some((a, b)) = ring.take_recv_iovecs() {
            let slices = RecvSlices(a, b);
            let BufResult(res, returned) = (&*stream).read_vectored(slices).await;
            let (a, b) = (returned.0, returned.1);
            let _ = (a, b);
            match res {
                Ok(0) => {
                    // EOF: the peer closed the connection.
                    ring.put_back_recv(0);
                    ring.close_rx();
                    return;
                }
                Ok(n) => ring.put_back_recv(n),
                Err(e) => {
                    if let Ok(mut guard) = error.lock() {
                        *guard = Some(e);
                    }
                    ring.close_rx();
                    return;
                }
            }
        }
        if ring.is_rx_closed() {
            return;
        }
    }
}

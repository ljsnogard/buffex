//! Per-framework tests: the shared scenarios plus the framework-specific
//! `AsyncRead` / `AsyncWrite` trait implementations.

use std::{
    boxed::Box,
    format, vec,
    vec::Vec,
};

use super::scenario_::{
    run_kernel_scenario, run_pipe_scenario, run_pipe_scenario_sync, run_scenarios_mini,
};
use super::{fill_segm, make_ring, seq_byte};

const TOTAL: usize = 200;

// ---------------------------------------------------------------------------
// compio (default feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "compio")]
mod compio_ {
    use super::*;
    use std::pin::Pin;

    fn spawn_blocking_io(f: Box<dyn FnOnce() + Send>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move { compio::runtime::spawn_blocking(move || f()).await.unwrap() })
    }

    fn spawn_io(f: Pin<Box<dyn Future<Output = ()> + Send>>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move { compio::runtime::spawn(f).await.unwrap() })
    }

    /// The shared pipe + kernel scenarios driven by compio's executor.
    #[test]
    fn compio_shared_scenarios() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            run_pipe_scenario(spawn_io, spawn_blocking_io).await;
            run_kernel_scenario(spawn_io, spawn_blocking_io).await;
        });
    }

    /// The `compio::io::AsyncRead` / `AsyncWrite` trait implementations: the
    /// ring is used as a direct pipe.
    #[test]
    fn compio_async_traits() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            use compio::buf::BufResult;
            use compio::io::{AsyncReadExt, AsyncWriteExt};

            let (_ring, mut tx, mut rx) = make_ring();

            let data: Vec<u8> = (0..TOTAL).map(seq_byte).collect();
            let producer = compio::runtime::spawn(async move {
                let BufResult(res, _) = tx.write_all(data).await;
                res.expect("write_all");
                compio::io::AsyncWrite::flush(&mut tx).await.expect("flush");
                compio::io::AsyncWrite::shutdown(&mut tx).await.expect("shutdown");
            });

            let expected: Vec<u8> = (0..TOTAL).map(seq_byte).collect();
            let consumer = compio::runtime::spawn(async move {
                let buf = vec![0u8; TOTAL];
                let BufResult(res, buf) = rx.read_exact(buf).await;
                res.expect("read_exact");
                assert_eq!(buf.as_slice(), expected.as_slice());
                rx.close();
            });

            producer.await.expect("producer");
            consumer.await.expect("consumer");
        });
    }

    /// The vectored (scatter/gather) kernel handoff against a *real* compio
    /// file: the ring's iovec pair is submitted to the kernel with a single
    /// `writev` / `readv` syscall.
    #[test]
    fn compio_vectored_kernel_io() {
        let path = std::env::temp_dir().join(format!(
            "buffex-compio-vectored-{}.bin",
            std::process::id()
        ));
        compio::runtime::Runtime::new().unwrap().block_on(async {
            use compio::buf::BufResult;
            use compio::io::{AsyncReadAt, AsyncWriteAt};

            // A bigger ring so the wrapped iovec pair is exercised.
            const BIG: usize = 64;
            let ring = std::sync::Arc::new(
                crate::ring_buffer::RingBuffer::<Box<[u8]>>::try_new(
                    vec![0u8; BIG].into_boxed_slice(),
                )
                .unwrap(),
            );
            // 设计思路：`try_split_shared` 要求唯一持有者拆分（引用计数 == 1），
            // 所以把新建的 Arc 直接移入；下面驱动循环所需的 Arc 在拆分后从
            // 写半区 clone 得到（计数 >= 2，不会产生第二对生产者/消费者）。
            let (mut tx, _) = crate::ring_buffer::RingBuffer::try_split_shared(
                ring,
                std::sync::Arc::strong_count,
                std::sync::Arc::weak_count,
            )
            .expect("新建 ring 的引用计数为 1，拆分必须成功");
            let ring = tx.shared().clone();

            let data: Vec<u8> = (0..(BIG * 3)).map(seq_byte).collect();
            let mut file = compio::fs::File::create(&path).await.expect("create file");

            // The user fills the ring; a driver loop only drains (submits the
            // readable region as an iovec pair to the kernel with
            // write_vectored_at, a single syscall) when the ring is full. This
            // makes the reader position lag behind and forces wrapped iovec
            // pairs.
            let mut off = 0usize;
            let mut pos = 0u64;
            let mut had_wrap = false;
            while off < data.len() {
                let mut progressed = false;
                {
                    let res = tx.try_write_at_most(8);
                    if let Ok(mut segm) = res {
                        // 段可能比剩余源数据更长（ring 的空位可能多于剩余字节），
                        // 只填入实际剩余的部分，避免越过 data 末尾；
                        let len = core::cmp::min(segm.least_count(), data.len() - off);
                        fill_segm(&mut segm, &(0..len).map(|i| data[off + i]).collect::<Vec<_>>());
                        drop(segm);
                        off += len;
                        progressed = true;
                    }
                }
                if !progressed {
                    // ring full: drain everything committed
                    while let Some((a, b)) = ring.take_send_iovecs() {
                        if !b.is_empty() {
                            had_wrap = true;
                        }
                        let n = a.len() + b.len();
                        let BufResult(res, _) =
                            file.write_vectored_at(crate::ring_buffer::compio_::SendSlices(a, b), pos)
                                .await;
                        res.expect("kernel writev");
                        pos += n as u64;
                        ring.put_back_send(n);
                    }
                }
            }
            tx.close();
            while let Some((a, b)) = ring.take_send_iovecs() {
                if !b.is_empty() {
                    had_wrap = true;
                }
                let n = a.len() + b.len();
                let BufResult(res, _) =
                    file.write_vectored_at(crate::ring_buffer::compio_::SendSlices(a, b), pos)
                        .await;
                res.expect("kernel writev (drain)");
                pos += n as u64;
                ring.put_back_send(n);
            }
            assert!(had_wrap, "the test should exercise a wrapped iovec pair");

            // Read the file back and verify the exact byte order.
            let rfile = compio::fs::File::open(&path).await.expect("open file");
            let content = vec![0u8; data.len()];
            let BufResult(res, content) = rfile.read_at(content, 0).await;
            res.expect("file read");
            assert_eq!(content.as_slice(), data.as_slice());
        });
        let _ = std::fs::remove_file(&path);
    }
}

// ---------------------------------------------------------------------------
// tokio (feature `tokio`)
// ---------------------------------------------------------------------------

#[cfg(feature = "tokio")]
mod tokio_ {
    use super::*;

    /// The shared pipe + kernel scenarios driven by tokio's executor.
    #[test]
    fn tokio_shared_scenarios() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                run_pipe_scenario(
                    |f| Box::pin(async { tokio::spawn(f).await.unwrap() }),
                    |f| Box::pin(async { tokio::task::spawn_blocking(move || f()).await.unwrap() }),
                )
                .await;
                run_kernel_scenario(
                    |f| Box::pin(async { tokio::spawn(f).await.unwrap() }),
                    |f| Box::pin(async { tokio::task::spawn_blocking(move || f()).await.unwrap() }),
                )
                .await;
            });
    }

    /// The `tokio::io::AsyncRead` / `AsyncWrite` trait implementations: the
    /// ring is used as a direct pipe.
    #[test]
    fn tokio_async_traits() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                let (_ring, tx, rx) = make_ring();

                let data: Vec<u8> = (0..TOTAL).map(seq_byte).collect();
                let producer = tokio::spawn(async move {
                    let tx = tx;
                    // The halves are `!Unpin` (their waker slot must not
                    // move); pin them for the poll-based traits.
                    tokio::pin!(tx);
                    tx.write_all(&data).await.expect("write_all");
                    tx.flush().await.expect("flush");
                    tx.shutdown().await.expect("shutdown");
                });

                let expected: Vec<u8> = (0..TOTAL).map(seq_byte).collect();
                let consumer = tokio::spawn(async move {
                    let rx = rx;
                    tokio::pin!(rx);
                    let mut buf = vec![0u8; TOTAL];
                    rx.read_exact(&mut buf).await.expect("read_exact");
                    assert_eq!(buf, expected);
                    unsafe { rx.get_unchecked_mut() }.close();
                });

                producer.await.expect("producer");
                consumer.await.expect("consumer");
            });
    }
}

// ---------------------------------------------------------------------------
// smol / futures-io (feature `smol`) — driven by a hand-rolled executor
// ---------------------------------------------------------------------------

#[cfg(feature = "smol")]
mod smol_ {
    use super::super::mini_exec::MiniExec;
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    /// The shared pipe + kernel scenarios driven by the hand-rolled
    /// [`MiniExec`].
    #[test]
    fn smol_shared_scenarios() {
        let mut exec = MiniExec::new();
        run_scenarios_mini(&mut exec);
        exec.run_until_empty();
    }

    /// A tiny `write_all` for `futures_io::AsyncWrite`.
    async fn fio_write_all<W: futures_io::AsyncWrite>(mut w: Pin<&mut W>, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let n = std::future::poll_fn(|cx| w.as_mut().poll_write(cx, data)).await?;
            assert!(n > 0, "futures-io poll_write returned 0");
            data = &data[n..];
        }
        Ok(())
    }

    /// A tiny `read_exact` for `futures_io::AsyncRead`.
    async fn fio_read_exact<R: futures_io::AsyncRead>(mut r: Pin<&mut R>, buf: &mut [u8]) -> std::io::Result<()> {
        let mut off = 0usize;
        while off < buf.len() {
            let n = std::future::poll_fn(|cx| r.as_mut().poll_read(cx, &mut buf[off..])).await?;
            assert!(n > 0, "futures-io poll_read returned 0 (unexpected EOF)");
            off += n;
        }
        Ok(())
    }

    /// The `futures_io::AsyncRead` / `AsyncWrite` trait implementations.
    #[test]
    fn smol_async_traits() {
        let (_ring, tx, rx) = make_ring();

        // The halves are `!Unpin`; own them through a pinning box.
        let mut tx = Box::pin(tx);
        let mut rx = Box::pin(rx);

        let mut exec = MiniExec::new();
        let data: Vec<u8> = (0..TOTAL).map(seq_byte).collect();
        let expected: Vec<u8> = (0..TOTAL).map(seq_byte).collect();

        let producer = {
            let data = data.clone();
            Box::pin(async move {
                fio_write_all(tx.as_mut(), &data).await.expect("fio write_all");
                // dropping `tx` closes the tx end
                drop(tx);
            }) as Pin<Box<dyn Future<Output = ()>>>
        };
        let consumer = {
            let mut got = vec![0u8; TOTAL];
            Box::pin(async move {
                fio_read_exact(rx.as_mut(), &mut got).await.expect("fio read_exact");
                assert_eq!(got, expected);
                // dropping `rx` closes the rx end
                drop(rx);
            }) as Pin<Box<dyn Future<Output = ()>>>
        };

        exec.spawn(producer);
        exec.spawn(consumer);
        exec.run_until_empty();
    }
}

// ---------------------------------------------------------------------------
// framework-independent sanity
// ---------------------------------------------------------------------------

/// The same pipe logic fully synchronous with `std::thread`s.
#[test]
fn pipe_sync_threads() {
    run_pipe_scenario_sync();
}

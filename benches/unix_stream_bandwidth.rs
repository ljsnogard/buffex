//! Bandwidth benchmark: a compio unix socket wrapped in the abs_buff buffer
//! traits (`TrBuffWrite` / `TrBuffRead`), with the ring buffer providing the
//! segments and a single `writev` / `readv` syscall per handoff.
//!
//! Run with:
//!
//! ```sh
//! cargo bench -p buffex --bench unix_stream_bandwidth
//! ```
//!
//! Reports the one-way client→server bandwidth (client send / server recv)
//! and the echo bandwidth (client send + recv through a server loopback).

#[cfg(not(all(feature = "compio", unix)))]
fn main() {
    eprintln!("this benchmark requires the `compio` feature on unix");
}

#[cfg(all(feature = "compio", unix))]
fn main() {
    use std::{
        format,
        time::Instant,
        vec::Vec,
    };
    use abs_buff::{
        x_deps::abs_cancel,
        Demand,
        TrBuffRead, TrBuffTryRead, TrBuffTryWrite, TrBuffWrite,
    };
    use abs_cancel::{NonCancellableToken, TrMayCancel};

    use buffex::unix_stream::BufferedUnixStream;

    const RING_CAP: usize = 64 * 1024;
    const TOTAL: usize = 64 * 1024 * 1024; // 64 MiB per direction
    const CHUNK: usize = 16 * 1024;

    fn sock_path() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("buffex-bench-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Write `data` into a write segment — the segment's buffer is the ring's
    /// own memory — and commit exactly `data.len()` units (per-piece reclaim
    /// granularity of the `abs_buff` segments).
    fn fill_segm(segm: &mut buffex::ring_buffer::ReclSliceMut<'_, u8>, data: &[u8]) {
        use core::mem::MaybeUninit;
        let mut staging: Vec<MaybeUninit<u8>> =
            data.iter().map(|&b| MaybeUninit::new(b)).collect();
        let moved = unsafe { segm.move_items_from_buff(&mut staging) };
        assert_eq!(moved, data.len());
    }

    /// Consume `len` units from a read segment into a fresh `Vec<u8>`; the
    /// reader position advances by `len` when the segment drops.
    fn take_segm(segm: &mut buffex::ring_buffer::ReclSliceRef<'_, u8>, len: usize) -> Vec<u8> {
        use core::mem::MaybeUninit;
        let mut dst: Vec<MaybeUninit<u8>> = Vec::with_capacity(len);
        dst.resize_with(len, MaybeUninit::uninit);
        let moved = unsafe { segm.move_items_to_buff(&mut dst) };
        assert_eq!(moved, len);
        dst[..len].iter().map(|m| unsafe { m.assume_init_read() }).collect()
    }

    /// Push `total` bytes through `TrBuffWrite::write_async`, filling each
    /// borrowed segment completely.
    async fn send_all(buffered: &mut BufferedUnixStream, total: usize) {
        let mut off = 0usize;
        while off < total {
            let demand = Demand::less_than(core::cmp::min(CHUNK, total - off));
            let x = buffered
                .write_async(&demand)
                .may_cancel_with(NonCancellableToken::shared_mut())
                .await;
            let Some(mut segm) = x.pick_left() else {
                panic!("write_async failed");
            };
            let len = segm.least_count();
            fill_segm(&mut segm, &(0..len).map(|i| (off + i) as u8).collect::<Vec<_>>());
            drop(segm);
            off += len;
        }
    }

    /// Receive `total` bytes through `TrBuffRead::read_async`, consuming each
    /// segment.
    async fn recv_all(buffered: &mut BufferedUnixStream, total: usize) {
        let mut off = 0usize;
        while off < total {
            let demand = Demand::less_than(core::cmp::min(CHUNK, total - off));
            let x = buffered
                .read_async(&demand)
                .may_cancel_with(NonCancellableToken::shared_mut())
                .await;
            let Some(segm) = x.pick_left() else {
                panic!("read_async failed (peer closed early?)");
            };
            let len = segm.least_count();
            let mut segm = segm;
            let _ = take_segm(&mut segm, len);
            drop(segm);
            off += len;
        }
    }

    let mi_bps = |elapsed: std::time::Duration, bytes: usize| -> f64 {
        bytes as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0)
    };

    compio::runtime::Runtime::new().unwrap().block_on(async {
        // ---- one-way: client sends, server receives ----
        let path = sock_path();
        let listener = compio::net::UnixListener::bind(&path).await.expect("bind");
        let accept = compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            stream
        });
        let stream = compio::net::UnixStream::connect(&path).await.expect("connect");
        let server_stream = accept.await.expect("accept task");

        let server = compio::runtime::spawn(async move {
            let mut buffered = BufferedUnixStream::new(server_stream, RING_CAP);
            let start = Instant::now();
            recv_all(&mut buffered, TOTAL).await;
            let elapsed = start.elapsed();
            buffered.shutdown().await;
            elapsed
        });

        let mut buffered = BufferedUnixStream::new(stream, RING_CAP);
        let start = Instant::now();
        send_all(&mut buffered, TOTAL).await;
        let send_elapsed = start.elapsed();
        buffered.shutdown().await;
        let recv_elapsed = server.await.expect("server task");

        println!(
            "one-way client→server: {} MiB in {:.3}s → send {:.1} MiB/s, recv {:.1} MiB/s",
            TOTAL / (1024 * 1024),
            send_elapsed.as_secs_f64(),
            mi_bps(send_elapsed, TOTAL),
            mi_bps(recv_elapsed, TOTAL),
        );

        // ---- echo: server loops the data back; the client interleaves
        // sends and receives through the *try* interfaces (nothing parks, so
        // the full-duplex backpressure cannot deadlock) ----
        let path = sock_path();
        let listener = compio::net::UnixListener::bind(&path).await.expect("bind");
        let accept = compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            stream
        });
        let stream = compio::net::UnixStream::connect(&path).await.expect("connect");
        let server_stream = accept.await.expect("accept task");

        let server = compio::runtime::spawn(async move {
            let mut buffered = BufferedUnixStream::new(server_stream, RING_CAP);
            let mut off = 0usize;
            while off < TOTAL {
                let demand = Demand::less_than(CHUNK);
                let x = buffered
                    .read_async(&demand)
                    .may_cancel_with(NonCancellableToken::shared_mut())
                    .await;
                let Some(segm) = x.pick_left() else {
                    panic!("echo server read failed");
                };
                let len = segm.least_count();
                let mut segm = segm;
                let scratch = take_segm(&mut segm, len);
                drop(segm);

                let wdemand = Demand::less_than(len);
                let wx = buffered
                    .write_async(&wdemand)
                    .may_cancel_with(NonCancellableToken::shared_mut())
                    .await;
                let Some(mut wsegm) = wx.pick_left() else {
                    panic!("echo server write failed");
                };
                fill_segm(&mut wsegm, &scratch);
                drop(wsegm);
                off += len;
            }
            buffered.shutdown().await;
        });

        let mut buffered = BufferedUnixStream::new(stream, RING_CAP);
        let mut sent = 0usize;
        let mut recvd = 0usize;
        let start = Instant::now();
        while sent < TOTAL || recvd < TOTAL {
            if sent < TOTAL {
                let demand = Demand::less_than(core::cmp::min(CHUNK, TOTAL - sent));
                let x = buffered.try_write(&demand);
                if let Some(mut segm) = x.pick_left() {
                    let len = segm.least_count();
                    fill_segm(&mut segm, &(0..len).map(|i| (sent + i) as u8).collect::<Vec<_>>());
                    drop(segm);
                    sent += len;
                }
            }
            if recvd < TOTAL {
                let demand = Demand::less_than(core::cmp::min(CHUNK, TOTAL - recvd));
                let x = buffered.try_read(&demand);
                if let Some(segm) = x.pick_left() {
                    let len = segm.least_count();
                    let mut segm = segm;
                    let _ = take_segm(&mut segm, len);
                    drop(segm);
                    recvd += len;
                }
            }
            if sent < TOTAL || recvd < TOTAL {
                futures_lite::future::yield_now().await;
            }
        }
        let echo_elapsed = start.elapsed();
        buffered.shutdown().await;
        server.await.expect("echo server task");

        println!(
            "echo: {} MiB sent + received in {:.3}s → {:.1} MiB/s round-trip",
            TOTAL / (1024 * 1024),
            echo_elapsed.as_secs_f64(),
            mi_bps(echo_elapsed, TOTAL * 2),
        );
    });
}

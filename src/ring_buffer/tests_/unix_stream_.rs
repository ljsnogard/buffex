//! Unit tests for the compio unix-socket adapter wrapped in the abs_buff
//! buffer traits (`TrBuffTryWrite` / `TrBuffTryRead`).

use std::{
    format, vec,
    vec::Vec,
};

use abs_buff::{
    x_deps::abs_cancel,
    pipelining::PipeJoin,
    Demand,
    TrBuffRead, TrBuffTryRead, TrBuffTryWrite, TrBuffWrite,
};
use abs_cancel::{NonCancellableToken, TrMayCancel};

use crate::unix_stream::BufferedUnixStream;

use super::{fill_segm, take_segm};

fn sock_path(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("buffex-{}-{}.sock", name, std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Write `total` bytes of `(off + i) as u8` through the wrapper's
/// `TrBuffWrite::write_async`, returning the number of bytes written.
async fn write_bytes(buffered: &mut BufferedUnixStream, mut off: usize, total: usize) -> usize {
    let mut written = 0usize;
    while written < total {
        // "borrow N commits N": never demand more than what remains to write
        let demand = Demand::less_than(core::cmp::min(1009, total - written));
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
        written += len;
    }
    written
}

/// Read `total` bytes through the wrapper's `TrBuffRead::read_async`,
/// verifying the pattern, returning the number of bytes read.
async fn read_and_verify(buffered: &mut BufferedUnixStream, mut off: usize, total: usize) -> usize {
    let mut read = 0usize;
    while read < total {
        let demand = Demand::less_than(997);
        let x = buffered
            .read_async(&demand)
            .may_cancel_with(NonCancellableToken::shared_mut())
            .await;
        let Some(segm) = x.pick_left() else {
            panic!("read_async failed");
        };
        let segm_len = segm.least_count();
        let mut segm = segm;
        let got = take_segm(&mut segm, segm_len);
        for (i, b) in got.iter().enumerate() {
            assert_eq!(*b, (off + i) as u8, "mismatch at {}+{}", off, i);
        }
        drop(segm);
        off += segm_len;
        read += segm_len;
    }
    read
}

/// Client sends `seq` data; server receives and verifies it.
#[test]
fn unix_stream_roundtrip() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        const TOTAL: usize = 1 << 20; // 1 MiB
        let path = sock_path("roundtrip");

        let listener = compio::net::UnixListener::bind(&path).await.expect("bind");
        let accept = compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            stream
        });
        let stream = compio::net::UnixStream::connect(&path).await.expect("connect");
        let server_stream = accept.await.expect("accept task");

        let server = compio::runtime::spawn(async move {
            let mut buffered = BufferedUnixStream::new(server_stream, 4096);
            let n = read_and_verify(&mut buffered, 0, TOTAL).await;
            assert_eq!(n, TOTAL);
            buffered.shutdown().await;
        });

        let mut buffered = BufferedUnixStream::new(stream, 4096);
        let n = write_bytes(&mut buffered, 0, TOTAL).await;
        assert_eq!(n, TOTAL);
        buffered.shutdown().await;

        server.await.expect("server task");
    });
}

/// Echo server built on `abs_buff::chaining::Chain`: the server moves data
/// from its read ring to its write ring; the client sends and reads back the
/// echo. This exercises the `Chain` machinery over the ring-buffer segments.
#[test]
fn unix_stream_echo_via_chain() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        const TOTAL: usize = 1 << 20; // 1 MiB
        let path = sock_path("echo");

        let listener = compio::net::UnixListener::bind(&path).await.expect("bind");
        let accept = compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            stream
        });
        let stream = compio::net::UnixStream::connect(&path).await.expect("connect");
        let server_stream = accept.await.expect("accept task");

        // --- server: echo via Chain ---
        let server = compio::runtime::spawn(async move {
            let mut buffered = BufferedUnixStream::new(server_stream, 4096);
            {
                let (tx, rx) = buffered.split();
                let mut pipe = PipeJoin::new(tx, rx);
                let _ = pipe.pipe_async().await;
            }
            buffered.shutdown().await;
        });

        // --- client: interleave sends and receives through the *try*
        // interfaces. A send-then-read client would deadlock on the
        // full-duplex backpressure (the server's echo write fills up once the
        // client stops reading); the try-based loop never parks, so the
        // background flush/fill tasks keep the pipe moving. ---
        let mut buffered = BufferedUnixStream::new(stream, 4096);
        let mut sent = 0usize;
        let mut recvd = 0usize;
        let mut got = Vec::new();
        loop {
            if sent < TOTAL {
                let demand = Demand::less_than(core::cmp::min(1009, TOTAL - sent));
                let x = buffered.try_write(&demand);
                if let Some(mut segm) = x.pick_left() {
                    let len = segm.least_count();
                    fill_segm(&mut segm, &(0..len).map(|i| (sent + i) as u8).collect::<Vec<_>>());
                    drop(segm);
                    sent += len;
                }
            }
            if recvd < TOTAL {
                let demand = Demand::less_than(core::cmp::min(997, TOTAL - recvd));
                let x = buffered.try_read(&demand);
                if let Some(segm) = x.pick_left() {
                    let len = segm.least_count();
                    let mut segm = segm;
                    let chunk = take_segm(&mut segm, len);
                    got.extend_from_slice(&chunk);
                    drop(segm);
                    recvd += len;
                }
            }
            if sent >= TOTAL && recvd >= TOTAL {
                break;
            }
            futures_lite::future::yield_now().await;
        }
        assert_eq!(sent, TOTAL);
        assert_eq!(got.len(), TOTAL, "echo must return exactly the sent bytes");
        for (i, b) in got.iter().enumerate() {
            assert_eq!(*b, i as u8, "[echo client] mismatch at {i}");
        }
        buffered.shutdown().await;

        server.await.expect("server task");
    });
}

/// A bigger-capacity sanity test with non-power-of-two sizes.
#[test]
fn unix_stream_odd_sizes() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        const TOTAL: usize = 300_007;
        let path = sock_path("odd");

        let listener = compio::net::UnixListener::bind(&path).await.expect("bind");
        let accept = compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            stream
        });
        let stream = compio::net::UnixStream::connect(&path).await.expect("connect");
        let server_stream = accept.await.expect("accept task");

        let server = compio::runtime::spawn(async move {
            let mut buffered = BufferedUnixStream::new(server_stream, 5000);
            let n = read_and_verify(&mut buffered, 0, TOTAL).await;
            assert_eq!(n, TOTAL);
            buffered.shutdown().await;
        });

        let mut buffered = BufferedUnixStream::new(stream, 5000);
        let n = write_bytes(&mut buffered, 0, TOTAL).await;
        assert_eq!(n, TOTAL);
        buffered.shutdown().await;

        server.await.expect("server task");
    });
}

/// The synchronous `TrBuffTryWrite` / `TrBuffTryRead` interfaces.
#[test]
fn unix_stream_try_api() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let path = sock_path("try");
        let listener = compio::net::UnixListener::bind(&path).await.expect("bind");
        let accept = compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            stream
        });
        let stream = compio::net::UnixStream::connect(&path).await.expect("connect");
        let server_stream = accept.await.expect("accept task");

        let server = compio::runtime::spawn(async move {
            let mut buffered = BufferedUnixStream::new(server_stream, 4096);
            // read until the 16 bytes arrive (the fill task receives them)
            let mut got = Vec::new();
            for _ in 0..10_000 {
                let x = buffered.try_read(&Demand::less_than(64));
                if let Some(segm) = x.pick_left() {
                    let len = segm.least_count();
                    let mut segm = segm;
                    let chunk = take_segm(&mut segm, len);
                    got.extend_from_slice(&chunk);
                    drop(segm);
                    if got.len() >= 16 {
                        break;
                    }
                }
                futures_lite::future::yield_now().await;
            }
            assert_eq!(got, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
            buffered.shutdown().await;
        });

        let mut buffered = BufferedUnixStream::new(stream, 4096);
        // write 16 bytes via the try interface
        let mut written = 0usize;
        for _ in 0..10_000 {
            if written >= 16 {
                break;
            }
            let x = buffered.try_write(&Demand::less_than(core::cmp::min(64, 16 - written)));
            if let Some(mut segm) = x.pick_left() {
                let len = segm.least_count();
                fill_segm(&mut segm, &(0..len).map(|i| (written + i) as u8).collect::<Vec<_>>());
                drop(segm);
                written += len;
            }
            futures_lite::future::yield_now().await;
        }
        assert_eq!(written, 16);
        buffered.shutdown().await;

        server.await.expect("server task");
    });
}

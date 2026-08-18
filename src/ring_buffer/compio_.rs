//! `compio::io::AsyncRead` / `compio::io::AsyncWrite` implementations
//! (feature `compio`, enabled by default).
//!
//! compio's model takes an *owned* buffer for each operation and returns it
//! together with the result. The ring exposes two complementary modes:
//!
//! * the *user-space* mode below, where the caller provides a buffer and the
//!   ring copies data into / out of it;
//! * the *kernel-handoff* (vectored) mode: [`RingBuffer::take_send_iovecs`]
//!   and [`RingBuffer::take_recv_iovecs`] hand the readable / writable region
//!   of the ring to the caller as an iovec pair (one or two `&'static [u8]`
//!   slices, depending on whether the region wraps). The pair can be passed
//!   directly to compio's `write_vectored` / `read_vectored` (a single
//!   `writev` / `readv` syscall); the region is returned with
//!   [`RingBuffer::put_back_send`] / [`RingBuffer::put_back_recv`].

/// Two read-only slices of the ring's readable region, submitable to
/// compio's `write_vectored` / `write_vectored_at` (one `writev` syscall).
///
/// ```ignore
/// let (a, b) = ring.take_send_iovecs().unwrap();
/// let BufResult(res, _) = file.write_vectored_at(SendSlices(a, b), pos).await;
/// ring.put_back_send(res.unwrap());
/// ```
#[allow(dead_code)] // used by consumers of the writev direction
pub struct SendSlices(pub &'static [u8], pub &'static [u8]);

impl IoVectoredBuf for SendSlices {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        [self.0, self.1].into_iter()
    }
}

/// Two mutable slices of the ring's writable region, submitable to compio's
/// `read_vectored` / `read_vectored_at` (one `readv` syscall).
///
/// ```ignore
/// let (a, b) = ring.take_recv_iovecs().unwrap();
/// let mut slices = RecvSlices(a, b);
/// let BufResult(res, _) = file.read_vectored_at(slices, pos).await;
/// ring.put_back_recv(res.unwrap());
/// ```
#[allow(dead_code)] // used by consumers of the readv direction
pub struct RecvSlices(pub &'static mut [u8], pub &'static mut [u8]);

impl IoVectoredBuf for RecvSlices {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        [&self.0[..], &self.1[..]].into_iter()
    }
}

impl IoVectoredBufMut for RecvSlices {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        // SAFETY: `&mut [u8]` has the same layout as `&mut [MaybeUninit<u8>]`.
        let a: &mut [MaybeUninit<u8>] = unsafe {
            core::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), self.0.len())
        };
        let b: &mut [MaybeUninit<u8>] = unsafe {
            core::slice::from_raw_parts_mut(self.1.as_mut_ptr().cast(), self.1.len())
        };
        [a, b].into_iter()
    }
}

impl SetLen for RecvSlices {
    unsafe fn set_len(&mut self, _len: usize) {
        // The initialized-length bookkeeping of the iovec pair is not used by
        // the ring; the caller returns the total via `put_back_recv`.
    }
}

extern crate std;

use std::{borrow::Borrow, io, ops::DerefMut, ptr};

use core::mem::MaybeUninit;

use compio::buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut, SetLen};
use compio::io::{AsyncRead, AsyncWrite};

use super::{
    error_::TxError,
    futures_::ParkFuture,
    rx_::RingRx,
    state_::{check_rx_readable, check_tx_writable, ParkSide, RingBuffer},
    tx_::RingTx,
};

impl<H, B> AsyncRead for RingRx<H, B, u8>
where
    H: Borrow<RingBuffer<B, u8>>,
    B: DerefMut<Target = [u8]>,
{
    async fn read<X: IoBufMut>(&mut self, mut buf: X) -> BufResult<usize, X> {
        loop {
            let ring: &RingBuffer<B, u8> = self.ring();
            let cap = buf.buf_capacity();
            #[allow(clippy::collapsible_if)]
            if cap > 0 {
                if let Ok((start, take)) = ring.try_read_at(cap) {
                    // try_read_at 可能返回跨末端环绕的区域；这里只取连续前缀，
                    // 剩余的环绕部分由下一次 read 继续读取。
                    let first = core::cmp::min(take, ring.capacity() - start);
                    let src = &ring.buffer_ref()[start..start + first];
                    let dst = buf.as_uninit();
                    // SAFETY: `first <= cap`, and both slices are valid for
                    // `first` bytes.
                    unsafe {
                        ptr::copy_nonoverlapping(
                            src.as_ptr(),
                            dst.as_mut_ptr().cast::<u8>(),
                            first,
                        );
                    }
                    // The first `first` bytes are now initialized.
                    unsafe { buf.set_len(first) };
                    ring.advance_read(first);
                    return BufResult(io::Result::Ok(first), buf);
                }
            }
            if ring.is_rx_closed() {
                // EOF
                return BufResult(io::Result::Ok(0), buf);
            }
            ParkFuture::new(ring, ParkSide::RxUser, check_rx_readable, cap).await;
        }
    }
}

impl<H, B> AsyncWrite for RingTx<H, B, u8>
where
    H: Borrow<RingBuffer<B, u8>>,
    B: DerefMut<Target = [u8]>,
{
    async fn write<X: IoBuf>(&mut self, buf: X) -> BufResult<usize, X> {
        let src: &[u8] = buf.as_init();
        loop {
            let ring: &RingBuffer<B, u8> = self.ring();
            if src.is_empty() {
                return BufResult(io::Result::Ok(0), buf);
            }
            match ring.try_write_at(src.len()) {
                Ok((start, take)) => {
                    // 同读侧：只取连续前缀，环绕部分由下一次 write 写入。
                    let first = core::cmp::min(take, ring.capacity() - start);
                    let dst = ring.buffer_uninit();
                    // SAFETY: `first <= dst.len() - start`.
                    unsafe {
                        ptr::copy_nonoverlapping(
                            src[..first].as_ptr(),
                            dst[start..start + first].as_mut_ptr().cast::<u8>(),
                            first,
                        );
                    }
                    ring.advance_write(first);
                    return BufResult(io::Result::Ok(first), buf);
                }
                Err(TxError::Stuffed(_)) => {
                    if ring.is_tx_closed() {
                        return BufResult(
                            io::Result::Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "tx end closed",
                            )),
                            buf,
                        );
                    }
                    ParkFuture::new(ring, ParkSide::TxUser, check_tx_writable, src.len()).await;
                }
                Err(TxError::Closing) => {
                    return BufResult(
                        io::Result::Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "tx end closed",
                        )),
                        buf,
                    );
                }
                Err(TxError::Argument) => unreachable!("[compio AsyncWrite] Argument"),
            }
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        // The ring has no internal buffering: written data is immediately
        // visible to the reader (user or kernel).
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.close();
        Ok(())
    }
}

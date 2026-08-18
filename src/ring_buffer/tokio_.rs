//! `tokio::io::AsyncRead` / `tokio::io::AsyncWrite` implementations
//! (feature `tokio`).

extern crate std;

use std::{
    borrow::Borrow,
    io,
    ops::DerefMut,
    pin::Pin,
    ptr,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::{
    error_::{RxError, TxError},
    rx_::RingRx,
    state_::RingBuffer,
    tx_::RingTx,
};

impl<H, B> AsyncRead for RingRx<H, B, u8>
where
    H: Borrow<RingBuffer<B, u8>>,
    B: DerefMut<Target = [u8]>,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        let ring: &RingBuffer<B, u8> = this.ring();
        loop {
            match ring.try_read_at(buf.remaining()) {
                Ok((start, take)) => {
                    // try_read_at 可能返回跨末端环绕的区域；适配层只取连续前缀，
                    // 剩余的环绕部分由下一次 poll 继续读取。
                    let first = core::cmp::min(take, ring.capacity() - start);
                    let src = &ring.buffer_ref()[start..start + first];
                    buf.put_slice(src);
                    ring.advance_read(first);
                    return Poll::Ready(Ok(()));
                }
                Err(RxError::Drained(_)) => {
                    let waiter = unsafe { &mut *this.waiter.get() };
                    waiter.waker = Some(cx.waker().clone());
                    ring.register_rx_user(waiter);
                    return Poll::Pending;
                }
                // EOF
                Err(RxError::Closing) => return Poll::Ready(Ok(())),
                Err(RxError::Argument) => unreachable!("[tokio poll_read] Argument"),
            }
        }
    }
}

impl<H, B> AsyncWrite for RingTx<H, B, u8>
where
    H: Borrow<RingBuffer<B, u8>>,
    B: DerefMut<Target = [u8]>,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        let ring: &RingBuffer<B, u8> = this.ring();
        loop {
            match ring.try_write_at(buf.len()) {
                Ok((start, take)) => {
                    // 同读侧：只取连续前缀，环绕部分由下一次 poll 写入。
                    let first = core::cmp::min(take, ring.capacity() - start);
                    let dst = ring.buffer_uninit();
                    // SAFETY: `first <= dst.len() - start`.
                    unsafe {
                        ptr::copy_nonoverlapping(
                            buf.as_ptr(),
                            dst[start..start + first].as_mut_ptr().cast::<u8>(),
                            first,
                        );
                    }
                    ring.advance_write(first);
                    return Poll::Ready(Ok(first));
                }
                Err(TxError::Stuffed(_)) => {
                    let waiter = unsafe { &mut *this.waiter.get() };
                    waiter.waker = Some(cx.waker().clone());
                    ring.register_tx_user(waiter);
                    return Poll::Pending;
                }
                Err(TxError::Closing) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "tx end closed",
                    )));
                }
                Err(TxError::Argument) => unreachable!("[tokio poll_write] Argument"),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // The ring has no internal buffering.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        this.close();
        Poll::Ready(Ok(()))
    }
}

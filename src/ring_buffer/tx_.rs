use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    mem::MaybeUninit,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_buff::{TrBuffIterTryWrite, TrBuffIterWrite};
use abs_sync::cancellation::{
    NonCancellableToken, TrCancellationToken, TrMayCancel,
};
use atomex::TrCmpxchOrderings;
use segm_buff::x_deps::{abs_buff, abs_sync};

use super::{
    buffer_::{RingBuffer, TxError},
    reclaim_::ReclSliceMut,
    sync_::{CtrlHint, Demand, IoCtx},
    Dual,
};

/// To move data into, or to put data into, the buffer.
pub struct BuffTx<B, P, T, O>(IoCtx<B, P, T, O>)
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings;

impl<B, P, T, O> BuffTx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    pub(super) fn new(ctx: IoCtx<B, P, T, O>) -> Self {
        ctx.state().incr_use_count();
        BuffTx(ctx)
    }

    pub fn try_write(
        &mut self,
        length: usize,
    ) -> Result<Dual<ReclSliceMut<'_, P, T, O>>, TxError<usize>> {
        self.0.buffer().try_write_(length)
    }

    pub fn write_async(
        &mut self,
        length: usize,
    ) -> WriteAsync<'_, B, P, T, O> {
        // Safe because IoCtx is `!Unpin`
        let mut io_ctx = unsafe {
            let mut pointer = NonNull::new_unchecked(&mut self.0);
            Pin::new_unchecked(pointer.as_mut())
        };
        let _ = io_ctx.as_mut().try_reset_demand();
        WriteAsync::new(io_ctx, length)
    }

    #[inline(always)]
    pub fn buffer(&self) -> &RingBuffer<P, T, O> {
        self.as_ref()
    }
}

impl<B, P, T, O> Drop for BuffTx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        let ctx = self.0.borrow_mut();
        let ctrl = ctx.state().decr_use_count();
        if matches!(ctrl, CtrlHint::MarkClose(_)) {
            ctx.buffer().state().mark_tx_closed();
        }
        #[cfg(test)]
        log::trace!("[BuffWrite::Drop] ctrl({ctrl})");
    }
}

impl<B, P, T, O> AsRef<RingBuffer<P, T, O>> for BuffTx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &RingBuffer<P, T, O> {
        self.0.buffer()
    }
}

impl<B, P, T, O> TrBuffIterWrite<T> for BuffTx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type SegmMut<'a> = ReclSliceMut<'a, P, T, O> where Self: 'a;
    type Segments<'a> = Dual<Self::SegmMut<'a>> where Self: 'a;
    type WriteAsync<'a> = WriteAsync<'a, B, P, T, O> where Self: 'a;
    type Err = TxError<usize>;

    #[inline]
    fn write_async(&mut self, length: usize) -> Self::WriteAsync<'_> {
        BuffTx::write_async(self, length)
    }
}

impl<B, P, T, O> TrBuffIterTryWrite<T> for BuffTx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    fn try_write(&mut self, length: usize) -> Result<
        <Self as TrBuffIterWrite<T>>::Segments<'_>,
        <Self as TrBuffIterWrite<T>>::Err,
    > {
        BuffTx::try_write(self, length)
    }
}

pub struct WriteAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    io_ctx_: Pin<&'a mut IoCtx<B, P, T, O>>,
    length_: usize,
}

impl<'a, B, P, T, O> WriteAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    pub(super) fn new(
        io_ctx: Pin<&'a mut IoCtx<B, P, T, O>>,
        length: usize,
    ) -> Self {
        WriteAsync {
            io_ctx_: io_ctx,
            length_: length,
        }
    }

    #[inline(always)]
    pub fn may_cancel_with<'f, C: TrCancellationToken>(
        self,
        cancel: Pin<&'f mut C>,
    ) -> WriteFuture<'a, 'f, C, B, P, T, O>
    where
        Self: 'f,
    {
        WriteFuture::new(self.io_ctx_, self.length_, cancel)
    }
}

impl<'a, B, P, T, O> IntoFuture for WriteAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type IntoFuture = WriteFuture<'a, 'a, NonCancellableToken, B, P, T, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        WriteFuture::new(self.io_ctx_, self.length_, cancel)
    }
}

impl<'a, B, P, T, O> TrMayCancel<'a> for WriteAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type MayCancelOutput = <<Self as IntoFuture>::IntoFuture as Future>::Output;

    #[inline(always)]
    fn may_cancel_with<'f, C: TrCancellationToken>(
        self,
        cancel: Pin<&'f mut C>,
    ) -> impl IntoFuture<Output = Self::MayCancelOutput>
    where
        Self: 'f,
    {
        WriteAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project]
pub struct WriteFuture<'ctx, 'tok, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    io_ctx_: Pin<&'ctx mut IoCtx<B, P, T, O>>,
    cancel_: Pin<&'tok mut C>,
    length_: usize,
    #[pin]demand_: Option<Demand<O>>,
}

impl<'ctx, 'tok, C, B, P, T, O> WriteFuture<'ctx, 'tok, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(
        io_ctx: Pin<&'ctx mut IoCtx<B, P, T, O>>,
        length: usize,
        cancel: Pin<&'tok mut C>,
    ) -> Self {
        WriteFuture {
            io_ctx_: io_ctx,
            cancel_: cancel,
            length_: length,
            demand_: Option::None,
        }
    }
}

impl<'ctx, C, B, P, T, O> Future
for WriteFuture<'ctx, '_, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type Output = Result<Dual<ReclSliceMut<'ctx, P, T, O>>, TxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let ring_buf: &'ctx RingBuffer<P, T, O> = unsafe {
            let ptr = this.io_ctx_.as_mut().get_unchecked_mut();
            NonNull::new_unchecked(ptr).as_ref().buffer()
        };
        loop {
            if let Option::Some(demand) = this.io_ctx_.as_mut().demand_mut() {
                let try_write = ring_buf.try_write_(*this.length_);
                let Result::Err(tx_err) = try_write else {
                    // try_write is ok
                    #[cfg(test)]
                    log::trace!("[WriteFuture::poll] enqueued({demand:p}) try_write ok");
                    let _ = ring_buf.state().dequeue_tx(demand);
                    return Poll::Ready(try_write);
                };
                let TxError::Stuffed(p) = tx_err else {
                    // try_write is not TxError::Stuffed
                    #[cfg(test)]
                    log::trace!("[WriteFuture::poll] enqueued({demand:p}) try_write err: {tx_err:?}");
                    let _ = ring_buf.state().dequeue_tx(demand);
                    return Poll::Ready(Result::Err(tx_err));
                };
                let fut_cancel = this
                    .cancel_
                    .as_mut()
                    .cancellation()
                    .into_future();
                pin_mut!(fut_cancel);
                if fut_cancel.poll(cx).is_ready() {
                    #[cfg(test)]
                    log::trace!("[WriteFuture::poll] enqueued({demand:p}) cancelled");
                    let _ = ring_buf.state().dequeue_tx(demand);
                    return Poll::Ready(Result::Err(TxError::Stuffed(p)));
                }
                break Poll::Pending;
            } else {
                let try_write = ring_buf.try_write_(*this.length_);
                let Result::Err(write_err) = try_write else {
                    // try_write is ok
                    return Poll::Ready(try_write);
                };
                let TxError::Stuffed(_) = write_err else {
                    // try_write is not TxError::Stuffed
                    #[cfg(test)]
                    log::trace!("[WriteFuture::poll] not queued try_write err: {write_err:?}");
                    return Poll::Ready(Result::Err(write_err));
                };
                let demand = Demand::new(
                    *this.length_,
                    Demand::producer_check,
                );
                let try_init = this
                    .io_ctx_
                    .as_mut()
                    .try_init_demand(demand);
                let Result::Ok(demand) = try_init else {
                    unreachable!("[WriteFuture::poll]")
                };
                let x = demand.try_init_waker(|| cx.waker().clone());
                assert!(x.is_ok());
                let x = ring_buf.state().enqueue_tx(demand);
                assert!(x);
                #[cfg(test)]
                log::trace!("[WriteFuture::poll] enqueued demand({demand:p})");
            }
        }
    }
}

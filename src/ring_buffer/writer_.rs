use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_buff::{TrBuffIterWrite, TrBuffIterTryWrite};
use abs_sync::{cancellation::*, x_deps::pin_utils};
use asyncex::x_deps::{abs_sync, atomex};
use atomex::TrCmpxchOrderings;

use super::{
    buffer_::{RingBuffer, TxError},
    reclaim_::ReclSliceMut,
    sync_::*,
    Dual,
};

pub struct BuffWrite<B, P, T, O>(B, PhantomData<RingBuffer<P, T, O>>)
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<B, P, T, O> BuffWrite<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(buff: B) -> Self {
        BuffWrite(buff, PhantomData)
    }

    pub fn try_write(
        &mut self,
        length: usize,
    ) -> Result<Dual<ReclSliceMut<'_, P, T, O>>, TxError<usize>> {
        self.0.borrow().try_write_(length)
    }

    pub fn write_async(
        &mut self,
        length: usize,
    ) -> WriteAsync<'_, B, P, T, O> {
        WriteAsync::new(self.0.borrow(), length)
    }

    #[inline(always)]
    pub fn buffer(&self) -> &RingBuffer<P, T, O> {
        self.borrow()
    }
}

impl<B, P, T, O> Drop for BuffWrite<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        self.0.borrow().state().mark_producer_closed()
    }
}

impl<B, P, T, O> Borrow<RingBuffer<P, T, O>> for BuffWrite<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn borrow(&self) -> &RingBuffer<P, T, O> {
        self.0.borrow()
    }
}

impl<B, P, T, O> TrBuffIterWrite<T> for BuffWrite<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type SliceMut<'a> = ReclSliceMut<'a, P, T, O> where Self: 'a;
    type BuffIter<'a> = Dual<Self::SliceMut<'a>> where Self: 'a;
    type Err = TxError<usize>;
    type WriteAsync<'a> = WriteAsync<'a, B, P, T, O> where Self: 'a;

    #[inline]
    fn write_async(&mut self, length: usize) -> Self::WriteAsync<'_> {
        BuffWrite::write_async(self, length)
    }
}

impl<B, P, T, O> TrBuffIterTryWrite<T> for BuffWrite<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    fn try_write(&mut self, length: usize) -> Result<
        <Self as TrBuffIterWrite<T>>::BuffIter<'_>,
        <Self as TrBuffIterWrite<T>>::Err,
    > {
        BuffWrite::try_write(self, length)
    }
}

pub struct WriteAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    _writer: PhantomData<&'a mut BuffWrite<B, P, T, O>>,
    buffer_: &'a RingBuffer<P, T, O>,
    length_: usize,
}

impl<'a, B, P, T, O> WriteAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    pub(super) fn new(
        buffer: &'a RingBuffer<P, T, O>,
        length: usize,
    ) -> Self {
        WriteAsync {
            _writer: PhantomData,
            buffer_: buffer,
            length_: length,
        }
    }

    #[inline(always)]
    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> WriteFuture<'a, C, B, P, T, O>
    where
        C: TrCancellationToken,
    {
        WriteFuture::new(self.buffer_, cancel, self.length_)
    }
}

impl<'a, B, P, T, O> IntoFuture for WriteAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type IntoFuture = WriteFuture<'a, NonCancellableToken, B, P, T, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        WriteFuture::new(self.buffer_, cancel, self.length_)
    }
}

impl<'a, B, P, T, O> TrIntoFutureMayCancel<'a>
for WriteAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type MayCancelOutput = <<Self as IntoFuture>::IntoFuture as Future>::Output;

    #[inline(always)]
    fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> impl Future<Output = Self::MayCancelOutput>
    where
        C: TrCancellationToken,
    {
        WriteAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project]
pub struct WriteFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    buffer_: &'a RingBuffer<P, T, O>,
    cancel_: Pin<&'a mut C>,
    length_: usize,
    #[pin]demand_: Option<Demand<O>>,
    _use_b_: PhantomData<&'a mut BuffWrite<B, P, T, O>>,
}

impl<'a, C, B, P, T, O> WriteFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn new(
        buffer: &'a RingBuffer<P, T, O>,
        cancel: Pin<&'a mut C>,
        length: usize,
    ) -> Self {
        WriteFuture {
            buffer_: buffer,
            cancel_: cancel,
            length_: length,
            demand_: Option::None,
            _use_b_: PhantomData,
        }
    }

    async fn write_async_(
        self: Pin<&mut Self>,
    ) -> Result<Dual<ReclSliceMut<'a, P, T, O>>, TxError<usize>> {
        let mut this = self.project();
        let length = *this.length_;
        let try_write = this.buffer_.try_write_(length);
        let Result::Err(write_err) = try_write else {
            return try_write;
        };
        let TxError::Stuffed(_) = write_err else {
            return Result::Err(write_err);
        };
        loop {
            if let Option::Some(demand) = this.demand_.as_ref().get_ref() {
                let x = this.buffer_.state().check_producer(demand);
                assert!(x, "[WriteFuture::write_async_] check_producer");
                let sign_recv = demand.signal.peeker();
                pin_mut!(sign_recv);
                let x = sign_recv
                    .peek_async()
                    .may_cancel_with(this.cancel_.as_mut())
                    .await;
                let _ = this.buffer_.state().abort_producer(demand);
                return if x.is_ok() {
                    this.buffer_.try_write_(length)
                } else {
                    
                    Result::Err(TxError::Stuffed(0usize))
                }
            } else {
                let opt = unsafe { this.demand_.as_mut().get_unchecked_mut() };
                let demand = Demand::new(
                    &mut |s| Demand::producer_check(s),
                );
                let replaced = opt.replace(demand);
                assert!(replaced.is_none());
                let Option::Some(demand) = opt.as_mut() else {
                    unreachable!("[WriteFuture::write_async_] opt")
                };
                let x = this.buffer_.state().enqueue_producer(demand);
                assert!(x)
            }
        }
    }
}

impl<'a, C, B, P, T, O> Future  for WriteFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type Output = Result<Dual<ReclSliceMut<'a, P, T, O>>, TxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let f = self.write_async_();
        pin_mut!(f);
        f.poll(cx)
    }
}

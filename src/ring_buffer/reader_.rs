use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    mem::MaybeUninit,
    pin::Pin,
    task::{Context, Poll},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_buff::{
    TrBuffIterPeek, TrBuffIterRead, TrBuffIterTryPeek, TrBuffIterTryRead,
};
use abs_sync::{cancellation::*, x_deps::pin_utils};
use asyncex::x_deps::{abs_sync, atomex};
use atomex::TrCmpxchOrderings;

use super::{
    buffer_::{DemandCtx, RingBuffer, RxError},
    reclaim_::ReclSliceRef,
    peeker_::*,
    sync_::*,
    Dual,
};

pub struct BuffRead<B, P, T, O>(DemandCtx<B, P, T, O>)
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<B, P, T, O> BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(buffer: B) -> Self {
        BuffRead(DemandCtx::new(buffer))
    }

    pub fn try_read(
        &mut self,
        length: usize,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        self.0.buffer().try_read_(length)
    }

    pub fn read_async(
        &mut self,
        length: usize,
    ) -> ReadAsync<'_, B, P, T, O> {
        ReadAsync::new(&mut self.0, length)
    }

    fn try_peek_(
        &mut self,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        self.0.buffer().try_peek_()
    }

    fn peek_async_(&mut self) -> PeekAsync<'_, B, P, T, O> {
        PeekAsync::new(&mut self.0)
    }

    pub fn into_peeker(self) -> BuffPeek<B, P, T, O> {
        let mut m = MaybeUninit::new(self);
        BuffPeek::<B, P, T, O>::new(unsafe {
            m.assume_init_mut().0.buffer()
        })
    }

    #[inline(always)]
    pub fn buffer(&self) -> &RingBuffer<P, T, O> {
        self.borrow()
    }
}

impl<B, P, T, O> From<BuffPeek<B, P, T, O>> for BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn from(value: BuffPeek<B, P, T, O>) -> Self {
        // Disable drop
        let m: MaybeUninit<_> = MaybeUninit::new(value);
        // Get field pointer
        let b = unsafe { m.assume_init_ref().as_ref() as *const B };
        // Manually move field
        BuffRead::new(unsafe { b.read() })
    }
}

impl<B, P, T, O> Drop for BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        self.0.borrow().state().mark_consumer_closed()
    }
}

impl<B, P, T, O> TrBuffIterRead<T> for BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type SliceRef<'a> = ReclSliceRef<'a, P, T, O> where Self: 'a;
    type BuffIter<'a> = Dual<Self::SliceRef<'a>> where Self: 'a;
    type Err = RxError<usize>;
    type ReadAsync<'a> = ReadAsync<'a, B, P, T, O> where Self: 'a;

    #[inline]
    fn read_async(&mut self, length: usize) -> Self::ReadAsync<'_> {
        BuffRead::read_async(self, length)
    }
}

impl<B, P, T, O> TrBuffIterTryRead<T> for BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[inline]
    fn try_read(&mut self, length: usize) -> Result<
        <Self as TrBuffIterRead<T>>::BuffIter<'_>,
        <Self as TrBuffIterRead<T>>::Err,
    > {
        BuffRead::try_read(self, length)
    }
}

impl<B, P, T, O> Borrow<RingBuffer<P, T, O>> for BuffRead<B, P, T, O>
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

impl<B, P, T, O> AsRef<Self> for BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<B, P, T, O> AsMut<Self> for BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn as_mut(&mut self) -> &mut Self {
        self
    }
}

impl<B, P, T, O> AsRef<B> for BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &B {
        &self.0
    }
}

impl<B, P, T, O> TrBuffIterPeek<T> for BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type SliceRef<'a> = ReclSliceRef<'a, P, T, O> where Self: 'a;
    type BuffIter<'a> = Dual<Self::SliceRef<'a>> where Self: 'a;
    type Err = RxError<usize>;
    type PeekAsync<'a> = PeekAsync<'a, B, P, T, O> where Self: 'a;

    #[inline(always)]
    fn peek_async(
        &mut self,
    ) -> Self::PeekAsync<'_> {
        PeekAsync::new(self.0.borrow(), skip)
    }
}

impl<B, P, T, O> TrBuffIterTryPeek<T> for BuffRead<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[inline]
    fn try_peek(
        &mut self,
    ) -> Result<
        <Self as TrBuffIterPeek<T>>::BuffIter<'_>,
        <Self as TrBuffIterPeek<T>>::Err,
    > {
        self.0.borrow().try_peek_()
    }
}

pub struct ReadAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    context_: &'a mut DemandCtx<B, P, T, O>,
    length_: usize,
}

impl<'a, B, P, T, O> ReadAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(
        context: &'a mut DemandCtx<B, P, T, O>,
        length: usize,
    ) -> Self {
        ReadAsync {
            context_: context,
            length_: length,
        }
    }

    #[inline(always)]
    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> ReadFuture<'a, C, B, P, T, O>
    where
        C: TrCancellationToken,
    {
        ReadFuture::new(self.reader_, self.length_, cancel)
    }
}

impl<'a, B, P, T, O> IntoFuture for ReadAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type IntoFuture = ReadFuture<'a, NonCancellableToken, B, P, T, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        ReadFuture::new(self.reader_, self.length_, cancel)
    }
}

impl<'a, B, P, T, O> TrIntoFutureMayCancel<'a> for ReadAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type MayCancelOutput =
        <<Self as IntoFuture>::IntoFuture as Future>::Output;

    #[inline(always)]
    fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> impl Future<Output = Self::MayCancelOutput>
    where
        C: TrCancellationToken,
    {
        ReadAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project(PinnedDrop)]
pub struct ReadFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[pin]reader_: &'a mut BuffRead<B, P, T, O>,
    length_: usize,
    cancel_: Pin<&'a mut C>,
}

impl<'a, C, B, P, T, O> ReadFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn new(
        reader: &'a mut BuffRead<B, P, T, O>,
        length: usize,
        cancel: Pin<&'a mut C>,
    ) -> Self {
        ReadFuture {
            reader_: reader,
            length_: length,
            cancel_: cancel,
        }
    }

    async fn read_async_(
        self: Pin<&mut Self>,
    ) -> Result<Dual<ReclSliceRef<'a, P, T, O>>, RxError<usize>> {
        let mut this = self.project();
        let try_read = this.reader_.buffer().try_read_(*this.length_);
        let Result::Err(read_err) = try_read else {
            return try_read;
        };
        let RxError::Drained(_) = read_err else {
            return Result::Err(read_err);
        };
        loop {
            if let Option::Some(demand) = this.demand_.as_ref().get_ref() {
                let x = this.buffer_.state().check_consumer(demand);
                assert!(x, "[ReadFuture::read_async_] check_consumer");
                let sign_recv = demand.signal.peeker();
                pin_mut!(sign_recv);
                let x = sign_recv
                    .peek_async()
                    .may_cancel_with(this.cancel_.as_mut())
                    .await;
                let _ = this.buffer_.state().abort_consumer(demand);
                return if x.is_ok() {
                    this.buffer_.try_read_(*this.length_)
                } else {
                    Result::Err(RxError::Drained(D::ZERO))
                }
            } else {
                let opt = unsafe { this.demand_.as_mut().get_unchecked_mut() };
                let demand = Demand::new(
                    &mut |s| Demand::consumer_check(s)
                );
                let replaced = opt.replace(demand);
                assert!(replaced.is_none());
                let Option::Some(demand) = opt.as_mut() else {
                    unreachable!("[ReadFuture::read_async_] opt")
                };
                let x = this.buffer_.state().enqueue_consumer(demand);
                #[cfg(test)]
                log::trace!("[ReadFuture::read_async_] enqueued");
                assert!(x)
            }
        }
    }
}

impl<'a, C, B, P, T, O> Future for ReadFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type Output = Result<Dual<ReclSliceRef<'a, P, T, O>>, RxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let f = self.read_async_();
        pin_mut!(f);
        f.poll(cx)
    }
}

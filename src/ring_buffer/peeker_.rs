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
    reader_::{BuffRead, ReadAsync},
    sync_::*,
    Dual,
};

pub struct BuffPeek<B, P, T, O>(DemandCtx<B, P, T, O>)
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<B, P, T, O> BuffPeek<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(buffer: B) -> Self {
        BuffPeek(DemandCtx::new(buffer))
    }

    #[inline]
    pub fn try_peek(
        &mut self,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        self.0.buffer().try_peek_() 
    }

    #[inline]
    pub fn peek_async(
        &mut self,
    ) -> PeekAsync<'_, B, P, T, O> {
        PeekAsync::new(&mut self.0)
    }

    #[inline]
    fn try_read_(
        &mut self,
        length: usize,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        self.0.buffer().try_read_(length)
    }

    #[inline]
    fn read_async_(&mut self, length: usize) -> ReadAsync<'_, B, P, T, O> {
        ReadAsync::new(&mut self.0, length)
    }
}

impl<B, P, T, O> From<BuffRead<B, P, T, O>> for BuffPeek<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn from(value: BuffRead<B, P, T, O>) -> Self {
        let m: MaybeUninit<_> = MaybeUninit::new(value);
        let b = unsafe {
            m.assume_init_ref().as_ref() as *const B
        };
        BuffPeek::new(unsafe { b.read() })
    }
}

impl<B, P, T, O> TrBuffIterPeek<T> for BuffPeek<B, P, T, O>
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

    #[inline]
    fn peek_async(&mut self) -> Self::PeekAsync<'_> {
        BuffPeek::peek_async(self)
    }
}

impl<B, P, T, O> TrBuffIterTryPeek<T> for BuffPeek<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{ 
    #[inline]
    fn try_peek(&mut self) -> Result<
        <Self as TrBuffIterPeek<T>>::BuffIter<'_>,
        <Self as TrBuffIterPeek<T>>::Err,
    > {
        BuffPeek::try_peek(self)
    }
}

impl<B, P, T, O> TrBuffIterRead<T> for BuffPeek<B, P, T, O>
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
        ReadAsync::new(&mut self.0, length)
    }
}

impl<B, P, T, O> TrBuffIterTryRead<T> for BuffPeek<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn try_read(&mut self, length: usize) -> Result<
        <Self as TrBuffIterRead<T>>::BuffIter<'_>,
        <Self as TrBuffIterRead<T>>::Err,
    > {
        BuffPeek::try_read_(self, length)
    }
}

impl<B, P, T, O> Borrow<RingBuffer<P, T, O>> for BuffPeek<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn borrow(&self) -> &RingBuffer<P, T, O> {
        self.0.buffer()
    }
}

impl<B, P, T, O> AsRef<Self> for BuffPeek<B, P, T, O>
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

impl<B, P, T, O> AsMut<Self> for BuffPeek<B, P, T, O>
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

pub struct PeekAsync<'a, B, P, T, O>(&'a mut DemandCtx<B, P, T, O>)
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<'a, B, P, T, O> PeekAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    pub(super) fn new(context: &'a mut DemandCtx<B, P, T, O>) -> Self {
        PeekAsync(context)
    }

    #[inline(always)]
    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> PeekFuture<'a, C, B, P, T, O>
    where
        C: TrCancellationToken,
    {
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, B, P, T, O> IntoFuture for PeekAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type IntoFuture = PeekFuture<'a, NonCancellableToken, B, P, T, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, B, P, T, O> TrIntoFutureMayCancel<'a> for PeekAsync<'a, B, P, T, O>
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
        PeekAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project]
pub struct PeekFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[pin]context_: &'a mut DemandCtx<B, P, T, O>,
    cancel_: Pin<&'a mut C>,
}

impl<'a, C, B, P, T, O> PeekFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn new(
        context: &'a mut DemandCtx<B, P, T, O>,
        cancel: Pin<&'a mut C>,
    ) -> Self {
        PeekFuture {
            context_: context,
            cancel_: cancel,
        }
    }

    async fn peek_async_(self: Pin<&mut Self>) -> <Self as Future>::Output {
        let mut this = self.project();
        let skip_len = *this.skip_;
        let try_peek = this.buffer_.try_peek_(skip_len);
        let Result::Err(peek_err) = try_peek else {
            return try_peek;
        };
        let RxError::Drained(_) = peek_err else {
            return Result::Err(peek_err);
        };
        let mut check = move |s: &RwState<D, O>| {
            let (_, _, l, _) = s.load_state();
            let l = d_to_usize(l, "");
            l > skip_len
        };
        loop {
            if let Option::Some(demand) = this.demand_.as_ref().get_ref() {
                let x = this.buffer_.state().check_consumer(demand);
                assert!(x, "[PeekFuture::peek_async_] check_consumer");
                let signal_recv = demand.signal.peeker();
                pin_mut!(signal_recv);
                let x = signal_recv
                    .peek_async()
                    .may_cancel_with(this.cancel_.as_mut())
                    .await;
                let _ = this.buffer_.state().abort_consumer(demand);
                return if x.is_ok() {
                    this.buffer_.try_peek_(skip_len)
                } else {
                    Result::Err(RxError::Drained(D::ZERO))
                }
            } else {
                let opt = unsafe { this.demand_.as_mut().get_unchecked_mut() };
                let demand = Demand::new(&mut check);
                let replaced = opt.replace(demand);
                assert!(replaced.is_none());
                let Option::Some(demand) = opt.as_mut() else {
                    unreachable!("[PeekFuture::peek_async_] opt")
                };
                let x = this.buffer_.state().enqueue_consumer(demand);
                assert!(x)
            }
        }
    }
}

impl<'a, C, B, P, T, O> Future for PeekFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type Output = Result<Dual<ReclSliceRef<'a, P, T, O>>, RxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let f = self.peek_async_();
        pin_mut!(f);
        f.poll(cx)
    }
}

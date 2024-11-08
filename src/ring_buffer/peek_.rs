use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    marker::PhantomData,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_buff::{TrBuffIterPeek, TrBuffIterTryPeek};
use abs_sync::{cancellation::*, x_deps::pin_utils};
use asyncex::x_deps::{abs_sync, atomex};
use atomex::TrCmpxchOrderings;

use super::{
    buffer_::{IoCtx, RingBuffer, RxError},
    reclaim_::ReclSliceRef,
    sync_::{Demand, RwState},
    Dual,
};

pub struct BuffPeek<X, B, P, T, O>(X, PhantomData<IoCtx<B, P, T, O>>)
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<X, B, P, T, O> BuffPeek<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(ctx: X) -> Self {
        BuffPeek(ctx, PhantomData)
    }

    #[inline]
    pub fn try_peek(
        &mut self,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        self.0.borrow().buffer().try_peek_() 
    }

    #[inline]
    pub fn peek_async(&mut self) -> PeekAsync<'_, B, P, T, O> {
        // Safe because IoCtx is !Unpin
        let io_ctx = unsafe {
            let mut pointer = NonNull::new_unchecked(self.0.borrow_mut());
            Pin::new_unchecked(pointer.as_mut())
        };
        PeekAsync::new(io_ctx)
    }
}

impl<X, B, P, T, O> Drop for BuffPeek<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        let ctx = self.0.borrow_mut();
        if ctx.use_count().dec() == 1usize {
            ctx.buffer().state().mark_consumer_closed()
        }
    }
}

impl<X, B, P, T, O> TrBuffIterPeek<T> for BuffPeek<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
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

impl<X, B, P, T, O> TrBuffIterTryPeek<T> for BuffPeek<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
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

impl<X, B, P, T, O> Borrow<RingBuffer<P, T, O>> for BuffPeek<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[inline]
    fn borrow(&self) -> &RingBuffer<P, T, O> {
        self.0.borrow().buffer()
    }
}

pub struct PeekAsync<'a, B, P, T, O>(Pin<&'a mut IoCtx<B, P, T, O>>)
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
    pub(super) fn new(io_ctx: Pin<&'a mut IoCtx<B, P, T, O>>) -> Self {
        PeekAsync(io_ctx)
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
    io_ctx_: Pin<&'a mut IoCtx<B, P, T, O>>,
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
        context: Pin<&'a mut IoCtx<B, P, T, O>>,
        cancel: Pin<&'a mut C>,
    ) -> Self {
        PeekFuture {
            io_ctx_: context,
            cancel_: cancel,
        }
    }

    async fn peek_async_(self: Pin<&mut Self>) -> <Self as Future>::Output {
        let this = self.project();
        let mut p_ctx = unsafe {
            let ptr = this.io_ctx_.as_mut().get_unchecked_mut();
            NonNull::new_unchecked(ptr)
        };
        let p_ring_buf = unsafe { 
            let ring_buf = p_ctx.as_ref().buffer();
            let ptr = ring_buf as *const _ as *mut RingBuffer<P, T, O>;
            NonNull::new_unchecked(ptr)
        };
        let try_peek = unsafe { p_ring_buf.as_ref().try_peek_() };
        let Result::Err(peek_err) = try_peek else {
            return try_peek;
        };
        let RxError::Drained(_) = peek_err else {
            return Result::Err(peek_err);
        };
        let mut check = move |s: &RwState<O>| {
            let i = s.load_state();
            i.reader_length > 0
        };
        loop {
            let ring_buf = unsafe { p_ring_buf.as_ref() };
            if let Option::Some(demand) = this.io_ctx_.demand() {
                let x = ring_buf.state().check_consumer(demand);
                assert!(x, "[PeekFuture::peek_async_] check_consumer");
                let signal_recv = demand.signal.peeker();
                pin_mut!(signal_recv);
                let x = signal_recv
                    .peek_async()
                    .may_cancel_with(this.cancel_.as_mut())
                    .await;
                let _ = ring_buf.state().abort_consumer(demand);
                return if x.is_ok() {
                    unsafe { p_ring_buf.as_ref().try_peek_() }
                } else {
                    Result::Err(RxError::Drained(0usize))
                }
            } else {
                let demand = Demand::new(&mut check);
                let try_init = unsafe {
                    let ctx_pin = Pin::new_unchecked(p_ctx.as_mut());
                    ctx_pin.try_init_demand(demand)
                };
                let Result::Ok(demand_ref) = try_init else { continue; };
                let x = ring_buf.state().enqueue_consumer(demand_ref);
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

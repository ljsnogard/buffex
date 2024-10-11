use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    marker::PhantomData,
    mem::MaybeUninit,
    pin::Pin,
    task::{Context, Poll},
};
use pin_project::{pin_project, pinned_drop};
use pin_utils::pin_mut;

use abs_buff::{IoSliceRef, NoReclaim, TrBuffPeeker, TrBuffReader};
use abs_sync::{cancellation::*, x_deps::pin_utils};
use atomex::{
    x_deps::funty,
    TrAtomicData, TrCmpxchOrderings,
};
use asyncex::x_deps::{abs_sync, atomex};

use crate::ring_buffer::sync_::RwState;

use super::{
    buffer_::{RingBuffer, SliceRef, RxError},
    reader_::*,
    sync_::{d_to_usize, Demand},
    TrAsyncRingBuffPeeker, TrAsyncRingBuffReader
};

pub struct Peeker<B, P, T, D, O>(B, PhantomData<RingBuffer<P, T, D, O>>)
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings;

impl<B, P, T, D, O> Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(buff: B) -> Self {
        Peeker(buff, PhantomData)
    }

    #[inline(always)]
    pub fn can_peek(&mut self, skip: usize) -> bool {
        self.0.borrow().can_peek_(skip)
    }

    #[inline(always)]
    pub fn try_peek(
        &mut self,
        skip: usize,
    ) -> Result<IoSliceRef<'_, T, NoReclaim<T>>, RxError<D>> {
        self.0.borrow().try_peek_(skip)
    }

    #[inline(always)]
    pub fn peek_async(
        &mut self,
        skip: usize,
    ) -> PeekAsync<'_, B, P, T, D, O> {
        PeekAsync::new(self.0.borrow(), skip)
    }
}

impl<B, P, T, D, O> Clone for Peeker<B, P, T, D, O>
where
    B: Clone + Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn clone(&self) -> Self {
        Peeker::new(self.0.clone())
    }
}

impl<B, P, T, D, O> From<Reader<B, P, T, D, O>> for Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn from(value: Reader<B, P, T, D, O>) -> Self {
        let m: MaybeUninit<_> = MaybeUninit::new(value);
        let b = unsafe {
            m.assume_init_ref().as_ref() as *const B
        };
        Peeker::new(unsafe { b.read() })
    }
}

impl<B, P, T, D, O> TrBuffPeeker<T> for Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type BuffRef<'a> = IoSliceRef<'a, T, NoReclaim<T>> where Self: 'a;
    type Error = RxError<D>;
    type PeekAsync<'a> = PeekAsync<'a, B, P, T, D, O> where Self: 'a;

    #[inline(always)]
    fn can_peek(&mut self, skip: usize) -> bool {
        Peeker::can_peek(self, skip)
    }

    #[inline(always)]
    fn peek_async(
        &mut self,
        skip: usize,
    ) -> Self::PeekAsync<'_> {
        Peeker::peek_async(self, skip)
    }
}

impl<B, P, T, D, O> TrBuffReader<T> for Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type BuffRef<'a> = SliceRef<'a, P, T, D, O> where Self: 'a;
    type Error = RxError<D>;
    type ReadAsync<'a> = ReadAsync<'a, B, P, T, D, O> where Self: 'a;

    fn can_read(&mut self) -> bool {
        self.0.borrow().can_read_()
    }

    #[inline(always)]
    fn read_async(
        &mut self,
        length: usize,
    ) -> Self::ReadAsync<'_> {
        ReadAsync::new(self.0.borrow(), length)
    }
}

impl<B, P, T, D, O> TrAsyncRingBuffReader<T> for Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type RingBuffer = RingBuffer<P, T, D, O>;
    type Peeker = Self;

    fn try_read(&mut self, length: usize) -> Result<
        <Self as TrBuffReader<T>>::BuffRef<'_>,
        <Self as TrBuffReader<T>>::Error,
    > {
        self.0.borrow().try_read_(length)
    }
}

impl<B, P, T, D, O> Borrow<RingBuffer<P, T, D, O>> for Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn borrow(&self) -> &RingBuffer<P, T, D, O> {
        self.0.borrow()
    }
}

impl<B, P, T, D, O> AsRef<Self> for Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<B, P, T, D, O> AsMut<Self> for Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn as_mut(&mut self) -> &mut Self {
        self
    }
}

impl<B, P, T, D, O> AsRef<B> for Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &B {
        &self.0
    }
}

impl<B, P, T, D, O> TrAsyncRingBuffPeeker<T> for Peeker<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type RingBuffer = RingBuffer<P, T, D, O>;
    type Reader = Self;
 
    #[inline(always)]
    fn try_peek(&mut self, skip: usize) -> Result<
        <Self as TrBuffPeeker<T>>::BuffRef<'_>,
        <Self as TrBuffPeeker<T>>::Error,
    > {
        Peeker::try_peek(self, skip)
    }
}

pub struct PeekAsync<'a, B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    _peeker: PhantomData<&'a mut Peeker<B, P, T, D, O>>,
    buffer_: &'a RingBuffer<P, T, D, O>,
    skip_: usize,
}

impl<'a, B, P, T, D, O> PeekAsync<'a, B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    pub(super) fn new(buffer: &'a RingBuffer<P, T, D, O>, skip: usize) -> Self {
        PeekAsync {
            _peeker: PhantomData,
            buffer_: buffer,
            skip_: skip,
        }
    }

    #[inline(always)]
    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> PeekFuture<'a, C, B, P, T, D, O>
    where
        C: TrCancellationToken,
    {
        PeekFuture::new(self.buffer_, cancel, self.skip_)
    }
}

impl<'a, B, P, T, D, O> IntoFuture for PeekAsync<'a, B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type IntoFuture = PeekFuture<'a, NonCancellableToken, B, P, T, D, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        PeekFuture::new(self.buffer_, cancel, self.skip_)
    }
}

impl<'a, B, P, T, D, O> TrIntoFutureMayCancel<'a>
for PeekAsync<'a, B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
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

#[pin_project(PinnedDrop)]
pub struct PeekFuture<'a, C, B, P, T, D, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    buffer_: &'a RingBuffer<P, T, D, O>,
    cancel_: Pin<&'a mut C>,
    skip_: usize,
    #[pin]demand_: Option<Demand<D, O>>,
    _use_b_: PhantomData<&'a mut Peeker<B, P, T, D, O>>,
}

impl<'a, C, B, P, T, D, O> PeekFuture<'a, C, B, P, T, D, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn new(
        buffer: &'a RingBuffer<P, T, D, O>,
        cancel: Pin<&'a mut C>,
        skip: usize,
    ) -> Self {
        PeekFuture {
            buffer_: buffer,
            cancel_: cancel,
            skip_: skip,
            demand_: Option::None,
            _use_b_: PhantomData,
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

impl<'a, C, B, P, T, D, O> Future for PeekFuture<'a, C, B, P, T, D, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type Output = Result<IoSliceRef<'a, T, NoReclaim<T>>, RxError<D>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let f = self.peek_async_();
        pin_mut!(f);
        f.poll(cx)
    }
}

#[pinned_drop]
impl<C, B, P, T, D, O> PinnedDrop for PeekFuture<'_, C, B, P, T, D, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn drop(self: Pin<&mut Self>) {
        let mut this = self.project();
        let opt = unsafe { this.demand_.as_mut().get_unchecked_mut() };
        let Option::Some(demand) = opt else {
            return;
        };
        this.buffer_.state().dequeue_consumer(demand);
    }
}

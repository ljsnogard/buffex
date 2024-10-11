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

use super::{
    buffer_::{RingBuffer, RxError, SliceRef},
    peeker_::*,
    sync_::*,
    TrAsyncRingBuffPeeker, TrAsyncRingBuffReader
};

pub struct Reader<B, P, T, D, O>(B, PhantomData<RingBuffer<P, T, D, O>>)
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings;

impl<B, P, T, D, O> Reader<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(buff: B) -> Self {
        Reader(buff, PhantomData)
    }

    pub fn can_read(&mut self) -> bool {
        self.0.borrow().can_read_()
    }

    pub fn try_read(
        &mut self,
        length: usize,
    ) -> Result<SliceRef<'_, P, T, D, O>, RxError<D>> {
        self.0.borrow().try_read_(length)
    }

    pub fn read_async(
        &mut self,
        length: usize,
    ) -> ReadAsync<'_, B, P, T, D, O> {
        ReadAsync::new(self.0.borrow(), length)
    }

    pub fn into_peeker(self) -> Peeker<B, P, T, D, O> {
        let mut m = MaybeUninit::new(self);
        let b = unsafe { &mut m.assume_init_mut().0 as *mut B};
        Peeker::new(unsafe { b.read() })
    }

    #[inline(always)]
    pub fn buffer(&self) -> &RingBuffer<P, T, D, O> {
        self.borrow()
    }
}

impl<B, P, T, D, O> From<Peeker<B, P, T, D, O>> for Reader<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn from(value: Peeker<B, P, T, D, O>) -> Self {
        // Disable drop
        let m: MaybeUninit<_> = MaybeUninit::new(value);
        // Get field pointer
        let b = unsafe { m.assume_init_ref().as_ref() as *const B };
        // Manually move field
        Reader::new(unsafe { b.read() })
    }
}

impl<B, P, T, D, O> Drop for Reader<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        self.0.borrow().state().mark_consumer_closed()
    }
}

impl<B, P, T, D, O> TrBuffReader<T> for Reader<B, P, T, D, O>
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

    #[inline(always)]
    fn can_read(&mut self) -> bool {
        Reader::can_read(self)
    }

    #[inline(always)]
    fn read_async(&mut self, length: usize) -> Self::ReadAsync<'_> {
        Reader::read_async(self, length)
    }
}

impl<B, P, T, D, O> TrAsyncRingBuffReader<T> for Reader<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type RingBuffer = RingBuffer<P, T, D, O>;
    type Peeker = Self;

    #[inline(always)]
    fn try_read(&mut self, length: usize) -> Result<
        <Self as TrBuffReader<T>>::BuffRef<'_>,
        <Self as TrBuffReader<T>>::Error,
    > {
        Reader::try_read(self, length)
    }
}

impl<B, P, T, D, O> Borrow<RingBuffer<P, T, D, O>> for Reader<B, P, T, D, O>
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

impl<B, P, T, D, O> AsRef<Self> for Reader<B, P, T, D, O>
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

impl<B, P, T, D, O> AsMut<Self> for Reader<B, P, T, D, O>
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

impl<B, P, T, D, O> AsRef<B> for Reader<B, P, T, D, O>
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

impl<B, P, T, D, O> TrBuffPeeker<T> for Reader<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type BuffRef<'a> = IoSliceRef<'a, T, NoReclaim<T>> where Self: 'a;
    type PeekAsync<'a> = PeekAsync<'a, B, P, T, D, O> where Self: 'a;
    type Error = RxError<D>;

    #[inline(always)]
    fn can_peek(&mut self, skip: usize) -> bool {
        self.0.borrow().can_peek_(skip)
    }

    #[inline(always)]
    fn peek_async(
        &mut self,
        skip: usize,
    ) -> Self::PeekAsync<'_> {
        PeekAsync::new(self.0.borrow(), skip)
    }
}

impl<B, P, T, D, O> TrAsyncRingBuffPeeker<T> for Reader<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type RingBuffer = RingBuffer<P, T, D, O>;
    type Reader = Self;

    fn try_peek(&mut self, skip: usize) -> Result<
        <Self as TrBuffPeeker<T>>::BuffRef<'_>,
        <Self as TrBuffPeeker<T>>::Error,
    > {
        self.0.borrow().try_peek_(skip)
    }
}

pub struct ReadAsync<'a, B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    _reader: PhantomData<&'a mut Reader<B, P, T, D, O>>,
    buffer_: &'a RingBuffer<P, T, D, O>,
    length_: usize,
}

impl<'a, B, P, T, D, O> ReadAsync<'a, B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    pub(super) fn new(
        buffer: &'a RingBuffer<P, T, D, O>,
        length: usize,
    ) -> Self {
        ReadAsync {
            _reader: PhantomData,
            buffer_: buffer,
            length_: length,
        }
    }

    #[inline(always)]
    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> ReadFuture<'a, C, B, P, T, D, O>
    where
        C: TrCancellationToken,
    {
        ReadFuture::new(self.buffer_, self.length_, cancel)
    }
}

impl<'a, B, P, T, D, O> IntoFuture for ReadAsync<'a, B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type IntoFuture = ReadFuture<'a, NonCancellableToken, B, P, T, D, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        ReadFuture::new(self.buffer_, self.length_, cancel)
    }
}

impl<'a, B, P, T, D, O> TrIntoFutureMayCancel<'a>
for ReadAsync<'a, B, P, T, D, O>
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
        ReadAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project(PinnedDrop)]
pub struct ReadFuture<'a, C, B, P, T, D, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    buffer_: &'a RingBuffer<P, T, D, O>,
    length_: usize,
    cancel_: Pin<&'a mut C>,
    #[pin]demand_: Option<Demand<D, O>>,
    _use_b_: PhantomData<&'a mut Reader<B, P, T, D, O>>,
}

impl<'a, C, B, P, T, D, O> ReadFuture<'a, C, B, P, T, D, O>
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
        length: usize,
        cancel: Pin<&'a mut C>,
    ) -> Self {
        ReadFuture {
            buffer_: buffer,
            length_: length,
            cancel_: cancel,
            demand_: Option::None,
            _use_b_: PhantomData,
        }
    }

    async fn read_async_(
        self: Pin<&mut Self>,
    ) -> Result<SliceRef<'a, P, T, D, O>, RxError<D>> {
        let mut this = self.project();
        let try_read = this.buffer_.try_read_(*this.length_);
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

impl<'a, C, B, P, T, D, O> Future
for ReadFuture<'a, C, B, P, T, D, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type Output = Result<SliceRef<'a, P, T, D, O>, RxError<D>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let f = self.read_async_();
        pin_mut!(f);
        f.poll(cx)
    }
}

#[pinned_drop]
impl<C, B, P, T, D, O> PinnedDrop for ReadFuture<'_, C, B, P, T, D, O>
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

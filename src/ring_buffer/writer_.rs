use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use pin_project::{pin_project, pinned_drop};
use pin_utils::pin_mut;

use abs_buff::TrBuffWriter;
use abs_sync::{cancellation::*, x_deps::pin_utils};
use atomex::{
    x_deps::funty,
    TrAtomicData, TrCmpxchOrderings,
};
use asyncex::x_deps::{abs_sync, atomex};

use super::{
    buffer_::{RingBuffer, SliceMut, TxError},
    sync_::*,
    TrAsyncRingBuffWriter,
};

pub struct Writer<B, P, T, D, O>(B, PhantomData<RingBuffer<P, T, D, O>>)
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings;

impl<B, P, T, D, O> Writer<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(buff: B) -> Self {
        Writer(buff, PhantomData)
    }

    pub fn can_write(&mut self) -> bool {
        self.0.borrow().can_write_()
    }

    pub fn try_write(
        &mut self,
        length: usize,
    ) -> Result<SliceMut<'_, P, T, D, O>, TxError<D>> {
        self.0.borrow().try_write_(length)
    }

    pub fn write_async(
        &mut self,
        length: usize,
    ) -> WriteAsync<'_, B, P, T, D, O> {
        WriteAsync::new(self.0.borrow(), length)
    }

    #[inline(always)]
    pub fn buffer(&self) -> &RingBuffer<P, T, D, O> {
        self.borrow()
    }
}

impl<B, P, T, D, O> Drop for Writer<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        self.0.borrow().state().mark_producer_closed()
    }
}

impl<B, P, T, D, O> Borrow<RingBuffer<P, T, D, O>> for Writer<B, P, T, D, O>
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

impl<B, P, T, D, O> TrBuffWriter<T> for Writer<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type BuffMut<'a> = SliceMut<'a, P, T, D, O> where Self: 'a;
    type Error = TxError<D>;
    type WriteAsync<'a> = WriteAsync<'a, B, P, T, D, O> where Self: 'a;

    #[inline(always)]
    fn can_write(&mut self) -> bool {
        Writer::can_write(self)
    }

    #[inline(always)]
    fn write_async(&mut self, length: usize) -> Self::WriteAsync<'_> {
        Writer::write_async(self, length)
    }
}

impl<B, P, T, D, O> TrAsyncRingBuffWriter<T> for Writer<B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type RingBuffer = RingBuffer<P, T, D, O>;

    #[inline(always)]
    fn try_write(&mut self, length: usize) -> Result<
        <Self as TrBuffWriter<T>>::BuffMut<'_>,
        <Self as TrBuffWriter<T>>::Error,
    > {
        Writer::try_write(self, length)
    }
}

pub struct WriteAsync<'a, B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    _writer: PhantomData<&'a mut Writer<B, P, T, D, O>>,
    buffer_: &'a RingBuffer<P, T, D, O>,
    length_: usize,
}

impl<'a, B, P, T, D, O> WriteAsync<'a, B, P, T, D, O>
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
    ) -> WriteFuture<'a, C, B, P, T, D, O>
    where
        C: TrCancellationToken,
    {
        WriteFuture::new(self.buffer_, cancel, self.length_)
    }
}

impl<'a, B, P, T, D, O> IntoFuture for WriteAsync<'a, B, P, T, D, O>
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type IntoFuture = WriteFuture<'a, NonCancellableToken, B, P, T, D, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        WriteFuture::new(self.buffer_, cancel, self.length_)
    }
}

impl<'a, B, P, T, D, O> TrIntoFutureMayCancel<'a>
for WriteAsync<'a, B, P, T, D, O>
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
        WriteAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project(PinnedDrop)]
pub struct WriteFuture<'a, C, B, P, T, D, O>
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
    length_: usize,
    #[pin]demand_: Option<Demand<D, O>>,
    _use_b_: PhantomData<&'a mut Writer<B, P, T, D, O>>,
}

impl<'a, C, B, P, T, D, O> WriteFuture<'a, C, B, P, T, D, O>
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
    ) -> Result<SliceMut<'a, P, T, D, O>, TxError<D>> {
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
                    
                    Result::Err(TxError::Stuffed(D::ZERO))
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

impl<'a, C, B, P, T, D, O> Future 
for WriteFuture<'a, C, B, P, T, D, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type Output = Result<SliceMut<'a, P, T, D, O>, TxError<D>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let f = self.write_async_();
        pin_mut!(f);
        f.poll(cx)
    }
}

#[pinned_drop]
impl<C, B, P, T, D, O> PinnedDrop for WriteFuture<'_, C, B, P, T, D, O>
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
        this.buffer_.state().dequeue_producer(demand);
    }
}

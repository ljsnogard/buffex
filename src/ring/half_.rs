use core::{
    future::{Future, IntoFuture},
    marker::PhantomData,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::AtomicPtr,
    task::{Context, Poll, Waker},
};

use abs_buff::{Demand, x_deps::abs_cancel};
use abs_cancel::{NonCancellableToken, TrMayCancel, TrCancellationToken};
use atomex::AtomexPtrOwned;
use atomic_sync::x_deps::atomex;

use super::{
    error_::{ConsumerError, ProducerError},
    hook_::TrPark,
    ring_core_::RingState,
};

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ConsumerHook
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

#[derive(Debug)]
pub struct Consumer<T> {
    ring_half_: RingHalf_,
    _unused_t_: PhantomData<fn() -> T>,
}

impl<T> Consumer<T> {
    pub const fn new() -> Self {
        Consumer {
            ring_half_: RingHalf_::new_(),
            _unused_t_: PhantomData,
        }
    }

    #[inline]
    fn pending_demand_(&self) -> Option<&Demand<usize>> {
        self.ring_half_.pending_demand_()
    }
}

impl<T> Default for Consumer<T> {
    fn default() -> Self {
        Consumer::new()
    }
}

impl<T> TrPark for Consumer<T> {
    type ParkAsync<'f> = ConsumerParkAsync<'f, T> where Self: 'f;
    type Err = ConsumerError<usize>;

    fn wake(&self, state: &RingState) {
        let Option::Some(demand) = self.pending_demand_() else {
            return;
        };
        let min_demand = demand.min().copied().unwrap_or(1usize);
        let pos = state.io_pos();
        if pos.data_size() < min_demand {
            return;
        }
        let Option::Some(waker_ref) = &self.ring_half_.wake_slot_ else {
            return;
        };
        waker_ref.wake_by_ref();
    }

    #[inline]
    fn park_async<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Self::ParkAsync<'f> {
        ConsumerParkAsync::new_(self, demand)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ConsumerParkAsync
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub struct ConsumerParkAsync<'a, T>(&'a mut Consumer<T>);

impl<'a, T> ConsumerParkAsync<'a, T> {
    fn new_(
        consumer: &'a mut Consumer<T>,
        demand: &'a Demand<usize>,
    ) -> Self {
        consumer.ring_half_.init_demand_(demand);
        ConsumerParkAsync(consumer)
    }
}

impl<'a, T> IntoFuture for ConsumerParkAsync<'a, T> {
    type IntoFuture = ConsumerParkFuture<'a, T, NonCancellableToken>;
    type Output = Option<ConsumerError<usize>>;

    fn into_future(self) -> Self::IntoFuture {
        let consumer = self.0;
        let cancel = NonCancellableToken::new();
        ConsumerParkFuture::new_(consumer, cancel)
    }
}

impl<'a, T> TrMayCancel<'a> for ConsumerParkAsync<'a, T> {
    type MayCancelFuture<'f, C> = ConsumerParkFuture<'a, T, C>
    where
        'f: 'a,
        Self: 'f,
        C: 'f + TrCancellationToken;

    type MayCancelOutput = Option<ConsumerError<usize>>;

    fn may_cancel_with<C>(self, cancel: C) -> Self::MayCancelFuture<'a, C>
    where
        C: 'a + TrCancellationToken,
    {
        let consumer = self.0;
        ConsumerParkFuture::new_(consumer, cancel)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ConsumerParkFuture
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 将内部的 waker 注册到 consumer hook 里面，等待外部唤醒。如果在唤醒前收到
/// cancellation 信号则返回 `Poll::Ready(Some(ConsumerError::Cancelled))`
pub struct ConsumerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    consumer_: &'a mut Consumer<T>,
    cancel_tok_: Option<K>,
    cancel_sig_: Option<<K::ChildToken as TrCancellationToken>::Cancellation>,
}

impl<'a, T, K> ConsumerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    const fn new_(
        consumer: &'a mut Consumer<T>,
        cancel: K,
    ) -> Self {
        ConsumerParkFuture {
            consumer_: consumer,
            cancel_tok_: Option::Some(cancel),
            cancel_sig_: Option::None,
        }
    }
}

impl<'a, T, K> Future for ConsumerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    type Output = Option<ConsumerError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let make_cancelled = || ConsumerError::Cancelled;
        poll_park_future_(self, make_cancelled, cx)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ProducerHook
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub struct Producer<T> {
    ring_half_: RingHalf_,
    _unused_t_: PhantomData<fn() -> T>,
}

impl<T> Producer<T> {
    pub const fn new() -> Self {
        Producer {
            ring_half_: RingHalf_::new_(),
            _unused_t_: PhantomData,
        }
    }

    #[inline]
    fn pending_demand_(&self) -> Option<&Demand<usize>> {
        self.ring_half_.pending_demand_()
    }
}

impl<T> Default for Producer<T> {
    fn default() -> Self {
        Producer::new()
    }
}

impl<T> TrPark for Producer<T> {
    type ParkAsync<'f> = ProducerParkAsync<'f, T> where Self: 'f;
    type Err = ProducerError<usize>;

    fn wake(&self, state: &RingState) {
        let Option::Some(demand) = self.pending_demand_() else {
            return;
        };
        let min_demand = demand.min().copied().unwrap_or(1usize);
        let pos = state.io_pos();
        if pos.free_size() < min_demand {
            return;
        }
        let Option::Some(waker_ref) = &self.ring_half_.wake_slot_ else {
            return;
        };
        waker_ref.wake_by_ref();
    }

    #[inline]
    fn park_async<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Self::ParkAsync<'f> {
        ProducerParkAsync::new_(self, demand)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ProducerParkAsync
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub struct ProducerParkAsync<'a, T>(&'a mut Producer<T>);

impl<'a, T> ProducerParkAsync<'a, T> {
    fn new_(
        producer: &'a mut Producer<T>,
        demand: &'a Demand<usize>,
    ) -> Self {
        producer.ring_half_.init_demand_(demand);
        ProducerParkAsync(producer)
    }
}

impl<'a, T> IntoFuture for ProducerParkAsync<'a, T> {
    type IntoFuture = ProducerParkFuture<'a, T, NonCancellableToken>;
    type Output = Option<ProducerError<usize>>;

    fn into_future(self) -> Self::IntoFuture {
        let producer = self.0;
        let cancel = NonCancellableToken::new();
        ProducerParkFuture::new(producer, cancel)
    }
}

impl<'a, T> TrMayCancel<'a> for ProducerParkAsync<'a, T> {
    type MayCancelFuture<'f, C> = ProducerParkFuture<'a, T, C>
    where
        'f: 'a,
        Self: 'f,
        C: 'f + TrCancellationToken;

    type MayCancelOutput = Option<ProducerError<usize>>;

    fn may_cancel_with<C>(
        self,
        cancel: C,
    ) -> Self::MayCancelFuture<'a, C>
    where
        C: 'a + TrCancellationToken
    {
        let producer = self.0;
        ProducerParkFuture::new(producer, cancel)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ConsumerParkFuture
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 将内部的 waker 注册到 consumer hook 里面，等待外部唤醒。如果在唤醒前收到
/// cancellation 信号则返回 `Poll::Ready(Some(ConsumerError::Cancelled))`
pub struct ProducerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    producer_: &'a mut Producer<T>,
    cancel_tok_: Option<K>,
    cancel_sig_: Option<<K::ChildToken as TrCancellationToken>::Cancellation>,
}

impl<'a, T, K> ProducerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    const fn new(producer: &'a mut Producer<T>, cancel: K) -> Self {
        ProducerParkFuture {
            producer_: producer,
            cancel_tok_: Option::Some(cancel),
            cancel_sig_: Option::None,
        }
    }
}

impl<'a, T, K> Future for ProducerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    type Output = Option<ProducerError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let make_cancelled = || ProducerError::Cancelled;
        poll_park_future_(self, make_cancelled, cx)
    }
}

trait TrAsRingHalf_ {
    fn as_half_mut_(&mut self) -> &mut RingHalf_;
}

impl<T> TrAsRingHalf_ for Consumer<T> {
    fn as_half_mut_(&mut self) -> &mut RingHalf_ {
        &mut self.ring_half_
    }
}

impl<T> TrAsRingHalf_ for Producer<T> {
    fn as_half_mut_(&mut self) -> &mut RingHalf_ {
        &mut self.ring_half_
    }
}

#[derive(Debug)]
struct RingHalf_ {
    wake_slot_: Option<Waker>,
    opt_demand_: AtomexPtrOwned<Demand<usize>>,
}

impl RingHalf_ {
    const fn new_() -> Self {
        RingHalf_ {
            wake_slot_: Option::None,
            opt_demand_: AtomexPtrOwned::new(AtomicPtr::new(ptr::null_mut())),
        }
    }

    #[inline]
    fn init_demand_(&self, demand: &Demand<usize>) -> bool {
        self.opt_demand_.try_spin_init(NonNull::from(demand)).is_ok()
    }

    #[inline]
    fn pending_demand_(&self) -> Option<&Demand<usize>> {
        let ptr = self.opt_demand_.load()?;
        Option::Some(unsafe { ptr.as_ref() })
    }

    #[inline]
    fn reset_demand_(&self) -> bool {
        self.opt_demand_.try_reset().is_ok()
    }
}

trait TrFutBorrowRingHalf_: Future {
    type Half: TrAsRingHalf_;
    type CancelTok: TrCancellationToken;

    fn half_mut(&mut self) -> &mut Self::Half;

    fn cancel_sig(
        &mut self,
    ) -> &mut Option<
        <<Self::CancelTok as TrCancellationToken>::ChildToken
            as TrCancellationToken>::Cancellation
    >;

    fn cancel_tok(
        &mut self
    ) -> &mut Option<Self::CancelTok>;
}

impl<'a, T, K> TrFutBorrowRingHalf_ for ConsumerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    type Half = Consumer<T>;
    type CancelTok = K;

    fn half_mut(&mut self) -> &mut Self::Half {
        self.consumer_
    }

    fn cancel_sig(
        &mut self,
    ) -> &mut Option<
        <<Self::CancelTok as TrCancellationToken>::ChildToken
            as TrCancellationToken>::Cancellation
    > {
        &mut self.cancel_sig_
    }

    fn cancel_tok(
        &mut self
    ) -> &mut Option<Self::CancelTok> {
        &mut self.cancel_tok_
    }
}

impl<'a, T, K> TrFutBorrowRingHalf_ for ProducerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    type Half = Producer<T>;
    type CancelTok = K;

    fn half_mut(&mut self) -> &mut Self::Half {
        self.producer_
    }

    fn cancel_sig(
        &mut self,
    ) -> &mut Option<
        <<Self::CancelTok as TrCancellationToken>::ChildToken
            as TrCancellationToken>::Cancellation
    > {
        &mut self.cancel_sig_
    }

    fn cancel_tok(
        &mut self
    ) -> &mut Option<Self::CancelTok> {
        &mut self.cancel_tok_
    }
}

/// 实现一个与具体运行时无关的手动唤醒，与 Cancellation Token 竞争信号
/// 哪一个先到达
fn poll_park_future_<F, E>(
    future: Pin<&mut F>,
    make_cancelled: impl FnOnce() -> E,
    cx: &mut Context<'_>,
) -> Poll<Option<E>>
where
    F: TrFutBorrowRingHalf_,
{
    let this = unsafe { future.get_unchecked_mut() };
    // 没有 waker 时，表明这是第一次 poll。
    let x = if this.half_mut().as_half_mut_().wake_slot_.is_none() {
        let slot = &mut this.half_mut().as_half_mut_().wake_slot_;
        *slot = Some(cx.waker().clone());
        // 第一次 poll 时取得 cancellation signal。
        if this.cancel_sig().is_none() {
            let child_tok = this.cancel_tok()
                .as_ref()
                .map(|tk| tk.child_token())
                .expect("cancel token already taken");

            *this.cancel_sig() = Some(child_tok.cancellation());
        }
        // cancellation signal 与 consumer hook 共用当前 waker。
        let Option::Some(cancel_sig) = this.cancel_sig().as_mut() else {
            unreachable!()
        };
        let f = unsafe { Pin::new_unchecked(cancel_sig) };
        if f.poll(cx).is_ready() {
            Poll::Ready(Option::Some(make_cancelled()))
        } else {
            Poll::Pending
        }
    } else {
        if this.cancel_tok().as_ref().is_some_and(|tk| tk.is_cancelled()) {
            Poll::Ready(Option::Some(make_cancelled()))
        } else {
            Poll::Ready(Option::None)
        }
    };
    // 手动取消 half 与 demand 的绑定
    if x.is_ready() {
        let half_mut = this.half_mut().as_half_mut_();
        half_mut.reset_demand_();
        half_mut.wake_slot_ = Option::None;
    }
    x
}

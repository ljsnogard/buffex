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
        let min_demand = demand.min().unwrap_or(1usize);
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

/// `park_async` 返回的适配器。它在构造时就把本次等待的 `Demand` 绑定到 `RingHalf_` 上，
/// 因此在转换成 [`ConsumerParkFuture`] 之前被 drop 时也必须释放绑定（见下面的 `Drop`）。
pub struct ConsumerParkAsync<'a, T>(Option<&'a mut Consumer<T>>);

impl<'a, T> ConsumerParkAsync<'a, T> {
    fn new_(
        consumer: &'a mut Consumer<T>,
        demand: &'a Demand<usize>,
    ) -> Self {
        consumer.ring_half_.init_demand_(demand);
        ConsumerParkAsync(Option::Some(consumer))
    }

    /// 取出内部借用；适配器在一次 `park_async` 里只允许转换一次。
    fn take_consumer_(&mut self) -> &'a mut Consumer<T> {
        self.0.take().expect("park adapter already converted")
    }
}

impl<'a, T> Drop for ConsumerParkAsync<'a, T> {
    fn drop(&mut self) {
        // 未转换成 future 就被丢弃：绑定还挂在 half 上，必须在这里释放。
        if let Option::Some(consumer) = self.0.as_mut() {
            consumer.as_half_mut_().release_();
        }
    }
}

impl<'a, T> IntoFuture for ConsumerParkAsync<'a, T> {
    type IntoFuture = ConsumerParkFuture<'a, T, NonCancellableToken>;
    type Output = Option<ConsumerError<usize>>;

    fn into_future(mut self) -> Self::IntoFuture {
        let consumer = self.take_consumer_();
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

    fn may_cancel_with<C>(mut self, cancel: C) -> Self::MayCancelFuture<'a, C>
    where
        C: 'a + TrCancellationToken,
    {
        let consumer = self.take_consumer_();
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
        let min_demand = demand.min().unwrap_or(1usize);
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

/// `park_async` 返回的适配器。同 [`ConsumerParkAsync`]：构造时已绑定 `Demand`，
/// 未转换成 [`ProducerParkFuture`] 就被 drop 时也必须释放绑定。
pub struct ProducerParkAsync<'a, T>(Option<&'a mut Producer<T>>);

impl<'a, T> ProducerParkAsync<'a, T> {
    fn new_(
        producer: &'a mut Producer<T>,
        demand: &'a Demand<usize>,
    ) -> Self {
        producer.ring_half_.init_demand_(demand);
        ProducerParkAsync(Option::Some(producer))
    }

    /// 取出内部借用；适配器在一次 `park_async` 里只允许转换一次。
    fn take_producer_(&mut self) -> &'a mut Producer<T> {
        self.0.take().expect("park adapter already converted")
    }
}

impl<'a, T> Drop for ProducerParkAsync<'a, T> {
    fn drop(&mut self) {
        // 未转换成 future 就被丢弃：绑定还挂在 half 上，必须在这里释放。
        if let Option::Some(producer) = self.0.as_mut() {
            producer.as_half_mut_().release_();
        }
    }
}

impl<'a, T> IntoFuture for ProducerParkAsync<'a, T> {
    type IntoFuture = ProducerParkFuture<'a, T, NonCancellableToken>;
    type Output = Option<ProducerError<usize>>;

    fn into_future(mut self) -> Self::IntoFuture {
        let producer = self.take_producer_();
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
        mut self,
        cancel: C,
    ) -> Self::MayCancelFuture<'a, C>
    where
        C: 'a + TrCancellationToken
    {
        let producer = self.take_producer_();
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

    /// 解除与当前等待者的绑定：清空唤醒槽位并释放 demand 的占用。
    ///
    /// park future 无论是**正常结束**（被唤醒 / 取消，poll 收尾时调用）还是**中途被
    /// drop**（`select` 落败、外层 future 被丢弃，`Drop` 里调用）都必须调用它：
    ///
    /// * 不清空 `wake_slot_`，下一次 park 的首次 poll 会把残留槽位误判成「已经注册
    ///   过」，于是直接 `Ready(None)`，`ring_*_async` 的循环在**一次 poll 内**同步空转；
    /// * 不释放 `opt_demand_`，下一次 park 的 `init_demand_` 会静默失败（残留的旧
    ///   demand 继续生效），对端 `wake()` 还会按过时的下限判定，并可能解引用已经
    ///   释放的 `Demand`。
    #[inline]
    fn release_(&mut self) {
        self.reset_demand_();
        self.wake_slot_ = Option::None;
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
        this.half_mut().as_half_mut_().release_();
    }
    x
}

impl<'a, T, K> Drop for ConsumerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    fn drop(&mut self) {
        // 挂起状态下被丢弃（取消 / `select` 落败）时 poll 的收尾逻辑不会执行，
        // 必须在这里释放绑定，否则残留状态会污染下一次 park。
        self.consumer_.as_half_mut_().release_();
    }
}

impl<'a, T, K> Drop for ProducerParkFuture<'a, T, K>
where
    K: TrCancellationToken,
{
    fn drop(&mut self) {
        // 同 `ConsumerParkFuture`：挂起中被丢弃也要释放绑定。
        self.producer_.as_half_mut_().release_();
    }
}

#[cfg(test)]
mod tests_ {
    use super::*;

    /// 校验 park future 中途被 drop 时释放对 `RingHalf_` 的占用。
    /// - 测试目标：park future 完成注册（首次 poll 挂起）后被丢弃（取消 / `select`
    ///   落败）时，必须清空 `wake_slot_` 并释放 `opt_demand_`。否则下一次 park 的首次
    ///   poll 会把残留槽位误判成「已经注册过」而立即 `Ready(None)`（同步空转），且对端
    ///   `wake()` 可能解引用已经释放的 `Demand`。
    /// - 测试手段：读端与写端各构造一个 park future，用空 waker poll 一次确认它挂起，
    ///   随后 drop；再各丢弃一个**未转换成 future** 的 `park_async` 适配器（它在构造时
    ///   就已经完成了 demand 绑定）。
    /// - 判定标准：上述 drop 之后，两侧的 `wake_slot_` 都为空、`pending_demand_` 都为
    ///   `None`。
    #[test]
    fn dropped_park_future_releases_half_binding_() {
        let demand = Demand::at_least(1);

        // -- 读端 --
        let mut consumer = Consumer::<u8>::new();
        {
            let mut park = std::boxed::Box::pin(
                consumer.park_async(&demand).into_future(),
            );
            let mut cx = Context::from_waker(Waker::noop());
            assert!(
                park.as_mut().poll(&mut cx).is_pending(),
                "首次 park 应挂起（非取消令牌的 cancellation 永不就绪）"
            );
        } // 此处 drop：必须释放绑定
        assert!(
            consumer.ring_half_.wake_slot_.is_none(),
            "读端 park future drop 后应清空唤醒槽位"
        );
        assert!(
            consumer.pending_demand_().is_none(),
            "读端 park future drop 后应释放 demand 绑定"
        );

        // -- 写端 --
        let mut producer = Producer::<u8>::new();
        {
            let mut park = std::boxed::Box::pin(
                producer.park_async(&demand).into_future(),
            );
            let mut cx = Context::from_waker(Waker::noop());
            assert!(
                park.as_mut().poll(&mut cx).is_pending(),
                "首次 park 应挂起（非取消令牌的 cancellation 永不就绪）"
            );
        }
        assert!(
            producer.ring_half_.wake_slot_.is_none(),
            "写端 park future drop 后应清空唤醒槽位"
        );
        assert!(
            producer.pending_demand_().is_none(),
            "写端 park future drop 后应释放 demand 绑定"
        );

        // -- 适配器未转换成 future 就被 drop --
        // `park_async` 在构造适配器时已调用 `init_demand_`，此时绑定就已经建立，
        // 因此丢弃适配器也必须释放它。
        let mut consumer_ = Consumer::<u8>::new();
        drop(consumer_.park_async(&demand));
        assert!(
            consumer_.pending_demand_().is_none(),
            "未转换就被 drop 的读端适配器应释放 demand 绑定"
        );

        let mut producer_ = Producer::<u8>::new();
        drop(producer_.park_async(&demand));
        assert!(
            producer_.pending_demand_().is_none(),
            "未转换就被 drop 的写端适配器应释放 demand 绑定"
        );
    }
}

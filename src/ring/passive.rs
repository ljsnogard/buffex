use core::{
    future::{Future, IntoFuture},
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use abs_buff::{
    Demand,
    buffer::{TrAsBuffer, TrAsBufferMut},
    x_deps::abs_cancel,
};
use abs_cancel::{NonCancellableToken, TrMayCancel, TrCancellationToken};

use super::{
    error_::{ConsumerError, ProducerError},
    hook_::{TrConsumerHook, TrProducerHook, TrPark},
};

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ConsumerHook
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub struct ConsumerHook<B, T>
where
    B: TrAsBuffer<T>,
{
    wake_slot_: Option<Waker>,
    opt_demand_: Option<Demand<usize>>,
    _using_b_: PhantomData<fn() -> B>,
    _using_t_: PhantomData<fn() -> T>,
}

impl<B, T> TrConsumerHook<T> for ConsumerHook<B, T>
where
    B: TrAsBuffer<T>,
{
    type Buff = B;

    fn init_once(&mut self, buf: &Self::Buff, pos: &super::IoPos) {
        let _ = (buf, pos);
    }

    fn handle_event(&self, _: &Self::Buff, pos: &super::IoPos) {
        let Option::Some(demand) = &self.opt_demand_ else {
            return;
        };
        let min_demand = demand.min().copied().unwrap_or(1usize);
        if pos.data_size() < min_demand {
            return;
        }
        let Option::Some(waker_ref) = &self.wake_slot_ else {
            return;
        };
        waker_ref.wake_by_ref();
    }
}

impl<B, T> TrPark for ConsumerHook<B, T>
where
    B: TrAsBuffer<T>,
{
    type ParkAsync<'f> = ConsumerParkAsync<'f, B, T> where Self: 'f;
    type Err = ConsumerError<usize>;

    #[inline]
    fn park_async(&mut self) -> Self::ParkAsync<'_> {
        ConsumerParkAsync::new(self)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ConsumerParkAsync
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub struct ConsumerParkAsync<'a, B, T>
where
    B: TrAsBuffer<T>,
{
    hook_: &'a mut ConsumerHook<B, T>,
}

impl<'a, B, T> ConsumerParkAsync<'a, B, T>
where
    B: TrAsBuffer<T>,
{
    const fn new(hook: &'a mut ConsumerHook<B, T>) -> Self {
        ConsumerParkAsync { hook_: hook }
    }
}

impl<'a, B, T> IntoFuture for ConsumerParkAsync<'a, B, T>
where
    B: TrAsBuffer<T>,
{
    type IntoFuture = ConsumerParkFuture<'a, B, T, NonCancellableToken>;
    type Output = Option<ConsumerError<usize>>;

    fn into_future(self) -> Self::IntoFuture {
        let hook = self.hook_;
        let cancel = NonCancellableToken::new();
        ConsumerParkFuture::new(hook, cancel)
    }
}

impl<'a, B, T> TrMayCancel<'a> for ConsumerParkAsync<'a, B, T>
where
    B: TrAsBuffer<T>,
{
    type MayCancelFuture<'f, C> = ConsumerParkFuture<'a, B, T, C>
    where
        'f: 'a,
        Self: 'f,
        C: 'f + TrCancellationToken;

    type MayCancelOutput = Option<ConsumerError<usize>>;

    fn may_cancel_with<C>(
        self,
        cancel: C,
    ) -> Self::MayCancelFuture<'a, C>
    where
        C: 'a + TrCancellationToken
    {
        let hook = self.hook_;
        ConsumerParkFuture::new(hook, cancel)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ConsumerParkFuture
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 将内部的 waker 注册到 consumer hook 里面，等待外部唤醒。如果在唤醒前收到
/// cancellation 信号则返回 `Poll::Ready(Some(ConsumerError::Cancelled))`
pub struct ConsumerParkFuture<'a, B, T, K>
where
    B: TrAsBuffer<T>,
    K: TrCancellationToken,
{
    hook_: &'a mut ConsumerHook<B, T>,
    cancel_tok_: Option<K>,
    cancel_sig_: Option<<K::ChildToken as TrCancellationToken>::Cancellation>,
}

impl<'a, B, T, K> ConsumerParkFuture<'a, B, T, K>
where
    B: TrAsBuffer<T>,
    K: TrCancellationToken,
{
    const fn new(hook: &'a mut ConsumerHook<B, T>, cancel: K) -> Self {
        ConsumerParkFuture {
            hook_: hook,
            cancel_tok_: Option::Some(cancel),
            cancel_sig_: Option::None,
        }
    }
}

impl<'a, B, T, K> Future for ConsumerParkFuture<'a, B, T, K>
where
    B: TrAsBuffer<T>,
    K: TrCancellationToken,
{
    type Output = Option<ConsumerError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        // consumer hook 没有 waker 时，表明这是第一次 poll。
        if this.hook_.wake_slot_.is_none() {
            this.hook_.wake_slot_ = Some(cx.waker().clone());
            // 第一次 poll 时取得 cancellation signal。
            if this.cancel_sig_.is_none() {
                let child_tok = this.cancel_tok_
                    .as_ref()
                    .map(|tk| tk.child_token())
                    .expect("cancel token already taken");

                this.cancel_sig_ = Some(child_tok.cancellation());
            }
            // cancellation signal 与 consumer hook 共用当前 waker。
            let Option::Some(cancel_sig) = this.cancel_sig_.as_mut() else {
                unreachable!()
            };
            let f = unsafe { Pin::new_unchecked(cancel_sig) };
            if f.poll(cx).is_ready() {
                Poll::Ready(Some(ConsumerError::Cancelled))
            } else {
                Poll::Pending
            }
        } else {
            if this.cancel_tok_.as_ref().is_some_and(|tk| tk.is_cancelled()) {
                Poll::Ready(Option::Some(ConsumerError::Cancelled))
            } else {
                Poll::Ready(Option::None)
            }
        }
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ProducerHook
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub struct ProducerHook<B, T>
where
    B: TrAsBufferMut<T>,
{
    wake_slot_: Option<Waker>,
    opt_demand_: Option<Demand<usize>>,
    _using_b_: PhantomData<fn() -> B>,
    _using_t_: PhantomData<fn() -> T>,
}

impl<B, T> TrProducerHook<T> for ConsumerHook<B, T>
where
    B: TrAsBufferMut<T>,
{
    type Buff = B;

    fn init_once(&mut self, buf: &Self::Buff, pos: &super::IoPos) {
        let _ = (buf, pos);
    }

    fn handle_event(&self, _: &Self::Buff, pos: &super::IoPos) {
        let Option::Some(demand) = &self.opt_demand_ else {
            return;
        };
        let min_demand = demand.min().copied().unwrap_or(1usize);
        if pos.data_size() < min_demand {
            return;
        }
        let Option::Some(waker_ref) = &self.wake_slot_ else {
            return;
        };
        waker_ref.wake_by_ref();
    }
}

impl<B, T> TrPark for ProducerHook<B, T>
where
    B: TrAsBufferMut<T>,
{
    type ParkAsync<'f> = ProducerParkAsync<'f, B, T> where Self: 'f;
    type Err = ProducerError<usize>;

    #[inline]
    fn park_async(&mut self) -> Self::ParkAsync<'_> {
        ProducerParkAsync::new(self)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ProducerParkAsync
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub struct ProducerParkAsync<'a, B, T>
where
    B: TrAsBufferMut<T>,
{
    hook_: &'a mut ProducerHook<B, T>,
}

impl<'a, B, T> ProducerParkAsync<'a, B, T>
where
    B: TrAsBufferMut<T>,
{
    const fn new(hook: &'a mut ProducerHook<B, T>) -> Self {
        ProducerParkAsync { hook_: hook }
    }
}

impl<'a, B, T> IntoFuture for ProducerParkAsync<'a, B, T>
where
    B: TrAsBufferMut<T>,
{
    type IntoFuture = ProducerParkFuture<'a, B, T, NonCancellableToken>;
    type Output = Option<ProducerError<usize>>;

    fn into_future(self) -> Self::IntoFuture {
        let hook = self.hook_;
        let cancel = NonCancellableToken::new();
        ProducerParkFuture::new(hook, cancel)
    }
}

impl<'a, B, T> TrMayCancel<'a> for ProducerParkAsync<'a, B, T>
where
    B: TrAsBufferMut<T>,
{
    type MayCancelFuture<'f, C> = ProducerParkFuture<'a, B, T, C>
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
        let hook = self.hook_;
        ProducerParkFuture::new(hook, cancel)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// passive::ConsumerParkFuture
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 将内部的 waker 注册到 consumer hook 里面，等待外部唤醒。如果在唤醒前收到
/// cancellation 信号则返回 `Poll::Ready(Some(ConsumerError::Cancelled))`
pub struct ProducerParkFuture<'a, B, T, K>
where
    B: TrAsBufferMut<T>,
    K: TrCancellationToken,
{
    hook_: &'a mut ProducerHook<B, T>,
    cancel_tok_: Option<K>,
    cancel_sig_: Option<<K::ChildToken as TrCancellationToken>::Cancellation>,
}

impl<'a, B, T, K> ProducerParkFuture<'a, B, T, K>
where
    B: TrAsBufferMut<T>,
    K: TrCancellationToken,
{
    const fn new(hook: &'a mut ProducerHook<B, T>, cancel: K) -> Self {
        ProducerParkFuture {
            hook_: hook,
            cancel_tok_: Option::Some(cancel),
            cancel_sig_: Option::None,
        }
    }
}

impl<'a, B, T, K> Future for ProducerParkFuture<'a, B, T, K>
where
    B: TrAsBufferMut<T>,
    K: TrCancellationToken,
{
    type Output = Option<ProducerError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        // consumer hook 没有 waker 时，表明这是第一次 poll。
        if this.hook_.wake_slot_.is_none() {
            this.hook_.wake_slot_ = Some(cx.waker().clone());
            // 第一次 poll 时取得 cancellation signal。
            if this.cancel_sig_.is_none() {
                let child_tok = this.cancel_tok_
                    .as_ref()
                    .map(|tk| tk.child_token())
                    .expect("cancel token already taken");

                this.cancel_sig_ = Some(child_tok.cancellation());
            }
            // cancellation signal 与 consumer hook 共用当前 waker。
            let Option::Some(cancel_sig) = this.cancel_sig_.as_mut() else {
                unreachable!()
            };
            let f = unsafe { Pin::new_unchecked(cancel_sig) };
            if f.poll(cx).is_ready() {
                Poll::Ready(Some(ProducerError::Cancelled))
            } else {
                Poll::Pending
            }
        } else {
            if this.cancel_tok_.as_ref().is_some_and(|tk| tk.is_cancelled()) {
                Poll::Ready(Option::Some(ProducerError::Cancelled))
            } else {
                Poll::Ready(Option::None)
            }
        }
    }
}

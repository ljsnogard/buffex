//! 有界 SPSC 队列：对 [`circular_buff`](crate::circular_buff) 被动 × 被动半部的
//! **薄包装**。
//!
//! # 组成
//!
//! * **承载**：消息存放在 [`circular_buff`](crate::circular_buff) 的环形缓冲里，
//!   单元类型是内部的槽位协议 [`SlotMsg<T>`](super::slot_)——公开 API 只呈现用户
//!   载荷 `T`；
//! * **命名与收敛**：把 `(Producer, Consumer)` 呈现为 [`SpscSender`] /
//!   [`SpscReceiver`]，并把构建链收敛成 [`SpscChannel::with_capacity`] +
//!   [`SpscChannel::into_parts`]；
//! * **异步迭代**：两端都实现
//!   [`TrAsyncIterator`](abs_async_iter::TrAsyncIterator)。
//!
//! 本模块没有自己的同步原语（SPSC 不需要锁）、没有自己的缓冲。`T` 不需要 `Copy`，
//! 也不需要 `Clone`。
//!
//! # 发送侧的两类入口
//!
//! | 入口 | 占用格数 | 场景 |
//! | --- | --- | --- |
//! | [`SpscSender::send`] | 1 | 单体消息，载荷现成 |
//! | [`SpscSender::fill_bulk`] | `n + 1` | 一次连续放 `n` 条（载荷可不可 `Clone`） |
//!
//! `fill_bulk` 会在段首写入一个**预告头**，记录本帧的成员数；随后把
//! [`BulkWriter`] 交给调用者自己 `push`。**预告头额外占一格**——`n` 的语义始终是
//! 「用户消息条数」，协议开销由实现内部吸收并计入容量。
//!
//! # 空洞与已写前缀
//!
//! 生产者只填了 `k < n` 条时，`BulkWriter` 的 `Drop` 会把剩下的格补成**空洞**
//! （内部协议里的默认值）。于是本帧被整体提交，消费端按预告头收完 `n` 格、丢弃其中
//! 的空洞，于是：
//!
//! * **写满** ⇒ 消费端完整收到 bulk；
//! * **只写了一部分**（含中途放弃 / panic 展开）⇒ 消费端仍收到**已写好的那部分**。
//!
//! 空洞是**内部协议**：消费端读到即丢弃并视作已读，绝不交给上层调用者。
//!
//! # 单写者 / 单读者是前提
//!
//! 底层核心按 SPSC 约定实现（`Send + Sync` 的 `unsafe impl` 论证见
//! `circular_buff::core_`）。把 [`SpscSender`] 分发给多个任务会绕过该前提；需要多
//! 写者请用 [`MpscChannel`](super::MpscChannel)。

use core::mem::MaybeUninit;

use abs_async_iter::TrAsyncIterator;
use abs_buff::{Demand, gen_may_cancel_future, x_deps::abs_cancel};
use abs_cancel::{TrCancellationToken, TrMayCancel};
use mm_ptr::{
    Owned,
    x_deps::abs_mm::mem_alloc::{CoreAlloc, TrMalloc},
};

use super::{
    ChannelError,
    segm_::{
        BulkWriter, pop_msg_, push_bulk_head_, push_closing_wait_, push_payload_,
        try_push_closing_,
    },
    slot_::SlotMsg,
};
use crate::circular_buff::{
    BufConsumer, BufProducer, Consumer, Producer,
    builder::{BuilderError, CircularBuffBuilder, ConsumerSetBuilder},
    core_::CircCore,
    reclaim_::WriterReclaim,
};

/// 迭代路径统一使用的需求区间：至少 1 个单元、不设上限。
const ANY_: Demand<usize> = Demand::at_least(1);

/// 预留「尽可能大」的一段的需求。
///
/// 之所以用它而不是 `exactly(n)`：段会被**放在返回值里**交给调用者，因此它借用的
/// `Demand` 必须活到那时——`exactly(n)` 的 `n` 是运行期值、只能放在局部变量里，段就
/// 活不过函数返回。这里改用 `'static` 的「有多少要多少」，并在预留前用
/// [`free_size`](SpscSender::free_size) 自己把「够不够 `n + 1` 格」判掉；真正提交的
/// 格数由已写入量决定，因此多预留的部分不会被提交。
static BULK_DEMAND_: Demand<usize> = Demand::at_least(1);

// ---------------------------------------------------------------------------
// 具体类型别名
// ---------------------------------------------------------------------------

/// 环形缓冲的单元类型（内部槽位协议）。
pub(super) type Msg<T> = SlotMsg<T>;

/// 队列的数据承载：堆上的一块 `MaybeUninit<Msg<T>>`。
pub(super) type BufOf<T, A> = Owned<[MaybeUninit<Msg<T>>], A>;

/// 队列核心（被动生产端 × 被动消费端）。
pub(super) type CoreOf<T, A> =
    CircCore<BufProducer<Msg<T>>, BufConsumer<Msg<T>>, BufOf<T, A>, Msg<T>>;

/// 写提交器。
pub(super) type WrReclaim<'a, T, A> = WriterReclaim<'a, CoreOf<T, A>>;

/// 队列的默认数据承载（对外仅作为「这个队列自带缓冲」的说明，不含协议细节）。
pub type DefaultBuf<T, A = CoreAlloc> = Owned<[MaybeUninit<T>], A>;

// ---------------------------------------------------------------------------
// 构造入口
// ---------------------------------------------------------------------------

/// SPSC 队列的构造入口：只保存容量，直到
/// [`into_parts`](SpscChannel::into_parts) 才真正分配核心。
///
/// # Examples
///
/// ```no_run
/// use buffex::channels::SpscChannel;
///
/// # async fn demo() -> Result<(), buffex::channels::ChannelError> {
/// let (mut tx, mut rx) = SpscChannel::with_capacity(1024)
///     .expect("容量非法")
///     .into_parts::<u8, buffex::circular_buff::CoreAlloc>()
///     .await
///     .expect("装配失败");
/// // `tx` / `rx` 各自拥有半部，可分别送到两个任务。
/// # let _ = (&mut tx, &mut rx);
/// # Ok(())
/// # }
/// ```
pub struct SpscChannel {
    /// `_` 后缀：非 pub 字段。
    capacity_: usize,
}

impl SpscChannel {
    /// 以指定容量（格数）创建；校验规则与
    /// [`CircularBuffBuilder::with_capacity`] 一致。
    ///
    /// # Errors
    ///
    /// 容量为 0、低于下限或高于实现上限时返回 [`BuilderError`]。
    pub fn with_capacity(capacity: usize) -> Result<Self, BuilderError<usize>> {
        let _probe =
            CircularBuffBuilder::<_, u8, CoreAlloc>::with_capacity(capacity)?;
        Ok(SpscChannel { capacity_: capacity })
    }

    /// 容量（格数）。注意一帧 bulk 会额外占用 1 格作为预告头。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity_
    }

    /// 装配底层核心并产出两半。
    ///
    /// 底层固定使用**双端被动**拓扑。
    ///
    /// # Errors
    ///
    /// 分配失败或双端异步初始化失败时返回 [`BuilderError`]。
    pub async fn into_parts<T, A>(
        self,
    ) -> Result<(SpscSender<T, A>, SpscReceiver<T, A>), BuilderError<()>>
    where
        T: Send + Sync + 'static,
        A: Send + Sync + TrMalloc + Clone + Default + 'static,
    {
        let builder =
            CircularBuffBuilder::<BufOf<T, A>, Msg<T>, A>::with_capacity(
                self.capacity_,
            )
            .map_err(super::map_builder_err_)?;
        let ready: ConsumerSetBuilder<
            BufConsumer<Msg<T>>,
            BufOf<T, A>,
            Msg<T>,
            A,
        > = builder.consumer_passive();
        let mut ready = ready.producer_passive();
        let (tx, rx) = ready.build_async().await.map_err(super::map_builder_err_)?;
        Ok((
            SpscSender { inner_: tx },
            SpscReceiver { inner_: rx, pending_: 0 },
        ))
    }
}

// ---------------------------------------------------------------------------
// 发送端
// ---------------------------------------------------------------------------

/// 发送端半部（薄包装）：直接代理 [`Producer`]。
///
/// **不**实现 [`TrAsyncIterator`]：写端要产出的是「可写空位」，而它本质上是借出来
/// 的（`ReclSliceMut<'f, ..>`）；`TrAsyncIterator::Item` 没有生命周期参数，装不下
/// 借用，所以「可写空位」这种迭代物在当前 trait 形状下表达不出来。需要等待就用
/// [`SpscSender::send_async`] / [`SpscSender::send_bulk_async`]。
pub struct SpscSender<T = u8, A = CoreAlloc>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。
    ///
    /// **唯一**的写入口就是这个半部本身，按值拥有 ⇒ `Producer::try_write(&mut self)`
    /// 的独占借用由借用检查器保证。
    inner_: Producer<BufConsumer<Msg<T>>, BufOf<T, A>, Msg<T>, A>,
}

impl<T, A> SpscSender<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 队列容量（格数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner_.capacity()
    }

    /// 当前可写格数。
    #[inline]
    pub fn free_size(&self) -> usize {
        self.inner_.free_size()
    }

    /// 当前可读格数（含预告头与空洞，仅供观察）。
    #[inline]
    pub fn data_size(&self) -> usize {
        self.inner_.data_size()
    }

    /// 同步关闭写端：**尽力**放入 `Closing` 协议信号，然后落写端标志。
    ///
    /// 环满时放不下信号（本入口不能等空间），此时靠标志位的带外 EOF 兜底；需要
    /// 「保证信号一定入环」就用 [`SpscSender::close_async`]。
    pub fn close(&mut self) {
        let _ = try_push_closing_(&mut self.inner_);
        self.inner_.close();
    }

    /// 异步关闭写端（带取消语义）。
    ///
    /// # 取消语义
    ///
    /// * **尚未开始关闭**就被取消 ⇒ 返回 [`ChannelError::Cancelled`]，什么都不做；
    /// * 关闭一旦开始（标志已落下），取消只中断「等主动消费端排空」这一步，
    ///   **不会撤销关闭**。
    pub fn close_async<'f>(&'f mut self) -> SpscCloseAsync<'f, 'f, T, A> {
        SpscCloseAsync::new(self)
    }

    /// 内部：尝试发送一条；失败时把消息留在 `slot` 里交还（供异步重试）。
    fn try_send_item_(&mut self, slot: &mut Option<T>) -> Result<(), ChannelError> {
        let demand = Demand::exactly(1);
        let obtained = self.inner_.try_write(&demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
            return Err(ChannelError::from(err));
        }
        let Some(mut segm) = obtained.pick_left() else {
            return Err(ChannelError::Stuffed);
        };
        if !push_payload_(&mut segm, slot) {
            return Err(ChannelError::Stuffed);
        }
        Ok(())
    }

    /// **尝试**发送一条单体消息：只抢一两次写入位，抢不到就放弃。
    ///
    /// SPSC 没有锁竞争，因此这里的「放弃」只可能是**队满**。
    ///
    /// # Errors
    ///
    /// 没有空位返回 [`ChannelError::Stuffed`]；写端已关闭返回
    /// [`ChannelError::Closing`]。
    pub fn try_send(&mut self, item: T) -> Result<(), ChannelError> {
        let mut slot = Option::Some(item);
        self.try_send_item_(&mut slot)
    }

    /// 异步发送**一条单体消息**：队满时等待读端腾出空间。
    ///
    /// # 取消语义
    ///
    /// 取消令牌就绪即返回 [`ChannelError::Cancelled`]；**消息不会**被写入（随
    /// future 一起被丢弃）。
    pub fn send_async<'f>(
        &'f mut self,
        item: T,
    ) -> SpscSendAsync<'f, 'f, T, A> {
        SpscSendAsync::new(self, item)
    }

    /// 异步预留一个 bulk 帧：队满时等待空间，随后返回**同步**的 [`BulkWriter`]。
    ///
    /// # 取消语义
    ///
    /// 取消令牌就绪即返回 [`ChannelError::Cancelled`]，此时没有预留任何格。
    pub fn send_bulk_async<'f>(
        &'f mut self,
        n: usize,
    ) -> SpscSendBulkAsync<'f, 'f, T, A> {
        SpscSendBulkAsync::new(self, n)
    }

    /// **尝试**预留一个 bulk 帧（`n` 个成员 + 1 个预告头）并返回 [`BulkWriter`]。
    ///
    /// 返回的填充器是**同步**的：写入位此刻已在手，调用者可以随意 `push`。
    ///
    /// # Errors
    ///
    /// 需要 `n + 1` 格连续空间；不足返回 [`ChannelError::Stuffed`]。
    ///
    /// # 独占性由借用检查器保证
    ///
    /// 下面这段**不能编译**——这正是「写端不产出可独立使用的迭代物」所守住的
    /// 不变量：段还活着时，写端无法再借出去（否则两段会从同一个尚未推进的写位置
    /// 借出重叠的 `&mut` 区域）。
    ///
    /// ```compile_fail
    /// # use buffex::channels::SpscChannel;
    /// # async fn demo() {
    /// let (mut tx, _rx) = SpscChannel::with_capacity(16)
    ///     .expect("容量合法")
    ///     .into_parts::<u8, buffex::circular_buff::CoreAlloc>()
    ///     .await
    ///     .expect("装配成功");
    /// let mut w = tx.try_send_bulk(3).expect("预留成功");
    /// // 段还活着，写端已被独占借用 ⇒ 下面这行应当编译失败。
    /// let _ = tx.try_send(1);
    /// w.push(7);
    /// # }
    /// ```
    pub fn try_send_bulk<'f>(
        &'f mut self,
        n: usize,
    ) -> Result<BulkWriter<'f, T, A>, ChannelError> {
        let need = n.saturating_add(1);
        // 单写者 ⇒ 这个判断在预留之前是权威的。
        if self.inner_.free_size() < need {
            return Err(ChannelError::Stuffed);
        }
        let obtained = self.inner_.try_write(&BULK_DEMAND_);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
            return Err(ChannelError::from(err));
        }
        let Some(mut segm) = obtained.pick_left() else {
            return Err(ChannelError::Stuffed);
        };
        if !push_bulk_head_(&mut segm, n) {
            return Err(ChannelError::Stuffed);
        }
        Ok(BulkWriter::new_(segm, n))
    }
}

impl<T, A> Drop for SpscSender<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    fn drop(&mut self) {
        // 写者消失即关闭写端：读端不会因为「写端已经不存在」而永久挂起。
        // 与显式关闭**同一套机制**：先尽力放 `Closing` 协议信号，再落标志兜底。
        let _ = try_push_closing_(&mut self.inner_);
        self.inner_.close();
    }
}

// ---------------------------------------------------------------------------
// 接收端
// ---------------------------------------------------------------------------

/// 接收端半部（薄包装）：直接代理 [`Consumer`]。
///
/// 实现 [`TrAsyncIterator`]，产出物是**一条用户载荷** `T`：内部协议（空洞、预告头）
/// 全部在本类型内部消化，绝不暴露给调用者。
pub struct SpscReceiver<T = u8, A = CoreAlloc>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。
    inner_: Consumer<BufProducer<Msg<T>>, BufOf<T, A>, Msg<T>, A>,
    /// 当前 bulk 帧还剩多少成员格未消费（0 表示不在帧内）。
    pending_: usize,
}

impl<T, A> SpscReceiver<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 队列容量（格数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner_.capacity()
    }

    /// 当前可读格数（含预告头与空洞，仅供观察）。
    #[inline]
    pub fn data_size(&self) -> usize {
        self.inner_.data_size()
    }

    /// 写端是否已关闭。
    #[inline]
    pub fn is_writer_closed(&self) -> bool {
        self.inner_.is_producer_closed()
    }

    /// 取出一条用户载荷。
    ///
    /// `Ok(Option::None)` 表示**暂态**没有数据（可稍后重试）；`Err(Closing)` 表示
    /// EOF（写端已关闭且已读空）或读端已关闭。
    ///
    /// 内部规则：**空洞丢弃并视作已读**；`BulkHead` 只更新「本帧还剩多少成员」，
    /// 不产出任何东西。因此调用者看到的是一串干净的载荷。
    ///
    /// # Errors
    ///
    /// 见上。
    pub fn recv(&mut self) -> Result<Option<T>, ChannelError> {
        loop {
            let msg = match self.pop_one_()? {
                Option::Some(m) => m,
                Option::None => return Ok(Option::None),
            };
            match msg {
                Msg::Payload(t) => {
                    if self.pending_ > 0 {
                        self.pending_ -= 1;
                    }
                    return Ok(Option::Some(t));
                }
                // 空洞：丢弃、视作已读，继续等下一条。
                Msg::Void => {
                    if self.pending_ > 0 {
                        self.pending_ -= 1;
                    }
                }
                // 预告头：记录本帧成员数，不产出。
                Msg::BulkHead { len } => {
                    self.pending_ = len;
                }
                // 关闭信号：带内 EOF——排在它前面的载荷都已经交付过了。
                Msg::Closing => return Err(ChannelError::Closing),
            }
        }
    }

    /// 从环里取出一条协议消息（不做语义解释）。
    fn pop_one_(&mut self) -> Result<Option<Msg<T>>, ChannelError> {
        let demand = Demand::exactly(1);
        let obtained = self.inner_.try_read(&demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ConsumerError::<usize>::Closing);
            return match err {
                crate::circular_buff::ConsumerError::Drained(_) => Ok(Option::None),
                other => Err(ChannelError::from(other)),
            };
        }
        let Some(mut segm) = obtained.pick_left() else {
            return Ok(Option::None);
        };
        Ok(pop_msg_(&mut segm))
    }
}

impl<T, A> TrAsyncIterator for SpscReceiver<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
{
    /// 产出物：一条用户载荷。内部协议在本类型内部消化。
    type Item = T;
    type Err = ChannelError;

    type NextAsync<'f>
        = SpscReceiveNextAsync<'f, 'f, T, A>
    where
        Self: 'f;

    #[inline]
    fn next_async(&mut self) -> Self::NextAsync<'_> {
        SpscReceiveNextAsync::new(self)
    }
}

// ---------------------------------------------------------------------------
// 生成的可取消 future
// ---------------------------------------------------------------------------

/// 读端 `next_async`：取得一条用户载荷。
///
/// 暂无数据时 `await` 底层可读等待 future（由写端提交后唤醒）；帧内的空洞在此被
/// 静默丢弃。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(SpscReceiveNext, pub)]
async fn spsc_receive_next_async_<'f, T, A, C>(
    receiver: &'f mut SpscReceiver<T, A>,
    cancel: C,
) -> Result<T, ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    let demand = ANY_;
    loop {
        if cancel.is_cancelled() {
            return Err(ChannelError::Cancelled);
        }
        match receiver.recv() {
            Ok(Option::Some(t)) => return Ok(t),
            Ok(Option::None) => {}
            Err(e) => return Err(e),
        }
        let _ = receiver
            .inner_
            .read_async(&demand)
            .may_cancel_with(cancel.child_token())
            .await;
    }
}

/// 异步发送一条单体消息：队满时等待读端腾出空间。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(SpscSend, pub)]
async fn spsc_send_async_<'f, T, A, C>(
    sender: &'f mut SpscSender<T, A>,
    item: T,
    cancel: C,
) -> Result<(), ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    let demand = ANY_;
    let mut slot = Option::Some(item);
    loop {
        if cancel.is_cancelled() {
            return Err(ChannelError::Cancelled);
        }
        match sender.try_send_item_(&mut slot) {
            Ok(()) => return Ok(()),
            Err(ChannelError::Stuffed) => {}
            Err(e) => return Err(e),
        }
        // 队满：park 在底层可写等待 future 上，由读端消费后唤醒再重试。
        let _ = sender
            .inner_
            .write_async(&demand)
            .may_cancel_with(cancel.child_token())
            .await;
    }
}

/// 写端 `send_bulk_async`：等够 `n + 1` 格空间后预留整帧，返回同步填充器。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(SpscSendBulk, pub)]
async fn spsc_send_bulk_async_<'f, T, A, C>(
    sender: &'f mut SpscSender<T, A>,
    n: usize,
    cancel: C,
) -> Result<BulkWriter<'f, T, A>, ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    let need = n.saturating_add(1);
    let demand = Demand::exactly(need);
    loop {
        if cancel.is_cancelled() {
            return Err(ChannelError::Cancelled);
        }
        match sender.try_send_bulk(n) {
            Ok(w) => return Ok(w),
            Err(ChannelError::Stuffed) => {}
            Err(e) => return Err(e),
        }
        // 等够整帧的空间，由读端消费后唤醒。
        let _ = sender
            .inner_
            .write_async(&demand)
            .may_cancel_with(cancel.child_token())
            .await;
    }
}

/// 写端 `close_async`：先关闭写端，再等主动消费端排空残留数据。
///
/// 「关闭前必须给消费端发协议信号，保证已写入的消息真的被收到」这件事由
/// [`Producer::close_async`](crate::circular_buff::Producer::close_async) 负责，
/// 本方法只是把它包成带取消语义的入口——**不自己手搓关闭流程**。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(SpscClose, pub)]
async fn spsc_close_async_<'f, T, A, C>(
    sender: &'f mut SpscSender<T, A>,
    cancel: C,
) -> Result<(), ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    if cancel.is_cancelled() {
        // 关闭还没开始：什么都不做，如实报告取消。
        return Err(ChannelError::Cancelled);
    }
    // 1) 把**协议信号**放进环里（环满时等空间）——与 MPSC 共用同一套实现。
    let _ = push_closing_wait_(&mut sender.inner_, cancel.child_token()).await;
    // 2) 落写端标志并交给 `Producer` 自己的异步关闭（排空 / 发信号给主动消费端）。
    sender
        .inner_
        .close_async()
        .may_cancel_with(cancel.child_token())
        .await;
    Ok(())
}

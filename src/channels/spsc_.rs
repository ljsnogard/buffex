//! 有界 SPSC 队列：对 [`circular_buff`](crate::circular_buff) 被动 × 被动半部的
//! **薄包装**。
//!
//! # 为什么不重新实现一套 SPSC
//!
//! [`crate::circular_buff`] 的 [`builder`](crate::circular_buff::builder) 在
//! 「双端被动」时已经产出一对拥有型半部
//! （[`Producer`](crate::circular_buff::Producer) /
//! [`Consumer`](crate::circular_buff::Consumer)）：唤醒协议、`Demand` 语义、跨末端
//! 两段式段、关闭 / EOF 事件均已实现且有测试覆盖。`channels::spsc_` 只做三件事：
//!
//! 1. **命名**：把 `(Producer, Consumer)` 呈现为 [`SpscSender`] / [`SpscReceiver`]；
//! 2. **抹平构建细节**：把 `CircularBuffBuilder` → `consumer_passive()` →
//!    `producer_passive()` → `build_async()` 收敛成
//!    [`SpscChannel::with_capacity`] + [`SpscChannel::into_parts`]；
//! 3. **补齐异步迭代形状**：两端都实现
//!    [`TrAsyncIterator`](abs_async_iter::TrAsyncIterator)。
//!
//! **本模块没有自己的同步原语、没有自己的缓冲、没有自己的消息约束。** 消息就在
//! 底层环形缓冲里**原地构造**：`T` 既不需要 `Copy`，也不需要 `Clone`。
//!
//! # 两端各提供两类入口（仿 `circ_buff` 的 `try_*` / `*_async` 分法）
//!
//! | 入口 | 一次处理 | 消息本体 | 适用 |
//! | --- | --- | --- | --- |
//! | [`SpscSender::send`] / [`SpscReceiver::recv`] | 1 个 | 直接 move | 消息现成 |
//! | [`SpscSender::try_next`] / [`SpscReceiver::try_next`] | 多个 | 借出段、就地构造 / 读取 | 批量、不可 `Clone` |
//!
//! 段的 `Drop` 负责提交：写段提交「已构造的单元数」，读段提交「已消费量」。
//! 一次没构造任何单元就 drop 写段，等价于「什么也没发生」。
//!
//! 也可以让 `T = Option<U>`：没来得及写入的槽位保持 `None`，由消费端按业务语义
//! 跳过——这比强制 `Default` 或额外的空洞协议更省事（见 `dev-notes/`）。
//!
//! # 单写者 / 单读者是前提
//!
//! 底层核心按 SPSC 约定实现（`Send + Sync` 的 `unsafe impl` 论证见
//! `circular_buff::core_` 的安全说明）。把 [`SpscSender`] 分发给多个任务会绕过该
//! 前提并产生数据竞争；需要多写者请用 [`MpscChannel`](super::MpscChannel)。

use core::{borrow::BorrowMut, mem::MaybeUninit};

use abs_async_iter::TrAsyncIterator;
use abs_buff::{Demand, gen_may_cancel_future, x_deps::abs_cancel};
use abs_cancel::{TrCancellationToken, TrMayCancel};
use mm_ptr::{
    Owned,
    x_deps::abs_mm::mem_alloc::{CoreAlloc, TrMalloc},
};

use super::ChannelError;
use crate::circular_buff::{
    BufConsumer, BufProducer, Consumer, Producer,
    abs_comp_::TrProducer,
    builder::{BuilderError, CircularBuffBuilder, ConsumerSetBuilder},
    core_::CircCore,
    reclaim_::{ChildReclaim, ReaderReclaim, WriterReclaim},
};

/// 迭代路径统一使用的需求区间：至少 1 个单元、不设上限（有多少给多少）。
const ANY_: Demand<usize> = Demand::at_least(1);

// ---------------------------------------------------------------------------
// 具体类型别名
// ---------------------------------------------------------------------------

/// 队列核心：被动生产端 × 被动消费端。
pub type SpscCore<B, T> = CircCore<BufProducer<T>, BufConsumer<T>, B, T>;

/// 写提交器。
pub type SpscWriterReclaim<'a, B, T> = WriterReclaim<'a, SpscCore<B, T>>;

/// 读提交器。
pub type SpscReaderReclaim<'a, P, B, T> =
    ReaderReclaim<'a, CircCore<P, BufConsumer<T>, B, T>>;

/// 写段：一次发送可用的可写空间（drop 时提交已构造的单元数）。
pub type SpscWriteSegm<'a, B, T> =
    crate::circular_buff::ReclSliceMut<'a, T, SpscWriterReclaim<'a, B, T>>;

/// 读段：一次接收可用的可读数据（drop 时提交已消费量）。
pub type SpscReadSegm<'a, P, B, T> =
    crate::circular_buff::ReclSliceRef<'a, T, SpscReaderReclaim<'a, P, B, T>>;

/// 子写段：调用者**原地构造**消息的窄视图（`SegmMut` 的薄封装）。
pub type SpscChildMut<'a, T> = abs_buff::buffer::SegmMut<'a, T, ChildReclaim<'a>>;

/// 子读段：调用者就地读取 / 借用消息的窄视图。
pub type SpscChildRef<'a, T> = abs_buff::buffer::SegmRef<'a, T, ChildReclaim<'a>>;

/// 队列的默认数据承载：堆上的一块 `MaybeUninit<T>` 环形缓冲，由
/// [`circular_buff`](crate::circular_buff) 的核心拥有。
pub type DefaultBuf<T, A = CoreAlloc> = Owned<[MaybeUninit<T>], A>;

/// 写端半部的默认形态。
pub type SpscSenderDefault<T, A = CoreAlloc> = SpscSender<DefaultBuf<T, A>, T, A>;

/// 读端半部的默认形态。
pub type SpscReceiverDefault<T, A = CoreAlloc> =
    SpscReceiver<BufProducer<T>, DefaultBuf<T, A>, T, A>;

// ---------------------------------------------------------------------------
// 两端
// ---------------------------------------------------------------------------

/// 发送端半部（薄包装）：直接代理 [`Producer`]。
///
/// 它实现 [`TrAsyncIterator`]，产出物是
/// [`SpscWriteSegm`]——**没有** `T: Copy` 或 `T: Clone` 约束：调用者在借出的段里
/// 原地构造消息。一次只放一个现成消息请用 [`SpscSender::send`]。
pub struct SpscSender<B, T = u8, A = CoreAlloc>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。
    inner_: Producer<BufConsumer<T>, B, T, A>,
}

/// 接收端半部（薄包装）：直接代理 [`Consumer`]。
///
/// 它实现 [`TrAsyncIterator`]，产出物是
/// [`SpscReadSegm`]——可以就地借用消息，也可以把消息 move 出来（`T` 不需要
/// `Copy` / `Clone`）。一次只取一个请用 [`SpscReceiver::recv`]。
pub struct SpscReceiver<P, B, T = u8, A = CoreAlloc>
where
    P: Send + Sync + TrProducer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。
    inner_: Consumer<P, B, T, A>,
}

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
/// # let _ = (&mut tx, &mut rx);
/// # Ok(())
/// # }
/// ```
pub struct SpscChannel {
    /// `_` 后缀：非 pub 字段。
    capacity_: usize,
}

impl SpscChannel {
    /// 以指定容量（单元数）创建；校验规则与
    /// [`CircularBuffBuilder::with_capacity`] 一致。
    ///
    /// # Errors
    ///
    /// 容量为 0、低于下限或高于实现上限时返回 [`BuilderError`]。
    pub fn with_capacity(capacity: usize) -> Result<Self, BuilderError<usize>> {
        // 立刻走一遍构建器的容量校验，把「容量非法」提前到此处暴露。
        let _probe =
            CircularBuffBuilder::<_, u8, CoreAlloc>::with_capacity(capacity)?;
        Ok(SpscChannel { capacity_: capacity })
    }

    /// 容量（单元数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity_
    }

    /// 装配底层核心并产出两半。
    ///
    /// 底层固定使用**双端被动**拓扑，因此 `build_async` 产出的是
    /// [`SpscPair`](crate::circular_buff::SpscPair)。
    ///
    /// # Errors
    ///
    /// 分配失败或双端异步初始化失败时返回 [`BuilderError`]。
    pub async fn into_parts<T, A>(
        self,
    ) -> Result<
        (SpscSenderDefault<T, A>, SpscReceiverDefault<T, A>),
        BuilderError<()>,
    >
    where
        T: Send + Sync + 'static,
        A: Send + Sync + TrMalloc + Clone + Default + 'static,
    {
        let builder = CircularBuffBuilder::<DefaultBuf<T, A>, T, A>::with_capacity(
            self.capacity_,
        )
        .map_err(map_builder_err_)?;
        let ready: ConsumerSetBuilder<BufConsumer<T>, DefaultBuf<T, A>, T, A> =
            builder.consumer_passive();
        let mut ready = ready.producer_passive();
        let (tx, rx) = ready.build_async().await.map_err(map_builder_err_)?;
        Ok((SpscSender { inner_: tx }, SpscReceiver { inner_: rx }))
    }
}

impl<B, T, A> SpscSender<B, T, A>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 借用底层 [`Producer`] 半部。
    #[inline]
    pub fn inner_mut(&mut self) -> &mut Producer<BufConsumer<T>, B, T, A> {
        &mut self.inner_
    }

    /// 队列容量（单元数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner_.capacity()
    }

    /// 当前可写空间。
    #[inline]
    pub fn free_size(&self) -> usize {
        self.inner_.free_size()
    }

    /// 当前可读数据量（观察用）。
    #[inline]
    pub fn data_size(&self) -> usize {
        self.inner_.data_size()
    }

    /// 同步关闭写端：读端随后会读到 `Ok(None)`（EOF）。
    #[inline]
    pub fn close(&mut self) {
        self.inner_.close()
    }

    /// 非阻塞取得一个**写段**：批量 / 就地构造消息的入口。
    ///
    /// 调用者用 [`iter_slices_mut`](SpscWriteSegm::iter_slices_mut) 拿到
    /// `&mut [MaybeUninit<T>]` 原地构造若干个消息（可含不可 `Clone` 的类型），
    /// 或直接用 [`write`](SpscWriteSegm::write) 放一个现成的值。段 drop 时把
    /// 「已构造的单元数」提交回核心并唤醒读端。
    ///
    /// `Ok(Option::None)` 表示**暂态**没有空间（背压，可稍后重试）——与读端
    /// `try_next` 的 `Ok(None)`（暂态无数据）对称。
    ///
    /// # Errors
    ///
    /// 写端已关闭返回 [`ChannelError::Closing`]。
    pub fn try_next<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Result<Option<SpscWriteSegm<'f, B, T>>, ChannelError> {
        let obtained = self.inner_.try_write(demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
            return match ChannelError::from(err) {
                ChannelError::Stuffed => Ok(Option::None),
                other => Err(other),
            };
        }
        Ok(obtained.pick_left())
    }

    /// 就地发送**一个**现成的值（消息本体已就绪时的快捷入口；不暴露段）。
    ///
    /// # Errors
    ///
    /// 空间不足返回 [`ChannelError::Stuffed`]；写端已关闭返回
    /// [`ChannelError::Closing`]。
    pub fn send(&mut self, item: T) -> Result<(), ChannelError> {
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
        if !fill_one_(&mut segm, item) {
            return Err(ChannelError::Stuffed);
        }
        Ok(())
    }
}

impl<P, B, T, A> SpscReceiver<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 借用底层 [`Consumer`] 半部。
    #[inline]
    pub fn inner_mut(&mut self) -> &mut Consumer<P, B, T, A> {
        &mut self.inner_
    }

    /// 队列容量（单元数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner_.capacity()
    }

    /// 当前可读数据量。
    #[inline]
    pub fn data_size(&self) -> usize {
        self.inner_.data_size()
    }

    /// 写端是否已关闭（EOF 的前置条件）。
    #[inline]
    pub fn is_writer_closed(&self) -> bool {
        self.inner_.is_producer_closed()
    }

    /// 关闭读端。
    ///
    /// 底层 `Consumer::close_async` 的实现只做一次同步的关闭标志设置
    /// （见 `circular_buff::spsc_` 的 `consumer_close_async_`），因此这里保留一个
    /// 同步入口，方便非 async 的收尾路径。
    #[inline]
    pub fn close(&mut self) {
        let mut fut = self.inner_.close_async();
        // 关闭标志在第一次 poll 之前就已置位（见其实现）。
        let _ = (&mut fut,);
    }

    /// 非阻塞取得一个**读段**：批量 / 就地读取消息的入口。
    ///
    /// 调用者用 [`iter_slices`](SpscReadSegm::iter_slices) 就地读取或**借用**消息
    /// （`T` 不需要 `Copy`）；段 drop 时把已消费量提交回核心。
    ///
    /// `Ok(None)` 表示**暂态**无数据（可稍后重试）；`Err(Closing)` 表示 EOF
    /// （写端已关闭且已读空）或读端已关闭。
    ///
    /// # Errors
    ///
    /// 见上。
    pub fn try_next<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Result<Option<SpscReadSegm<'f, P, B, T>>, ChannelError> {
        let obtained = self.inner_.try_read(demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ConsumerError::<usize>::Closing);
            return match err {
                crate::circular_buff::ConsumerError::Drained(_) => Ok(None),
                other => Err(ChannelError::from(other)),
            };
        }
        Ok(obtained.pick_left())
    }

    /// 取出**一个**值（消息本体可直接取走时的快捷入口；不暴露段）。
    ///
    /// `T` 不需要 `Copy`：值是从段里 move 出来的。
    ///
    /// # Errors
    ///
    /// 读端已关闭，或写端已关闭且缓冲已读空（EOF）时返回
    /// [`ChannelError::Closing`]。
    pub fn recv(&mut self) -> Result<Option<T>, ChannelError> {
        let demand = Demand::exactly(1);
        let obtained = self.inner_.try_read(&demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ConsumerError::<usize>::Closing);
            return match err {
                crate::circular_buff::ConsumerError::Drained(_) => Ok(None),
                other => Err(ChannelError::from(other)),
            };
        }
        let Some(mut segm) = obtained.pick_left() else {
            return Ok(None);
        };
        Ok(take_one_(&mut segm))
    }
}

// ---------------------------------------------------------------------------
// 两端都实现 abs_async_iter
// ---------------------------------------------------------------------------

impl<B, T, A> TrAsyncIterator for SpscSender<B, T, A>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]> + 'static,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
{
    /// 产出物：一段可写空间（`'static` 由核心的 `Shared` 保活承担——见
    /// [`SpscWriteSegm`]）。
    type Item = SpscWriteSegm<'static, B, T>;
    type Err = ChannelError;

    type NextAsync<'f>
        = SpscSendNextAsync<'f, 'f, B, T, A>
    where
        Self: 'f;

    #[inline]
    fn next_async(&mut self) -> Self::NextAsync<'_> {
        SpscSendNextAsync::new(self)
    }
}

impl<P, B, T, A> TrAsyncIterator for SpscReceiver<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T> + 'static,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]> + 'static,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
{
    /// 产出物：一段可读数据（就地借用；`T` 无需 `Copy` / `Clone`）。
    type Item = SpscReadSegm<'static, P, B, T>;
    type Err = ChannelError;

    type NextAsync<'f>
        = SpscReceiveNextAsync<'f, 'f, P, B, T, A>
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

/// 写端 `next_async`：取得一段可写空间。
///
/// 先做一次非阻塞尝试；**队满（背压）时真的 `await`** 底层
/// [`Producer::write_async`](crate::circular_buff::Producer::write_async) 的可写
/// 等待 future——由对端读取提交后唤醒，而不是空转或假装成功。生产端不需要等待
/// 任何设备，它等的只是缓冲里的空位。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(SpscSendNext, pub)]
async fn spsc_send_next_async_<'f, B, T, A, C>(
    sender: &'f mut SpscSender<B, T, A>,
    cancel: C,
) -> Result<Option<SpscWriteSegm<'static, B, T>>, ChannelError>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]> + 'static,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    let demand = ANY_;
    match sender.try_next(&demand) {
        Ok(Option::Some(segm)) => {
            // SAFETY: 保活责任由 `Producer` 半部内部持有的核心 `Shared` 承担。
            return Ok(Option::Some(unsafe { extend_write_segm_(segm) }));
        }
        Ok(Option::None) => {}
        Err(e) => return Err(e),
    }
    // 背压：park 在底层的可写等待 future 上，由读端提交唤醒。
    let obtained = sender
        .inner_
        .write_async(&demand)
        .may_cancel_with(cancel)
        .await;
    match obtained.pick_left() {
        // SAFETY: 同上的保活论证。
        Some(segm) => Ok(Option::Some(unsafe { extend_write_segm_(segm) })),
        None => Ok(Option::None),
    }
}

/// 读端 `next_async`：取得一段可读数据。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(SpscReceiveNext, pub)]
async fn spsc_receive_next_async_<'f, P, B, T, A, C>(
    receiver: &'f mut SpscReceiver<P, B, T, A>,
    cancel: C,
) -> Result<Option<SpscReadSegm<'static, P, B, T>>, ChannelError>
where
    P: Send + Sync + TrProducer<Data = T> + 'static,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]> + 'static,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    let demand = ANY_;
    if cancel.is_cancelled() {
        return Ok(Option::None);
    }
    let obtained = receiver.inner_.try_read(&demand);
    if obtained.as_ref().pick_left().is_none() {
        let err = obtained
            .pick_right()
            .unwrap_or(crate::circular_buff::ConsumerError::<usize>::Closing);
        return match err {
            // 暂无数据（暂态）：`Ok(None)`，由调用者稍后重新 `next_async`。
            crate::circular_buff::ConsumerError::Drained(_) => Ok(Option::None),
            other => Err(ChannelError::from(other)),
        };
    }
    let Some(segm) = obtained.pick_left() else {
        return Ok(Option::None);
    };
    // SAFETY: 同 `spsc_send_next_async_`。
    Ok(Option::Some(unsafe { extend_read_segm_(segm) }))
}

// ---------------------------------------------------------------------------
// 产出物的生命周期延长
// ---------------------------------------------------------------------------

/// 把写段的生命周期延长为 `'static`。
///
/// # Safety
///
/// 调用者必须保证：返回的段存活期间，它借用的核心**始终存活**。两个队列的发送端
/// 都持有核心的 `Shared` 句柄（`Producer` 半部内部即该句柄；MPSC 另有
/// `MpscShared::core_ref_`），因此满足该条件。
///
/// 之所以需要它：段的类型必须带上借用生命周期，而
/// [`TrAsyncIterator::Item`](abs_async_iter::TrAsyncIterator::Item) 是**无生命周期
/// 参数**的关联类型（`next_async` 的产出物不能借用 `self`）。
pub(super) unsafe fn extend_write_segm_<'f, B, T>(
    segm: SpscWriteSegm<'f, B, T>,
) -> SpscWriteSegm<'static, B, T>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
{
    // SAFETY: 见函数文档——仅延长生命周期，不改变任何位模式。
    unsafe {
        core::mem::transmute::<
            SpscWriteSegm<'f, B, T>,
            SpscWriteSegm<'static, B, T>,
        >(segm)
    }
}

/// 把读段的生命周期延长为 `'static`；约束与 [`extend_write_segm_`] 相同。
pub(super) unsafe fn extend_read_segm_<'f, P, B, T>(
    segm: SpscReadSegm<'f, P, B, T>,
) -> SpscReadSegm<'static, P, B, T>
where
    P: Send + Sync + TrProducer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
{
    // SAFETY: 见 [`extend_write_segm_`]。
    unsafe {
        core::mem::transmute::<
            SpscReadSegm<'f, P, B, T>,
            SpscReadSegm<'static, P, B, T>,
        >(segm)
    }
}

// ---------------------------------------------------------------------------
// 单格构造 / 取出的小工具（不要求 T: Copy / Clone）
// ---------------------------------------------------------------------------

/// 在写段的第 1 格**原地构造** `item`；成功返回 `true`。
///
/// 必须经 `move_items_from_buff`（与 `circ_buff` 自己的测试用 `fill_segm` 同一条
/// 路径）：它一边搬进元素、一边推进段的已消费偏移，段 drop 时才提交得出正确的
/// 单元数。直接对 `iter_slices_mut` 的槽位落笔不会推进任何偏移，父段会以提交量 0
/// 收场，表现为「写了但读不到」。
///
/// `item` 是 move 进暂存的，因此不需要 `Copy` / `Clone`。
pub(super) fn fill_one_<T, R>(
    segm: &mut crate::circular_buff::ReclSliceMut<'_, T, R>,
    item: T,
) -> bool
where
    R: abs_buff::buffer::TrReclaim,
{
    if segm.least_count() == 0 {
        return false;
    }
    // 用长度为 1 的栈上暂存：不引入分配；`T: Copy` 也**不**需要——
    // `move_items_from_buff` 是位拷贝，搬空后暂存无需 drop。
    let mut staging = [MaybeUninit::new(item)];
    // SAFETY: 位拷贝搬入；`staging` 在搬空后无剩余元素需要 drop。
    // `staging` 的元素已被位拷贝搬走；`MaybeUninit` 本身不 drop 其内容，因此
    // 数组在作用域结束时析构是安全的（不会二次 drop）。
    segm.move_items_from_buff(&mut staging) == 1
}

/// 从读段 move 出第 1 个值；无数据返回 `None`。
pub(super) fn take_one_<T, R>(
    segm: &mut crate::circular_buff::ReclSliceRef<'_, T, R>,
) -> Option<T>
where
    R: abs_buff::buffer::TrReclaim,
{
    if segm.least_count() == 0 {
        return None;
    }
    let mut dst = [MaybeUninit::<T>::uninit()];
    // SAFETY: 位拷贝搬出；`T` 的所有权随 `assume_init_read` 转移给调用者。
    let moved = unsafe { segm.move_items_to_buff(&mut dst) };
    if moved != 1 {
        return None;
    }
    // SAFETY: `moved == 1` 表示第 0 格已被移出并初始化。
    Some(unsafe { dst[0].assume_init_read() })
}

/// 把构建器的容量错误（`BuilderError<usize>`）转成装配期错误
/// （`BuilderError<()>`），只保留分类、丢弃被拒绝的容量数值。
///
/// `BuilderError` 是**外部 crate 的公开类型**且未实现 [`core::fmt::Display`]，
/// 也不允许在本 crate 内加 impl（孤儿规则）；因此这里保留原语义、把泛型参数抹成
/// `()`：容量数值在本层没有额外用处（调用者在
/// [`SpscChannel::with_capacity`] 时已经知道它）。
pub(super) fn map_builder_err_<T>(e: BuilderError<T>) -> BuilderError<()> {
    match e {
        BuilderError::SizeTooSmall(_) => BuilderError::SizeTooSmall(()),
        BuilderError::SizeTooBig(_) => BuilderError::SizeTooBig(()),
        BuilderError::Cancelled => BuilderError::Cancelled,
        BuilderError::ProducerInit => BuilderError::ProducerInit,
        BuilderError::ConsumerInit => BuilderError::ConsumerInit,
    }
}

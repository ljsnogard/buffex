//! 提供给最终用户、暴露的公共接口 Producer 和 Consumer。
//! 所有对 Circular Buffer 的操作都必须通过这两个实例。
//! 如果有一端在 Circular Buffer 构建时就已经被指定（主动模式），那么这一端
//! **不产出半部**——`build_async` 只把被动端的半部交给调用者（见
//! [`super::builder::BuildOutcome`]）；全主动时产出 [`Pipeline`]（流水线
//! future，由设备驱动）。
//!
//! # 设计意图（拥有型访问模型）
//!
//! 核心被分配在堆上（[`CoreRef`] = `Shared<CircCore<P, C, B, T>, A>`），
//! [`Producer`] / [`Consumer`] 各持一份引用，可独立分发、跨线程使用
//! （SPSC：至多一个生产线程 + 一个消费线程）。构建器产出 [`SpscPair`]，
//! 没有「缓冲聚合体」这一层——使用者只需持有这对半部（或其一）。
//!
//! 异步等待（`write_async` / `read_async`）把 waker 注册进核心的被动唤醒
//! 槽位：对端完成读取 / 写入时，hook 唤醒等待者，等待者重新检查条件。

use core::{
    borrow::BorrowMut,
    mem::MaybeUninit,
};

use abs_buff::{
    Demand, TrBuffRead, TrBuffTryRead, TrBuffTryWrite, TrBuffWrite,
    buffer::TrBufferState,
    gen_may_cancel_future,
    io::{TrInput, TrOutput},
    x_deps::{abs_cancel, anylr},
};
use abs_cancel::{TrCancellationToken, TrMayCancel};
use abs_mm::mem_alloc::{CoreAlloc, TrMalloc};
use anylr::SomeOf;
use mm_ptr::{
    Shared,
    x_deps::abs_mm,
};

use crate::{
    circular_buff::{
        BufConsumer, BufProducer, DevConsumer, DevProducer,
        error_::PipelineError,
    },
};
use super::{
    abs_comp_::{TrConsumer, TrProducer},
    core_::CircCore,
    error_::{ConsumerError, ProducerError},
    reclaim_::{ReaderReclaim, ReclSliceMut, ReclSliceRef, WriterReclaim},
};

/// 堆上核心的共享引用。
///
/// 名义 `pub`（模块 `spsc_` 私有，对外不可达）：构建产物装配
/// （[`super::builder::BuildOutcome`]）的公开 trait 方法签名需要引用它。
pub type CoreRef<P, C, B, T = u8, A = CoreAlloc> =
    Shared<CircCore<P, C, B, T>, A>;

/// 构建器产出的半部对：`(Producer, Consumer)`。使用者可持有两者或其一。
pub type SpscPair<B, T = u8, A = CoreAlloc> = (
    Producer<
        BufConsumer<T>,
        B, T, A,
    >,
    Consumer<
        BufProducer<T>,
        B, T, A,
    >,
);

// ---------------------------------------------------------------------------
// 生产端半部（拥有型）
// ---------------------------------------------------------------------------

/// 生产端半部：持有一份堆上核心的引用，代理转发用户请求到 `CircCore`。
///
/// `P` / `C` / `T` 与核心的端类型一致（被动 / 主动由构建期决定），`A` 是
/// 分配器（默认 `CoreAlloc`）。
///
/// **主动端不产出半部**：若生产端为主动模式（`P = DeviceProducer`），构建器
/// 不会把它交给调用者（见 [`super::builder::BuildOutcome`]）——本半部只代表
/// 被动生产端。
pub struct Producer<C, B, T, A>
where
    // P: TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: TrMalloc + Clone,
{
    core_ref_: CoreRef<BufProducer<T>, C, B, T, A>,
}

/// 消费端半部：与 [`Producer`] 对称（读路径）。
///
/// **主动端不产出半部**：若消费端为主动模式（`C = DeviceConsumer`），构建器
/// 不会把它交给调用者——本半部只代表被动消费端。
pub struct Consumer<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    // C: TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: TrMalloc + Clone,
{
    core_ref_: CoreRef<P, BufConsumer<T>, B, T, A>,
}

/// 半部的公共借用约束：段类型（[`ReclSliceMut`] / [`ReclSliceRef`]）要求
/// 核心实现 `TrCircBuffCore`（其超类
/// `Send + Sync`），故两端与元素类型必须 `Send + Sync`。此约束由各 impl 的
/// where 子句直接表达。
impl<C, B, T, A> Producer<C, B, T, A>
where
    // P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    pub(super) fn new(core_ref: CoreRef<BufProducer<T>, C, B, T, A>) -> Self {
        Producer { core_ref_: core_ref }
    }

    /// 环形缓冲的容量（单元数）。
    pub fn capacity(&self) -> usize {
        self.core_ref_.capacity()
    }

    /// 当前可写空间。
    pub fn free_size(&self) -> usize {
        self.core_ref_.free_size()
    }

    /// 当前可读数据量（观察用）。
    pub fn data_size(&self) -> usize {
        self.core_ref_.data_size()
    }

    /// 写端（本端）是否已关闭。
    pub fn is_producer_closed(&self) -> bool {
        self.core_ref_.is_tx_closed()
    }

    /// 消费端（对端）是否已关闭。
    pub fn is_consumer_closed(&self) -> bool {
        self.core_ref_.is_rx_closed()
    }

    /// 关闭写端：不再写入，触发消费端事件（`ProducerClose`）。
    pub fn close(&mut self) {
        self.core_ref_.close_tx();
    }

    /// 异步关闭写端：先关闭写端，再异步排空主动消费端的残留数据。
    ///
    /// 返回由 `gen_may_cancel_future` 生成的异步 future，可通过
    /// `may_cancel_with(&mut token)` 中断排空过程；一旦取消，关闭标志已设置。
    pub fn close_async<'f>(&'f mut self) -> ProducerCloseAsync<'f, C, B, T, A> {
        ProducerCloseAsync(self)
    }
}

impl<C, B, T, A> Producer<C, B, T, A>
where
    // P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    pub fn write_async<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> ProducerWriteAsync<'f, C, B, T, A> {
        ProducerWriteAsync(self, demand)
    }

    #[allow(clippy::type_complexity)]
    pub fn try_write<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> SomeOf< ReclSliceMut<'f, T,
            WriterReclaim<'f, CircCore<BufProducer<T>, C, B, T> >>,
        ProducerError<usize>,
    > {
        self.core_ref_.try_write_(demand)
    }
}

impl<C, B, T, A> TrBufferState for Producer<C, B, T, A>
where
    // P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    #[inline]
    fn capacity(&self) -> usize {
        Producer::capacity(self)
    }

    #[inline]
    fn data_size(&self) -> usize {
        Producer::data_size(self)
    }

    #[inline]
    fn free_size(&self) -> usize {
        Producer::free_size(self)
    }

    #[inline]
    fn is_consumer_closed(&self) -> bool {
        Producer::is_consumer_closed(self)
    }

    #[inline]
    fn is_producer_closed(&self) -> bool {
        Producer::is_producer_closed(self)
    }
}

impl<C, B, T, A> Drop for Producer<C, B, T, A>
where
    // P: TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: TrMalloc + Clone,
{
    fn drop(&mut self) {
        self.core_ref_.on_buf_producer_drop_();
    }
}

#[gen_may_cancel_future(ProducerWrite)]
async fn producer_write_async_<'f, K, B, T, A, C>(
    producer: &'f mut Producer<K, B, T, A>,
    demand: &'f Demand<usize>,
    cancel: &'f mut C,
) -> SomeOf<
    ReclSliceMut<'f, T,
        WriterReclaim<'f, CircCore<BufProducer<T>, K, B, T>> >,
    ProducerError<usize>,
> where
    // P: Send + Sync + TrProducer<Data = T>,
    K: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
    C: TrCancellationToken + Clone,
{
    producer.core_ref_
        .write_async_(demand)
        .may_cancel_with(cancel)
        .await
}

#[gen_may_cancel_future(ProducerClose)]
async fn producer_close_async_<'f, K, B, T, A, C>(
    producer: &'f mut Producer<K, B, T, A>,
    cancel: &'f mut C,
) -> ()
where
    K: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
    C: TrCancellationToken + Clone,
{
    producer.core_ref_.close_tx_async(cancel).await;
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// Consumer impl
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<P, B, T, A> Consumer<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    // C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    pub(super) fn new(core_ref_: CoreRef<P, BufConsumer<T>, B, T, A>) -> Self {
        Consumer { core_ref_ }
    }

    /// 环形缓冲的容量（单元数）。
    pub fn capacity(&self) -> usize {
        self.core_ref_.capacity()
    }

    /// 当前可读数据量。
    pub fn data_size(&self) -> usize {
        self.core_ref_.data_size()
    }

    /// 当前可写空间（观察用）。
    pub fn free_size(&self) -> usize {
        self.core_ref_.free_size()
    }

    /// 读端（本端）是否已关闭。
    pub fn is_consumer_closed(&self) -> bool {
        self.core_ref_.is_rx_closed()
    }

    /// 生产端（对端）是否已关闭（EOF）。
    pub fn is_producer_closed(&self) -> bool {
        self.core_ref_.is_tx_closed()
    }

    /// 关闭读端：不再读取，触发生产端事件（`ConsumerClose`）。
    pub fn close_async(&mut self) -> ConsumerCloseAsync<'_, P, B, T, A> {
        ConsumerCloseAsync(self)
    }
}

impl<P, B, T, A> Consumer<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    // C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    pub fn read_async<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> ConsumerReadAsync<'f, P, B, T, A> {
        ConsumerReadAsync(self, demand)
    }

    #[allow(clippy::type_complexity)]
    pub fn try_read<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> SomeOf<
        ReclSliceRef<'f, T, ReaderReclaim<'f, CircCore<P, BufConsumer<T>, B, T>>>,
        ConsumerError<usize>,
    > {
        self.core_ref_.try_read_(demand)
    }
}

impl<P, B, T, A> TrBufferState for Consumer<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    // C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    #[inline]
    fn capacity(&self) -> usize {
        Consumer::capacity(self)
    }

    #[inline]
    fn data_size(&self) -> usize {
        Consumer::data_size(self)
    }

    #[inline]
    fn free_size(&self) -> usize {
        Consumer::free_size(self)
    }

    #[inline]
    fn is_consumer_closed(&self) -> bool {
        Consumer::is_consumer_closed(self)
    }

    #[inline]
    fn is_producer_closed(&self) -> bool {
        Consumer::is_producer_closed(self)
    }
}

impl<P, B, T, A> Drop for Consumer<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    // C: TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: TrMalloc + Clone,
{
    fn drop(&mut self) {
        self.core_ref_.on_buf_consumer_drop_()
    }
}

#[gen_may_cancel_future(ConsumerRead)]
async fn consumer_read_async_<'f, P, B, T, A, C>(
    consumer: &'f mut Consumer<P, B, T, A>,
    demand: &'f Demand<usize>,
    cancel: &'f mut C,
) -> SomeOf<ReclSliceRef<'f, T,
    ReaderReclaim<'f, CircCore<P, BufConsumer<T>, B, T>> >,
    ConsumerError<usize>>
where
    P: Send + Sync + TrProducer<Data = T>,
    // K: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
    C: TrCancellationToken + Clone,
{
    consumer.core_ref_
        .read_async_(demand)
        .may_cancel_with(cancel)
        .await
}

#[gen_may_cancel_future(ConsumerClose)]
async fn consumer_close_async_<'f, P, B, T, A, C>(
    consumer: &'f mut Consumer<P, B, T, A>,
    _cancel: &'f mut C,
) -> ()
where
    P: Send + Sync + TrProducer<Data = T>,
    // K: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
    C: TrCancellationToken + Clone,
{
    consumer.core_ref_.close_rx();
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// Pair judgement
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<B, T, A> Producer<BufConsumer<T>, B, T, A>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    pub fn is_paired_with(
        &self,
        consumer: &Consumer<BufProducer<T>, B, T, A>,
    ) -> bool {
        let this = self.core_ref_.as_ptr();
        let that = consumer.core_ref_.as_ptr();
        core::ptr::eq(this, that)
    }
}

impl<B, T, A> Consumer<BufProducer<T>, B, T, A>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    pub fn is_paired_with(
        &self,
        producer: &Producer<BufConsumer<T>, B, T, A>,
    ) -> bool
    where
        B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
        T: Send + Sync + 'static,
        A: Send + Sync + TrMalloc + Clone,
    {
        let this = self.core_ref_.as_ptr();
        let that = producer.core_ref_.as_ptr();
        core::ptr::eq(this, that)
    }
}

// ---------------------------------------------------------------------------
// 全主动流水线（双端主动时 `build_async` 的产物）：一个由设备驱动的 Future
// ---------------------------------------------------------------------------

/// 双端全主动（`TrInput → 缓冲 → TrOutput`）时 `build_async` 的产物：一条
/// **流水线 Future**。
///
/// 由调用者交给异步运行时（`spawn`）驱动：**只要本 future 存活（未被取消 /
/// 未结束），数据就持续从输入设备流向输出设备**——泵循环 await 两端设备的
/// `read_async` / `write_async`，由设备自身的就绪/阻塞驱动流动；直到：
///
/// * 一端**出错**（设备 future 返回错误——表现为该方向不再有进展）；
/// * 一端**关闭**（核心的 tx / rx 端被关闭）；
/// * 调用者**请求断开**（[`Pipeline::disconnect_handle`] 的
///   [`PipelineDisconnect::request`]，或直接 drop / 取消本 future）。
///
/// 断开时流水线关闭两端、把缓冲残留排空到输出设备后结束（future 返回
/// `()`）。
///
/// # 设备契约
///
/// 为让「数据一到达就流动」，输入/输出设备的异步操作应在暂时无数据 / 无空间
/// 时返回 `Pending`（并注册 waker），由设备的运行时在就绪时唤醒——本 future
/// 的轮询完全由设备的就绪驱动。若设备在无数据时立即返回 `Ready(0)`（非阻塞
/// 风格），流水线在本轮无进展后停驻（不再流动，也不会空转）。
///
/// # 与被动端混合
///
/// 若需要流水线**自动**持续流动且不想持有本 future，请让至少一端保持被动：
/// 被动端的每次读写都会自动驱动对端的主动泵（见 [`Consumer`] / [`Producer`]）。
pub struct Pipeline<I, O, B, T, A>
where
    I: Send + Sync + TrInput<T>,
    O: Send + Sync + TrOutput<T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: 'static,
    A: TrMalloc + Clone,
{
    core_ref_: PipeCore<I, O, B, T, A>,
}

type PipeCore<I, O, B, T, A> =
    CoreRef<DevProducer<I, T>, DevConsumer<O, T>, B, T, A>;

impl<I, O, B, T, A> Pipeline<I, O, B, T, A>
where
    I: Send + Sync + TrInput<T>,
    O: Send + Sync + TrOutput<T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    pub(super) fn new(core_ref: PipeCore<I, O, B, T, A>) -> Self {
        Pipeline { core_ref_: core_ref, }
    }

    /// 环形缓冲的容量（单元数）。
    pub fn capacity(&self) -> usize {
        self.core_ref_.capacity()
    }

    /// 缓冲中当前的数据量。
    pub fn data_size(&self) -> usize {
        self.core_ref_.data_size()
    }

    /// 缓冲中当前的可写空间。
    pub fn free_size(&self) -> usize {
        self.core_ref_.free_size()
    }

    pub fn is_consumer_closed(&self) -> bool {
        self.core_ref_.is_rx_closed()
    }

    pub fn is_producer_closed(&self) -> bool {
        self.core_ref_.is_tx_closed()
    }

    /// 启动背压缓存开始搬运数据。只能通过 cancellation token 来中断搬运，否则
    /// future 会一直运行直到两端中有一方停止。
    pub fn pipe_async(&mut self) -> PipelineAsync<'_, I, O, B, T, A> {
        PipelineAsync(self)
    }
}

impl<I, O, B, T, A> TrBufferState for Pipeline<I, O, B, T, A>
where
    I: Send + Sync + TrInput<T>,
    O: Send + Sync + TrOutput<T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    #[inline]
    fn capacity(&self) -> usize {
        Pipeline::capacity(self)
    }

    #[inline]
    fn data_size(&self) -> usize {
        Pipeline::data_size(self)
    }

    #[inline]
    fn free_size(&self) -> usize {
        Pipeline::data_size(self)
    }

    #[inline]
    fn is_consumer_closed(&self) -> bool {
        Pipeline::is_consumer_closed(self)
    }

    #[inline]
    fn is_producer_closed(&self) -> bool {
        Pipeline::is_producer_closed(self)
    }
}

#[gen_may_cancel_future(Pipeline)]
async fn pipeline_piping_async_<'f, I, O, B, T, A, C>(
    pipeline: &'f mut Pipeline<I, O, B, T, A>,
    cancel: &'f mut C,
) -> Option<PipelineError<usize>>
where
    I: Send + Sync + TrInput<T>,
    O: Send + Sync + TrOutput<T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
    C: TrCancellationToken + Clone,
{
    // 泵循环：**由两端设备驱动**——每次 `await` 设备的 `read_async` /
    // `write_async`，设备就绪即流动、阻塞即挂起（executor 在设备 waker
    // 就绪时重新轮询本 future）。设备错误视为「该方向无进展」（本流水线
    // 不感知具体错误，见模块文档「设备错误传播」待定项）。
    let core = &*pipeline.core_ref_;
    loop {
        if cancel.is_cancelled()
            || (core.is_tx_closed() && core.is_rx_closed())
        {
            return None;
        }
        let in_moved = core.pipe_input_once().await;
        let out_moved = core.pipe_output_once().await;
        if in_moved == 0 && out_moved == 0 {
            // 本轮无进展：
            // - 阻塞设备：上面的 await 已挂起（Pending），不会到达这里；
            // - 非阻塞设备（立即返回 0）：输入已关闭且缓冲已排空 → 流水线
            //   结束；否则**停驻**（不再流动、也不会空转），等待外部唤醒 /
            //   drop——非阻塞设备没有可注册的「有新数据」waker。
            if core.is_tx_closed() && core.data_size() == 0 {
                return None;
            }
            // 有背压时，park 到主动端自己的 WakeSlot，由对端提交路径的
            // fire_* 唤醒；只有确实只是设备无进展且无背压时才保持挂起。
            if core.free_size() == 0 {
                core.park_producer().await;
            } else if core.data_size() == 0 {
                core.park_consumer().await;
            } else {
                core::future::pending::<()>().await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// abs_buff 读写 trait
// ---------------------------------------------------------------------------

impl<C, B, T, A> TrBuffWrite<T> for Producer<C, B, T, A>
where
    // P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    type WriteAsync<'f> = ProducerWriteAsync<'f, C, B, T, A> where Self: 'f;

    type SegmMut<'f> = ReclSliceMut<'f, T,
        WriterReclaim<'f, CircCore<BufProducer<T>, C, B, T>>>
    where Self: 'f;

    type Err = ProducerError<usize>;

    #[inline]
    fn is_stuffed_closing(&self) -> bool {
        self.core_ref_.is_tx_closed() || !self.core_ref_.producer_ready(1)
    }

    #[inline]
    fn write_async<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Self::WriteAsync<'f> {
        Producer::write_async(self, demand)
    }
}

impl<C, B, T, A> TrBuffTryWrite<T> for Producer<C, B, T, A>
where
    // P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    #[inline]
    fn try_write<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> SomeOf<Self::SegmMut<'f>, Self::Err> {
        Producer::try_write(self, demand)
    }
}

impl<P, B, T, A> TrBuffRead<T> for Consumer<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    // C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    type ReadAsync<'f> = ConsumerReadAsync<'f, P, B, T, A> where Self: 'f;

    type SegmRef<'f> = ReclSliceRef<'f, T,
        ReaderReclaim<'f, CircCore<P, BufConsumer<T>, B, T>>>
    where Self: 'f;

    type Err = ConsumerError<usize>;

    #[inline]
    fn is_drained_closing(&self) -> bool {
        // 「Drained」= 不再会有新数据：读端已关闭，或（写端已关闭且缓冲已
        // 空）。写端关闭但仍有缓冲数据时不算 drained——EOF 语义要求先把残留
        // 数据读走（与 `ring_buffer` 的 `RingRx::is_drained_closing` 一致）。
        self.core_ref_.is_rx_closed()
            || (self.core_ref_.is_tx_closed() && self.core_ref_.data_size() == 0)
    }

    #[inline]
    fn read_async<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Self::ReadAsync<'f> {
        Consumer::read_async(self, demand)
    }
}

impl<P, B, T, A> TrBuffTryRead<T> for Consumer<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    // C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    #[inline]
    fn try_read<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> SomeOf<Self::SegmRef<'f>, Self::Err> {
        Consumer::try_read(self, demand)
    }
}

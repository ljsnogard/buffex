//! 环形核心：位置状态机 + 两个端（生产端 / 消费端）+ 唤醒协议。
//!
//! # 设计
//!
//! 与 [`crate::ring_buffer`] 相同的思路：读写位置（rp / wp）与**全部标志**
//! （关闭 ×2、待机 ×2、REVERSION）打包进**一个** `AtomicUsize`，单次原子加载
//! 即可看到全部状态，每次状态迁移是一个自旋 compare-exchange 循环。满 / 空
//! 用 REVERSION 方案区分（见 [`IoPos`]，不再「空一槽」）：
//!
//! * `data = (wp - rp) mod cap`（REVERSION 时 `wp == rp` 表示满环）
//! * `free = cap - data`
//!
//! 状态字高 8 位全部用于标志（无额外 `AtomicBool`）：`TX_CLOSED` / `RX_CLOSED`
//! （关闭）、`TX_STNDBY` / `RX_STNDBY`（待机 / armed）、`REVERSION`（跨末端）。
//! 位置更新（`update_pos_`）保留这些位；标志操作（`set_flag_` / `clear_flag` /
//! `arm_producer` / `arm_consumer` 等）经 CAS 原子地读写，与位置更新共享同一把
//! 原子。
//!
//! 与 `ring_buffer` 的最大不同：核心**泛型于端类型**（`CircCore<P, C, T, A>`，
//! `P` / `C` 见 `circ_buff_`），两端以**具体类型**存放在核心中——
//! 主动端的设备因此无需类型擦除；缓冲由核心**拥有**（[`mm_ptr::Owned`]，
//! 分配器 `A`，默认 `CoreAlloc`）。
//!
//! # 事件分发：只唤醒，不搬运
//!
//! 状态提交（写入 / 读取推进、关闭）后，核心向对端触发事件
//! （[`super::abs_comp_`] 的 `ProducerHookEvent` / `ConsumerHookEvent`）。
//! **数据搬运由提交路径驱动对端泵，fire 只负责唤醒**：
//!
//! * **驱动**：`advance_read`（消费端读取提交）驱动生产端泵重复拉取补位，
//!   `advance_write`（生产端写入提交）驱动消费端泵重复推送排空——泵逻辑在
//!   主动端（`DevProducer` / `DevConsumer`）的 `react_async` 中（被动端的
//!   `react_async` 是 no-op），驱动只做**非阻塞单次 poll**（`Pending` 即放弃，
//!   不自旋）。`try_*` 的 `Stuffed` / `Drained` 重试、`close_tx` 排空同走
//!   这套驱动；构建期初始填满在 `DevProducer::init_async` 中完成；
//! * **唤醒**：fire 经 armed 门控后 `check` 裁决，唤醒注册在端类型唤醒槽位
//!   （[`WakeSlot`]）中的等待者 / 泵——被动端等待者 park 时 armed
//!   （`TX_STNDBY` / `RX_STNDBY`）并注册 waker，`check` 按登记的需求下限裁决；
//!   主动端在 `init_async` 时 armed 并注册到自身槽位，供 executor 驱动的泵
//!   （如 `Pipeline`）在等待缓冲状态时被唤醒。
//!
//! # 为什么不需要端锁（STNDBY 位作 armed 协议）
//!
//! 端类型只被**泵**可变访问（`&mut P` / `&mut C`），而泵只运行在「调用者线程
//! 的驱动路径」上（SPSC：至多一个生产线程 + 一个消费线程；泵与其触发的提交
//! 在同一调用栈上串行推进）。因此对端类型的可变访问天然串行，无需加锁。
//!
//! **`STNDBY` armed 协议**（`TX_STNDBY` / `RX_STNDBY` 位）：等待者 park 时先写
//! demand（端类型普通字段）、再 CAS 置 armed 位（[`CircCore::arm_producer`] /
//! [`CircCore::arm_consumer`]）；fire 侧从状态字 **Acquire 读**到 armed 位才
//! 访问 demand / 槽位（[`CircCore::fire_producer`] / [`CircCore::fire_consumer`]），
//! 与该 CAS 建立 happens-before——demand 无需原子。等待者三态：无等待者
//! （STNDBY=0，fire 不行动）/ 正在登记（有 demand、STNDBY 仍 0，fire 不行动，
//! 等待者注册后重查）/ 等待中（STNDBY=1，接受 fire）。等待者完成 / drop 时先
//! 清位再清 demand（[`CircCore::unpark_producer`] / [`CircCore::unpark_consumer`]），
//! armed 期间 demand 恒有效。
//!
//! 唤醒的另一半不变量：**任何「重新就位」的路径都必须重查状态**——等待
//! future 注册 waker 后会重查条件（见 `spsc_` 的 `Park`），关闭标志也包含在
//! 重查条件中（`producer_ready` / `consumer_ready`）。因此事件即使丢失 / 被
//! 取代，等待者也不会永久挂起。
//!
//! # 线程安全（只使用原子）
//!
//! 全部共享状态是原子（状态字、唤醒槽位）。缓冲内存与主动设备经
//! 内部可变性（裸指针 / `UnsafeCell`）访问，其正确性由以下**调用者义务**
//! 保证（与 `ring_buffer` 的 SPSC 约定一致）：
//!
//! * 至多一个生产线程、一个消费线程；
//! * 泵只在其驱动路径（调用者线程）上执行，不与活段 / 其它泵操作重叠；
//! * 活段（写段 / 读段）与泵的操作不重叠。
//!
//! 基于这些约定，[`CircCore`] 在端类型满足 `Send + Sync` 时实现 `Send + Sync`
//! （见文件末尾的安全说明）。

use core::{
    borrow::BorrowMut,
    cell::UnsafeCell,
    future::{Future, IntoFuture},
    marker::{PhantomData, PhantomPinned},
    mem::MaybeUninit,
    pin::pin,
    ptr::{self, NonNull},
    slice,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

use abs_buff::{
    Demand,
    buffer::{TrBuffSegmMut, TrBuffSegmRef},
    io::{TrInput, TrOutput},
    error::TrTaggedError,
    gen_may_cancel_future,
    x_deps::{anylr, abs_cancel},
};
use abs_cancel::{NonCancellableToken, TrCancellationToken, TrMayCancel};
use abs_sync::ok_or::XtOkOr;

use anylr::SomeOf;
use atomex::AtomicFlags;
use atomic_sync::x_deps::{abs_sync, atomex};

use super::{
    abs_comp_::{
        ConsumerHookEvent, ProducerHookEvent, ReceiverReact,
        TrCircBuffCore, TrConsumer, TrProducer,
    },
    error_::{ConsumerError, ProducerError},
    reclaim_::{ReaderReclaim, ReclSliceMut, ReclSliceRef, WriterReclaim},
};

// ---------------------------------------------------------------------------
// 状态字布局
// ---------------------------------------------------------------------------

/// 保留高8位作为状态字（关闭 ×2 + 待机 ×2 + REVERSION）。
const RSV_BITS: u32 = 8;

/// 生产者（写端）已关闭。
const TX_CLOSED: usize = 1usize << (usize::BITS - 1);
/// 消费者（读端）已关闭。
const RX_CLOSED: usize = 1usize << (usize::BITS - 2);
/// 生产端等待唤醒。被动模式下 demand 有值
const TX_STNDBY: usize = 1usize << (usize::BITS - 3);
/// 消费端等待唤醒。
const RX_STNDBY: usize = 1usize << (usize::BITS - 4);

/// 写入端已跨段标志，即此时 wp <= rp 是合法状态
pub(super) const REVERSION: usize = 1usize << (usize::BITS - 5);

/// 状态字全部标志的掩码（位置更新（`update_state`）保留这些位）。
#[allow(unused)]
pub(super) const FLAG_MASK: usize = TX_CLOSED
    | RX_CLOSED
    | TX_STNDBY
    | RX_STNDBY
    | REVERSION;
/// 每个位置占用的位数（两个位置共享低位，两个标志占高位）。
pub(super) const POS_BITS: u32 = (usize::BITS - RSV_BITS) / 2;
/// 位置掩码。
pub(super) const POS_MASK: usize = (1usize << POS_BITS) - 1;

pub(super) const MIN_CAPACITY: usize = 2;
/// 环形缓冲的最大容量（与 `ring_buffer` 的 `MAX_CAPACITY` 同量级）。
pub(super) const MAX_CAPACITY: usize = POS_MASK;

/// 环形位置状态：读者位置 `rp` / 写者位置 `wp`（均为 `[0, capacity_)` 内的
/// 物理索引）+ 跨末端标志 `rv`（即状态字中的 [`REVERSION`] 位）。
///
/// # 位置约定（REVERSION 方案，不再使用「空一槽」）
///
/// 读写位置打包进状态字低位（`rp` 占低 `POS_BITS` 位、`wp` 占次 `POS_BITS`
/// 位），容量**全部可用**——不再刻意保留一个空槽。满 / 空由 `rv` 区分：
///
/// * `rv == false`（写者未跨过物理末端）：原始 `wp >= rp`，数据量 = `wp - rp`；
///   其中 `wp == rp` 表示**空**（data = 0）；
/// * `rv == true`（写者已跨过物理末端，原始 `wp <= rp`）：数据量 =
///   `wp + capacity - rp`；其中 `wp == rp` 表示**满**（整环都是数据，
///   data = capacity）。
///
/// `rv` 只随位置推进而置位 / 清除（其余标志位原样保留）：
///
/// * [`IoPos::advance_wp`]：写者越过物理末端（`wp + amount >= capacity`）时
///   置位，且置位后一直保持——写者始终「在读者之后（含追上成满环）」；
/// * [`IoPos::advance_rp`]：读者越过物理末端（`rp + amount >= capacity`）时
///   清除——读者跨过末端后，写者的原始位置重新位于读者之前，恢复未跨状态。
pub(super) struct IoPos {
    /// 与 REVERSION flag 含义一致：写者是否已越过缓冲区物理末端（此时原始
    /// `wp <= rp`；`wp == rp` 表示环满）。
    pub rv: bool,
    /// 读者位置（物理索引，`[0, capacity_)`）。
    pub rp: usize,
    /// 写者位置（物理索引，`[0, capacity_)`）。
    pub wp: usize,
    /// circular buff 的容量。
    capacity_: usize,
}

impl IoPos {
    /// 本类型在状态字中「拥有」的位：两个位置字段（`rp` 低 `POS_BITS` 位、
    /// `wp` 次 `POS_BITS` 位）与 REVERSION 位。`pack` 只覆盖这些位，其余
    /// 标志位由传入的基座状态字原样保留。
    pub const MASK: usize = REVERSION | POS_MASK | (POS_MASK << POS_BITS);

    /// 从 `atm_stat_` 的状态字解出位置与保留标志。
    pub fn unpack(state: usize, cap: usize) -> Self {
        let rp = state & POS_MASK;
        let wp = (state >> POS_BITS) & POS_MASK;
        let rv = has_flag(state, REVERSION);
        IoPos { rv, rp, wp, capacity_: cap }
    }

    /// 当前可读数据量（遵循上述约定：空环 = 0、满环 = 容量）。
    #[inline]
    pub fn data_size(&self) -> usize {
        if self.rv && self.wp == self.rp {
            // 写者跨过末端后恰好追上读者：整环都是数据（满）。
            self.capacity_
        } else {
            // 未跨（wp > rp）或已跨未满（wp < rp）：`(wp - rp) mod capacity`。
            (self.wp + self.capacity_ - self.rp) % self.capacity_
        }
    }

    /// 当前可写空间量：`capacity - data_size`（满环 = 0、空环 = 容量）。
    #[inline]
    pub fn free_size(&self) -> usize {
        self.capacity_ - self.data_size()
    }

    /// 以传入的基座状态字 `state` 打包回完整状态字：`state` 中本类型不拥有的
    /// 位（[`IoPos::MASK`] 之外——关闭、待机、待办泵等标志）**原样保留**；
    /// 本类型拥有的位（`rp` / `wp` 位置字段与 REVERSION 位）用自身的新值
    /// **覆盖**（先清除基座中的旧值再写入，而非按位或——否则旧位置会残留并
    /// 与新位置混合）。
    pub fn pack(&self, state: usize) -> usize {
        let s = (state & !Self::MASK) | self.rp | (self.wp << POS_BITS);
        if self.rv { s | REVERSION } else { s & !REVERSION }
    }

    /// 推进写者位置：`wp += amount`（物理上环绕）；**写者越过缓冲区物理末端
    /// （`wp + amount >= capacity`）时设置 REVERSION flag**，且一旦置位保持
    /// 到读者追上来为止。返回推进后的**新位置状态**（不含任何标志位）；需要
    /// 写回 `atm_stat_` 时以原状态字为基座调用 [`IoPos::pack`]。
    ///
    /// 前置：`amount <= free_size`（不允许写过头；恰好写满时进入
    /// `wp == rp && rv` 的满态）。
    pub fn advance_wp(&self, amount: usize) -> Self {
        debug_assert!(amount <= self.free_size());
        let new_wp = self.wp + amount;
        let crossed = new_wp >= self.capacity_;
        Self {
            // 越过末端置位；已置位则保持（写者仍在读者之后，直到读者追上）。
            rv: self.rv || crossed,
            rp: self.rp,
            wp: new_wp % self.capacity_,
            capacity_: self.capacity_,
        }
    }

    /// 推进读者位置：`rp += amount`（物理上环绕）；**读者越过缓冲区物理末端
    /// （`rp + amount >= capacity`）时清除 REVERSION flag**——读者跨过末端后，
    /// 写者的原始位置重新位于读者之前，恢复未跨状态。返回推进后的**新位置状态**
    /// （不含任何标志位）；需要写回 `atm_stat_` 时以原状态字为基座调用
    /// [`IoPos::pack`]。
    ///
    /// 前置：`amount <= data_size`（不允许读过头；恰好读空时回到
    /// `wp == rp && !rv` 的空态）。
    pub fn advance_rp(&self, amount: usize) -> Self {
        debug_assert!(amount <= self.data_size());
        let new_rp = self.rp + amount;
        let crossed = self.rp + amount >= self.capacity_;
        Self {
            rv: self.rv && !crossed,
            rp: new_rp % self.capacity_,
            wp: self.wp,
            capacity_: self.capacity_,
        }
    }
}

#[inline]
fn has_flag(state: usize, flag: usize) -> bool {
    state & flag != 0
}

/// 同步路径的**非阻塞单次探测**：只 poll 一次，`Pending` 立即放弃。
/// 这不是忙等；真正的等待由设备 waker 或主动端 WakeSlot 负责。
pub(super) fn poll_once<F: Future>(fut: core::pin::Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    fut.poll(&mut cx)
}

// ---------------------------------------------------------------------------
// 环形核心
// ---------------------------------------------------------------------------

/// 环形核心：一个原子状态字 + 自有缓冲 + 两个端 + 泵状态。
/// 必须保证 Send + Sync。
///
/// **公开可见性说明**：本类型被半部（`spsc_::Producer` /
/// `spsc_::Consumer`）借出的段类型指名（`WriterReclaim<CircCore>`），
/// 因此必须 `pub`；它不是稳定的公共 API，通常只作为段类型的泛型参数出现。
///
/// # 泛型参数
///
/// * `P`——生产端类型：被动 `BuffProducer`
///   / 主动 `DeviceProducer`，实现
///   [`TrProducer`](super::abs_comp_::TrProducer)；
/// * `C`——消费端类型：被动 `BuffConsumer`
///   / 主动 `DeviceConsumer`，实现
///   [`TrConsumer`](super::abs_comp_::TrConsumer)；
/// * `T`——元素类型（默认 `u8`）；
/// * `A`——分配器（默认 `CoreAlloc`）：用于分配自有缓冲（[`Owned`]）。
///
/// 泛型于端类型是本设计**刻意为之**：主动端的设备以具体类型存放
/// （`producer_: UnsafeCell<P>` / `consumer_: UnsafeCell<C>`），**不做类型擦除**
/// （`TrInput` / `TrOutput` 带泛型关联类型、不能直接 `dyn`）。
///
/// # 职责
///
/// * **状态机**：`rp` / `wp` 与全部标志（关闭、待机、泵互斥、待办泵）打包进
///   一个 `AtomicUsize`（见本文件顶部的布局常量），单次原子加载看到全部状态，
///   每次迁移是一个自旋 compare-exchange 循环；
/// * **区域借出**：`try_write_at` / `try_read_at` 尊重 `Demand` 的 `[min, max]`
///   区间，把可写 / 可读区借出为两段式段（[`super::reclaim_`]），段 drop 时
///   按已消费量提交回本核心（经 [`TrCircBuffCore`](super::abs_comp_::TrCircBuffCore)）；
/// * **事件分发**：提交路径上向对端触发事件——被动端 `signal` 唤醒槽位，
///   主动端置待办泵标志 + `drive()`；
/// * **泵**：同步上下文走非阻塞尝试（单次 poll，`Pending` 即放弃、不自旋）；
///   被动端异步等待走 executor 驱动（`await` 设备 future）。泵自身的段 drop
///   提交会触发对端事件，双主动（`Pipeline`）由流水线 future 异步驱动。
pub struct CircCore<P, C, B, T = u8>
where
    P: TrProducer<Data = T>,
    C: TrConsumer<Data = T>,
    T: 'static,
    B: BorrowMut<[MaybeUninit<T>]>,
{
    buffer_: B,
    /// `rp`（低 `POS_BITS` 位）| `wp`（次 `POS_BITS` 位）| 全部标志（高位：
    /// 关闭 ×2、待机 ×2、泵互斥 ×1、待办泵 ×2）。
    atm_stat_: AtomicFlags<usize>,
    /// 环形缓冲（拥有）：统一 `[MaybeUninit<T>]` 视图。基址经裸指针访问，
    /// `Owned` 只负责所有权与生命周期。
    producer_: UnsafeCell<P>,
    consumer_: UnsafeCell<C>,
    _unuse_t_: PhantomData<fn() -> T>,
    _pinning_: PhantomPinned,
}

/// 设计为只给 SPSC 中的 Consumer<P, C, B, T, A> 或者 Producer<P, C, B, T, A> 
/// 调用。实际上不可并发调用。
impl<C, B, T> CircCore<BufProducer<T>, C, B, T>
where
    // P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
{
    pub fn try_write_<'f>(
        &'f self,
        demand: &'f Demand<usize>,
    ) -> SomeOf<ReclSliceMut<'f, T, WriterReclaim<'f, Self>>, ProducerError<usize>> {
        let x: SomeOf<
            ReclSliceMut<'_, T, WriterReclaim<'_, Self>>,
            ProducerError<usize>,
        > = self
            .try_write_at(demand)
            .map(|(start, take)| self.create_write_segm(start, take))
            .into();
        if x.contains_left() {
            return x;
        };
        let err = x.as_ref().pick_right().expect("");
        if err.err_tag().should_terminate() {
            return x;
        };
        // 对端主动消费时，先非阻塞排空一轮，再重试写入。
        self.pump_output_sync();
        self.try_write_at(demand)
            .map(|(start, take)| self.create_write_segm(start, take))
            .into()
    }

    pub fn write_async_<'f>(
        &'f self,
        demand: &'f Demand<usize>,
    ) -> CorePassiveWriteAsync<'f, C, B, T> {
        CorePassiveWriteAsync(self, demand)
    }

    pub fn on_buf_producer_drop_(&self) {
        self.clear_flag(TX_STNDBY);
        let p = unsafe { &*self.producer_.get() };
        let _ = p.try_reset_demand();
        // if let Result::Err(_) = x {
            // todo: clear existing demand
        // }
    }
}

impl<P, B, T> CircCore<P, BufConsumer<T>, B, T>
where
    P: Send + Sync + TrProducer<Data = T>,
    // C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync,
{
    pub fn try_read_<'f>(
        &'f self,
        demand: &'f Demand<usize>,
    ) -> SomeOf<ReclSliceRef<'f, T, ReaderReclaim<'f, Self>>, ConsumerError<usize>> {
        // #[allow(clippy::type_complexity)]
        let x: SomeOf<
            ReclSliceRef<'_, T, ReaderReclaim<'_, Self>>,
            ConsumerError<usize>,
        > = self
            .try_read_at(demand)
            .map(|(start, take)| self.create_read_segm(start, take))
            .into();
        if x.contains_left() {
            return x;
        };
        let err = x.as_ref().pick_right().expect("");
        if err.err_tag().should_terminate() {
            return x;
        };
        // 对端主动生产时，先非阻塞补入一轮，再重试读取。
        self.pump_input_sync();
        self.try_read_at(demand)
            .map(|(start, take)| self.create_read_segm(start, take))
            .into()
    }

    pub fn read_async_<'f>(
        &'f self,
        demand: &'f Demand<usize>,
    ) -> CorePassiveReadAsync<'f, P, B, T> {
        CorePassiveReadAsync(self, demand)
    }

    pub fn on_buf_consumer_drop_(&self) {
        self.clear_flag(RX_STNDBY);
        let c = unsafe { &*self.consumer_.get() };
        let _ = c.try_reset_demand();
        // if let Result::Err(_) = x {
            // todo: clear existing demand
        // }
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<P, C, B, T> CircCore<P, C, B, T>
where
    P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync,
{
    /// 构造核心：分配缓冲、状态归零、装入两端。
    ///
    /// `capacity` 必须已在 [`MIN_CAPACITY`] ..= [`MAX_CAPACITY`] 之间（builder
    /// 保证）。
    pub(super) fn new(
        producer: P,
        consumer: C,
        buffer: B,
    ) -> Self {
        CircCore {
            buffer_: buffer,
            atm_stat_: AtomicFlags::new(AtomicUsize::new(0usize)),
            producer_: UnsafeCell::new(producer),
            consumer_: UnsafeCell::new(consumer),
            _unuse_t_: PhantomData,
            _pinning_: PhantomPinned,
        }
    }

    // ------------------------------------------------------------------
    // 状态查询
    // ------------------------------------------------------------------

    #[inline]
    pub(super) fn capacity(&self) -> usize {
        self.buffer_.borrow().len()
    }

    /// 当前可读数据量。
    #[inline]
    pub(super) fn data_size(&self) -> usize {
        let state = self.atm_stat_.value();
        let pos = IoPos::unpack(state, self.capacity());
        pos.data_size()
    }

    /// 当前可写空间量。
    #[inline]
    pub(super) fn free_size(&self) -> usize {
        let state = self.atm_stat_.value();
        let pos = IoPos::unpack(state, self.capacity());
        pos.free_size()
    }

    #[inline]
    pub(super) fn is_tx_closed(&self) -> bool {
        has_flag(self.atm_stat_.value(), TX_CLOSED)
    }

    #[inline]
    pub(super) fn is_rx_closed(&self) -> bool {
        has_flag(self.atm_stat_.value(), RX_CLOSED)
    }

    pub(super) fn producer_ptr(&self) -> NonNull<P> {
        unsafe { NonNull::new_unchecked(self.producer_.get()) }
    }

    pub(super) fn consumer_ptr(&self) -> NonNull<C> {
        unsafe { NonNull::new_unchecked(self.consumer_.get()) }
    }

    // ------------------------------------------------------------------
    // 区域借出（尊重 Demand 语义）
    // ------------------------------------------------------------------

    /// 借出可写区，返回 `(start, take)`。
    ///
    /// 尊重 `Demand` 的 `[min, max]` 区间：**可写空间不足下限时不返回**（返回
    /// `Stuffed`），满足时最多借出 `max`。区域可能跨末端环绕（由段类型表达）。
    pub(super) fn try_write_at(
        &self,
        demand: &Demand<usize>,
    ) -> Result<(usize, usize), ProducerError<usize>> {
        let min_len = demand.min().copied().unwrap_or(0);
        let max_len = demand.max().copied().unwrap_or(usize::MAX);
        let state = self.atm_stat_.value();
        let cap = self.capacity();
        let pos = IoPos::unpack(state, cap);
        let free = pos.free_size();
        if free == 0 || free < min_len {
            if has_flag(state, TX_CLOSED) {
                return Err(ProducerError::Closing);
            }
            return Err(ProducerError::Stuffed(pos.wp));
        }
        let take = core::cmp::min(max_len, free);
        debug_assert!(take > 0 && take >= min_len);
        Ok((pos.wp, take))
    }

    /// 借出可读区，返回 `(start, take)`。
    ///
    /// 尊重 `Demand` 的 `[min, max]` 区间：可读数据不足下限且未关闭时**不返回**
    /// （返回 `Drained`）；**EOF 例外**——写端已关闭（不再会有更多数据）时，
    /// 返回现有部分（可能不足下限）；读端已关闭或缓冲区已空时返回 `Closing` /
    /// `Drained`。
    pub(super) fn try_read_at(
        &self,
        demand: &Demand<usize>,
    ) -> Result<(usize, usize), ConsumerError<usize>> {
        let min_len = demand.min().copied().unwrap_or(0);
        let max_len = demand.max().copied().unwrap_or(usize::MAX);
        let state = self.atm_stat_.value();
        let cap = self.capacity();
        let pos = IoPos::unpack(state, cap);
        let ready = pos.data_size();
        if ready == 0 {
            if has_flag(state, TX_CLOSED) || has_flag(state, RX_CLOSED) {
                return Err(ConsumerError::Closing); // EOF：写端已关且读空
            }
            return Err(ConsumerError::Drained(pos.rp));
        }
        if ready < min_len && !has_flag(state, TX_CLOSED) && !has_flag(state, RX_CLOSED) {
            return Err(ConsumerError::Drained(pos.rp)); // 不足下限且未关闭：等待更多
        }
        let take = core::cmp::min(max_len, ready);
        debug_assert!(take > 0);
        Ok((pos.rp, take))
    }

    /// 写者可以继续的条件（供等待 future 的 park 检查）：
    /// 可写空间 ≥ min，或写端已关闭（返回 `Closing` 不再等待）。
    pub(super) fn producer_ready(&self, min: usize) -> bool {
        self.try_write_at(&Demand::at_least(min.max(1))).is_ok()
            || self.is_tx_closed()
    }

    /// 读者可以继续的条件：可读数据 ≥ min（或 EOF 有部分数据），或任一端关闭。
    #[allow(unused)]
    pub(super) fn consumer_ready(&self, min: usize) -> bool {
        self.try_read_at(&Demand::at_least(min.max(1))).is_ok()
            || self.is_tx_closed()
            || self.is_rx_closed()
    }

    // ------------------------------------------------------------------
    // 泵：同步非阻塞探测 + 异步背压 park
    // ------------------------------------------------------------------

    /// 同步输入泵（非阻塞、不自旋）。
    ///
    /// # 调用上下文
    /// - `try_read_`：`Drained` 且未终止时，先拉取一轮再重试；
    /// - `advance_read`：被动消费端读取提交后，立即补位；
    /// - 仅“主动生产 + 被动消费”时实际工作；双主动交给 Pipeline，避免同步递归。
    ///
    /// # Safety / 别名
    /// 调用点都处于“没有同侧活段 / 没有其它泵正在运行”的路径上；本方法内部
    /// 通过 `UnsafeCell` 取 `&mut P`，与调用者持有的半部借用互斥（SPSC）。
    fn pump_input_sync(&self) -> usize {
        let producer = unsafe { &*self.producer_.get() };
        if producer.is_passive() {
            return 0;
        }
        let consumer = unsafe { &*self.consumer_.get() };
        if !consumer.is_passive() {
            return 0;
        }

        let mut total = 0;
        loop {
            let state = self.atm_stat_.value();
            if has_flag(state, TX_CLOSED) || has_flag(state, RX_CLOSED) {
                break;
            }
            let pos = IoPos::unpack(state, self.capacity());
            let free = pos.free_size();
            if free == 0 {
                break;
            }

            let mut segm = self.create_write_segm(pos.wp, free);
            let producer = unsafe { &mut *self.producer_.get() };
            let outcome = {
                let may_fut = producer
                    .react_async(&mut segm)
                    .may_cancel_with(NonCancellableToken::shared_mut());
                let mut fut = pin!(may_fut.into_future());
                poll_once(fut.as_mut())
            };
            let moved = segm.capacity() - segm.least_count();
            drop(segm); // 提交，触发 fire_consumer
            total += moved;

            if moved == 0 || !matches!(outcome, Poll::Ready(ReceiverReact::Reacted)) {
                break;
            }
        }
        total
    }

    /// 同步输出泵（非阻塞、不自旋）。
    ///
    /// # 调用上下文
    /// - `try_write_`：`Stuffed` 且未终止时，先排空一轮再重试；
    /// - `advance_write` / `close_tx`：被动生产端写入提交 / 关闭后，立即排空；
    /// - 仅“被动生产 + 主动消费”时实际工作；双主动交给 Pipeline，避免同步递归。
    ///
    /// # Safety / 别名
    /// 同 [`CircCore::pump_input_sync`]：调用点没有同侧活段 / 其它泵并发，
    /// `&mut C` 的取得满足 SPSC。
    fn pump_output_sync(&self) -> usize {
        let consumer = unsafe { &*self.consumer_.get() };
        if consumer.is_passive() {
            return 0;
        }
        let producer = unsafe { &*self.producer_.get() };
        if !producer.is_passive() {
            return 0;
        }

        let mut total = 0;
        loop {
            let state = self.atm_stat_.value();
            if has_flag(state, RX_CLOSED) {
                break;
            }
            let pos = IoPos::unpack(state, self.capacity());
            let data = pos.data_size();
            if data == 0 {
                break;
            }

            let mut segm = self.create_read_segm(pos.rp, data);
            let consumer = unsafe { &mut *self.consumer_.get() };
            let outcome = {
                let may_fut = consumer
                    .react_async(&mut segm)
                    .may_cancel_with(NonCancellableToken::shared_mut());
                let mut fut = pin!(may_fut.into_future());
                poll_once(fut.as_mut())
            };
            let moved = segm.capacity() - segm.least_count();
            drop(segm); // 提交，触发 fire_producer
            total += moved;

            if moved == 0 || !matches!(outcome, Poll::Ready(ReceiverReact::Reacted)) {
                break;
            }
        }
        total
    }

    /// 异步排空输出：持续驱动主动消费端，直到环形缓冲中的数据全部写出。
    ///
    /// # 调用上下文
    /// 供被动生产端在关闭 / shutdown 前调用，确保残留数据真正到达输出设备。
    /// 设备 Pending 时本方法会 `await`，由设备自己的 waker 唤醒。
    async fn drain_output_async_<'f, K>(
        &'f self,
        cancel: &'f mut K,
    )
    where
        K: TrCancellationToken + Clone,
    {
        loop {
            if cancel.is_cancelled() {
                break;
            }
            let consumer = unsafe { &mut *self.consumer_.get() };
            let moved = consumer
                .pump_async(self)
                .may_cancel_with(cancel)
                .await;
            if self.data_size() == 0 && self.is_rx_closed() {
                break;
            }
            if moved == 0 {
                break;
            }
        }
    }

    /// 生产端背压 park：把 waker 注册到主动生产端自己的 WakeSlot，
    /// 等待消费端读取提交后的 `fire_producer` 唤醒。
    /// 生产端背压 park：把 waker 注册到主动生产端自己的 WakeSlot，
    /// 等待消费端读取提交后的 `fire_producer` 唤醒。
    ///
    /// # 调用上下文
    /// 当前主要由全主动 `Pipeline` 在“输入泵无进展且缓冲满”时调用；
    /// 也适合任何“连续泵” future 在等待可写空间时使用。
    ///
    /// # Safety / 清理
    /// 本方法通过 `ActiveParkGuard` 保证 future 被取消 / drop 时注销
    /// `WakeSlot` 并清除 `TX_STNDBY`。
    pub(super) async fn park_producer(&self) {
        let Some(slot) = unsafe { &*self.producer_.get() }.wakeslot() else {
            return;
        };
        let mut guard = ActiveParkGuard {
            slot,
            waiter: Waiter::new(),
            core: self,
            registered: false,
            unarm: |core| core.unset_tx_standby(),
        };
        self.set_tx_standby();
        let _ = core::future::poll_fn(|cx| {
            guard.waiter.waker = Some(cx.waker().clone());
            guard.slot.register(&guard.waiter);
            guard.registered = true;
            if self.try_write_at(&Demand::at_least(1)).is_ok()
                || self.is_tx_closed()
                || self.is_rx_closed()
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    /// 消费端背压 park：把 waker 注册到主动消费端自己的 WakeSlot，
    /// 等待生产端写入提交后的 `fire_consumer` 唤醒。
    ///
    /// # 调用上下文
    /// 当前主要由全主动 `Pipeline` 在“输出泵无进展且缓冲空”时调用；
    /// 也适合任何“连续泵” future 在等待可读数据时使用。
    ///
    /// # Safety / 清理
    /// 同 [`CircCore::park_producer`]：通过 `ActiveParkGuard` 保证取消 / drop
    /// 时注销 `WakeSlot` 并清除 `RX_STNDBY`。
    pub(super) async fn park_consumer(&self) {
        let Some(slot) = unsafe { &*self.consumer_.get() }.wakeslot() else {
            return;
        };
        let mut guard = ActiveParkGuard {
            slot,
            waiter: Waiter::new(),
            core: self,
            registered: false,
            unarm: |core| core.unset_rx_standby(),
        };
        self.set_rx_standby();
        let _ = core::future::poll_fn(|cx| {
            guard.waiter.waker = Some(cx.waker().clone());
            guard.slot.register(&guard.waiter);
            guard.registered = true;
            if self.try_read_at(&Demand::at_least(1)).is_ok()
                || self.is_tx_closed()
                || self.is_rx_closed()
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    // ------------------------------------------------------------------
    // 段构建（提交目标：本核心，经 TrCircBuffCore）
    // ------------------------------------------------------------------

    /// 构建写段：覆盖 `[start, start+take)`，跨末端时拆成两段物理空间。
    ///
    /// 段 drop 时经 [`WriterReclaim`] 提交回本核心（推进写位置并触发消费端
    /// 事件）。
    pub(super) fn create_write_segm<'s>(
        &'s self,
        start: usize,
        take: usize,
    ) -> ReclSliceMut<'s, T, WriterReclaim<'s, Self>> {
        // SAFETY: `start`/`take` 来自 `try_write_at`，区域在缓冲内；可写区与
        // 其他活段 / 泵操作不重叠是调用者义务（SPSC）。
        let whole: &'s mut [MaybeUninit<T>] = self.buffer_view_mut();
        let first = core::cmp::min(take, self.capacity() - start);
        let pieces = if first < take {
            let (head, tail) = whole.split_at_mut(start);
            let b = &mut head[..take - first];
            super::reclaim_::SegmSlicesMut::Two(tail, b)
        } else {
            super::reclaim_::SegmSlicesMut::One(&mut whole[start..start + take])
        };
        ReclSliceMut::new(pieces, WriterReclaim::new(self))
    }

    /// 构建读段：覆盖 `[start, start+take)`，跨末端时拆成两段物理空间。
    ///
    /// 段 drop 时经 [`ReaderReclaim`] 提交回本核心（推进读位置并触发生产端
    /// 事件）。
    pub(super) fn create_read_segm<'s>(
        &'s self,
        start: usize,
        take: usize,
    ) -> ReclSliceRef<'s, T, ReaderReclaim<'s, Self>> {
        // SAFETY: 同 [`CircCore::write_segm`]。
        let base = self.buffer_.borrow().as_ptr().cast::<T>();
        let first = core::cmp::min(take, self.capacity() - start);
        let pieces = if first < take {
            let a = unsafe { slice::from_raw_parts(base.add(start), first) };
            let b = unsafe { slice::from_raw_parts(base, take - first) };
            super::reclaim_::SegmSlicesRef::Two(a, b)
        } else {
            let a = unsafe { slice::from_raw_parts(base.add(start), take) };
            super::reclaim_::SegmSlicesRef::One(a)
        };
        ReclSliceRef::new(pieces, ReaderReclaim::new(self))
    }

    // ------------------------------------------------------------------
    // 状态迁移（提交路径：推进位置 / 关闭，随后触发对端事件）
    // ------------------------------------------------------------------

    /// 写提交：按已消费量推进写位置，触发消费端事件。
    pub(super) fn advance_write(&self, amount: usize) {
        let cap = self.capacity();
        self.update_pos_(|s| {
            let pos = IoPos::unpack(s, cap);
            pos.advance_wp(amount).pack(s)
        });
        // 先触发消费端事件（反映刚提交的可读数据；fire 只唤醒不搬运），再
        // 驱动对端（消费端）泵重复推送到输出设备（一端主动一端被动时实际
        // 推送；被动端 / 双主动按内部门控 no-op）。泵的提交（advance_read）
        // 会再次触发生产端事件，唤醒等待可写空间的写者。
        let ev = if self.is_tx_closed() {
            ConsumerHookEvent::ProducerClose(self.data_size())
        } else {
            ConsumerHookEvent::Available(self.data_size())
        };
        self.fire_consumer(ev);
        self.pump_output_sync();
    }

    /// 读提交：按已消费量推进读位置，触发生产端事件。
    pub(super) fn advance_read(&self, amount: usize) {
        let cap = self.capacity();
        self.update_pos_(|s| {
            let pos = IoPos::unpack(s, cap);
            pos.advance_rp(amount).pack(s)
        });
        // 先触发生产端事件（反映刚释放的可写空间），再驱动对端（生产端）泵
        // 重复拉取补位（一端主动一端被动时实际拉取）。泵的提交（advance_write）
        // 会再次触发消费端事件，唤醒等待数据的读者。
        let event = if self.is_rx_closed() {
            ProducerHookEvent::ConsumerClose(self.free_size())
        } else {
            ProducerHookEvent::Available(self.free_size())
        };
        self.fire_producer(event);
        self.pump_input_sync();
    }

    /// 关闭写端：不再接受写入，触发消费端事件（`ProducerClose`）。
    ///
    /// 对端（消费端）为主动时，先**驱动输出泵排空**残留数据（`pump_output_sync`，非阻塞
    /// 尝试、不自旋；内部门控保证仅一端主动一端被动时实际排空）——写端关闭
    /// 后不再有新数据，残留必须送达输出设备（如 `buffex_iroh` 的 `shutdown`）。
    pub(super) fn close_tx(&self) {
        self.set_flag_(TX_CLOSED);
        self.fire_consumer(ConsumerHookEvent::ProducerClose(self.data_size()));
        self.pump_output_sync();
    }

    /// 异步关闭写端：设置关闭标志后，持续异步排空主动消费端的残留数据。
    ///
    /// 调用者可通过 `cancel` 中断排空；一旦取消，立即停止排空并返回，
    /// 关闭标志已经设置，最终收尾由调用方 / Drop 路径继续完成。
    pub(super) async fn close_tx_async<'f, K>(
        &'f self,
        cancel: &'f mut K,
    )
    where
        K: TrCancellationToken + Clone,
    {
        self.close_tx();
        self.drain_output_async_(cancel).await;
    }

    /// 关闭读端：不再读取，触发生产端事件（`ConsumerClose`）。
    pub(super) fn close_rx(&self) {
        self.set_flag_(RX_CLOSED);
        self.fire_producer(ProducerHookEvent::ConsumerClose(self.free_size()));
    }

    fn update_pos_<F>(&self, f: F)
    where
        F: Fn(usize) -> usize,
    {
        let expect = |_| true;
        let desire = f;
        self.atm_stat_
            .try_spin_compare_exchange_weak(expect, desire);
    }

    fn set_flag_(&self, flag: usize) {
        let expect = |s| s & flag == 0;
        let desire = |s| s | flag;
        self.atm_stat_
            .try_spin_compare_exchange_weak(expect, desire);
    }

    // ------------------------------------------------------------------
    // 事件分发与唤醒（数据搬运由 advance_* 驱动对端泵，fire 只唤醒）
    // ------------------------------------------------------------------

    /// 触发生产端事件（消费端完成读取 / 关闭后）。
    ///
    /// **本方法只唤醒，不搬运**——数据的重复拉取由 `advance_read` 直接驱动
    /// 对端泵（[`CircCore::pump_input_sync`]，主动端 `react_async` 是搬运循环、
    /// 被动端是 no-op），fire 的职责是唤醒注册在端类型唤醒槽位中的等待者：
    ///
    /// * 被动端（`BufProducer`）：等待可写空间的写者 park 时 **armed**
    ///   （`TX_STNDBY=1`，经 [`CircCore::arm_producer`]）并注册 waker；fire 先
    ///   检查 armed 位，armed 才访问 demand（STNDBY armed 协议，见下文），
    ///   `check` 按登记的需求下限裁决，感兴趣才 `signal` 唤醒槽位；
    /// * 主动端（`DevProducer`）：`init_async` 时 armed（等待空位）并注册到
    ///   自身的唤醒槽位；fire 经 armed 门控后 `check` 裁决（有可写空间 /
    ///   关闭等），感兴趣即 `signal` 唤醒——供 executor 驱动的泵（如
    ///   `Pipeline`）在等待缓冲状态时被唤醒。
    ///
    /// `TX_STNDBY=0` 时（无等待者 / 主动端未 armed）直接返回——等待者注册后
    /// 会重查条件、泵会重查状态，不会丢唤醒。
    ///
    /// # STNDBY armed 协议（demand 可见性）
    ///
    /// 等待者先写 demand（普通字段）、再 CAS 置 `TX_STNDBY`（AcqRel）；本方法
    /// 对状态字的 Acquire 读（`has_flag(…, TX_STNDBY)`）与该 CAS 建立
    /// happens-before，故此后对 demand（普通字段）的读取必为当前等待者的值。
    ///
    /// # Safety（取 `&P`）
    ///
    /// 本方法只由 `advance_read` / `close_rx` 触发。`advance_read` 先驱动
    /// 对端泵（[`CircCore::pump_input_sync`]，其中 `&mut P` 只在本方法返回前使用）
    /// 再触发本方法（`&P`）——两者顺序执行，不与任何活借用重叠。
    fn fire_producer(&self, event: ProducerHookEvent) {
        // 未 armed：无等待者（被动）或泵未 park（主动）——不唤醒。
        if !has_flag(self.atm_stat_.value(), TX_STNDBY) {
            return;
        }
        let producer = unsafe { &*self.producer_.get() };
        let _ = producer.check(event); // check 内部按兴趣 signal 自身槽位
    }

    /// 触发消费端事件（生产端完成写入 / 关闭后）。与 [`CircCore::fire_producer`]
    /// 对称：只唤醒不搬运（`RX_STNDBY` armed 门控，[`CircCore::arm_consumer`]）。
    fn fire_consumer(&self, event: ConsumerHookEvent) {
        if !has_flag(self.atm_stat_.value(), RX_STNDBY) {
            return;
        }
        let consumer = unsafe { &*self.consumer_.get() };
        let _ = consumer.check(event);
    }

    /// 生产端进入「等待中」（armed）：CAS 置 `TX_STNDBY`。
    ///
    /// 等待者（被动写者 / 主动输入泵）在**写 demand / 决定等待之后**调用——
    /// 置位后 fire 侧才可能访问 demand / 唤醒槽位。置位是无条件的（已置位则
    /// 保持不变）：同一时刻至多一个等待者（SPSC）。
    pub(super) fn set_tx_standby(&self) {
        let expect = |_| true;
        let desire = |s| s | TX_STNDBY;
        self.atm_stat_
            .try_spin_compare_exchange_weak(expect, desire);
    }

    /// 消费端进入「等待中」（armed）：CAS 置 `RX_STNDBY`。语义同
    /// [`CircCore::arm_producer`]。
    pub(super) fn set_rx_standby(&self) {
        let expect = |_| true;
        let desire = |s| s | RX_STNDBY;
        self.atm_stat_
            .try_spin_compare_exchange_weak(expect, desire);
    }

    /// 生产端退出「等待中」：CAS 清 `TX_STNDBY`。等待者完成 / drop（取消）时
    /// 调用——armed 期间 fire 侧对 demand / 槽位的访问与之互斥（AcqRel）。
    pub(super) fn unset_tx_standby(&self) {
        self.clear_flag(TX_STNDBY);
    }

    /// 消费端退出「等待中」：CAS 清 `RX_STNDBY`。语义同
    /// [`CircCore::unpark_producer`]。
    pub(super) fn unset_rx_standby(&self) {
        self.clear_flag(RX_STNDBY);
    }

    /// 原子地读取并清除一个标志（等价于 `swap(false)` 的原子读-清）。
    ///
    /// CAS 循环保证「读旧值 + 清位」是一个原子操作：成功时返回旧值中该位
    /// 是否置位，其余位（位置、其他标志）不受影响。
    fn clear_flag(&self, flag: usize) -> bool {
        let expect = |s| s & flag == flag;
        let desire = |s| s & !flag;
        let res = self
            .atm_stat_
            .try_spin_compare_exchange_weak(expect, desire);
        res.is_succ()
    }

    /// 整块缓冲的可变视图（内部可变性：由 SPSC 借用纪律保证不与活段重叠）。
    #[allow(clippy::mut_from_ref)]
    fn buffer_view_mut(&self) -> &mut [MaybeUninit<T>] {
        let base = self.buffer_.borrow().as_ptr() as *const _ as *mut MaybeUninit<T>;
        unsafe { slice::from_raw_parts_mut(base, self.capacity()) }
    }
}

impl<I, O, B, T> CircCore<DevProducer<I, T>, DevConsumer<O, T>, B, T>
where
    I: Send + Sync + TrInput<T>,
    O: Send + Sync + TrOutput<T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync,
{
    // ------------------------------------------------------------------
    // 双主动流水线泵（`Pipeline::pipe_async`）：await 设备，阻塞即挂起
    // ------------------------------------------------------------------

    /// 流水线的一轮输入泵（异步）：从输入设备读**一次**（`await`，设备阻塞
    /// 即挂起、由 executor 驱动其 waker），直接写入缓冲的一段**物理连续**
    /// 可写区，随后立即提交（`advance_write`）。返回本轮搬入字节数。
    ///
    /// 与同步泵 [`CircCore::pump_input_sync`] 的两个关键区别：
    ///
    /// 1. **await 设备 future 而非 `block_on` 自旋**——真实设备（如 iroh
    ///    连接）可能长时间 `Pending`，自旋永远等不到数据；
    /// 2. **每次读取后立即提交**，不在段中滞留半截数据——若设备中途阻塞，
    ///    已读部分仍会提交并流向输出泵（`react_async` 的段内循环会把已读
    ///    半截数据滞留到设备再次就绪）。
    pub(super) async fn pipe_input_once(&self) -> usize {
        let state = self.atm_stat_.value();
        if has_flag(state, TX_CLOSED) || has_flag(state, RX_CLOSED) {
            return 0;
        }
        let cap = self.capacity();
        let pos = IoPos::unpack(state, cap);
        let free = pos.free_size();
        if free == 0 {
            return 0;
        }
        // 一段物理连续的可写区（跨末端时本轮先取一段，下一轮再取另一段）。
        let take = core::cmp::min(free, cap - pos.wp);
        // SAFETY: 流水线独占驱动（双主动无其他访问者）；区域不与活段重叠。
        let dst = &mut self.buffer_view_mut()[pos.wp..pos.wp + take];
        let producer = unsafe { &mut *self.producer_.get() };
        let x = producer
            .input_mut()
            .read_async(dst)
            .may_cancel_with(NonCancellableToken::shared_mut())
            .await;
        let n = x.pick_left().unwrap_or(0);
        if n == 0 {
            return 0; // 设备暂无数据 / 错误
        }
        self.advance_write(n);
        n
    }

    /// 流水线的一轮输出泵（异步）：把缓冲的一段**物理连续**可读区写**一次**
    /// 到输出设备（`await`，设备阻塞即挂起），随后立即提交（`advance_read`）。
    /// 返回本轮搬出字节数。语义同 [`CircCore::pipe_input_once`]。
    pub(super) async fn pipe_output_once(&self) -> usize {
        let state = self.atm_stat_.value();
        if has_flag(state, RX_CLOSED) {
            return 0;
        }
        let cap = self.capacity();
        let pos = IoPos::unpack(state, cap);
        let data = pos.data_size();
        if data == 0 {
            return 0;
        }
        let take = core::cmp::min(data, cap - pos.rp);
        let src = &self.buffer_view_mut()[pos.rp..pos.rp + take];
        let consumer = unsafe { &mut *self.consumer_.get() };
        let x = consumer
            .output_mut()
            .write_async(src)
            .may_cancel_with(NonCancellableToken::shared_mut())
            .await;
        let n = x.pick_left().unwrap_or(0);
        if n == 0 {
            return 0; // 设备暂不能接收 / 错误
        }
        self.advance_read(n);
        n
    }

}

// ---------------------------------------------------------------------------
// TrCircBuffCore（段提交接口）
// ---------------------------------------------------------------------------

impl<P, C, B, T> TrCircBuffCore for CircCore<P, C, B, T>
where
    P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
{
    type Data = T;

    fn advance_read(&self, amount: usize) {
        CircCore::advance_read(self, amount);
    }

    fn advance_write(&self, amount: usize) {
        CircCore::advance_write(self, amount);
    }

    fn try_write_init<'f>(
        &'f self,
    ) -> Option<impl TrBuffSegmMut<'f, T>> {
        // 借出全部可写区（min=0、max=∞ → take = free）。
        let demand = Demand::less_than(self.capacity());
        self.try_write_at(&demand)
            .ok()
            .map(|(start, take)| self.create_write_segm(start, take))
    }

    fn try_read_init<'f>(
        &'f self,
    ) -> Option<impl TrBuffSegmRef<'f, T>> {
        let demand = Demand::less_than(self.capacity());
        self.try_read_at(&demand)
            .ok()
            .map(|(start, take)| self.create_read_segm(start, take))
    }

    fn arm_producer(&self) {
        CircCore::set_tx_standby(self);
    }

    fn arm_consumer(&self) {
        CircCore::set_rx_standby(self);
    }
}

// ---------------------------------------------------------------------------
// 线程安全
// ---------------------------------------------------------------------------

// SAFETY: 核心的全部共享状态为原子（状态字、唤醒槽位、泵标志）。缓冲内存
// （`Owned` 的裸指针基址）与两端（`UnsafeCell`）只经内部可变性访问，其
// 正确性由模块文档的 SPSC 调用者义务 + `pumping` 互斥保证：
// * 至多一个生产线程、一个消费线程；
// * 泵只在其触发线程上执行（`drive()` 互斥）；
// * 活段与泵的操作不重叠。
unsafe impl<P, C, B, T> Send for CircCore<P, C, B, T>
where
    P: Send + TrProducer<Data = T>,
    C: Send + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send,
{}

unsafe impl<P, C, B, T> Sync for CircCore<P, C, B, T>
where
    P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync,
{}

// ---------------------------------------------------------------------------
// 唤醒槽位
// ---------------------------------------------------------------------------

/// 等待者：注册进唤醒槽位的 waker。活在等待 future（或被等待方）内部，
/// 注册期间不得移动，因此由槽位以裸指针引用。
pub struct Waiter {
    pub waker: Option<Waker>,
}

impl Waiter {
    pub const fn new() -> Self {
        Waiter { waker: None }
    }
}

/// 唤醒槽位：同一时刻至多一个等待者（SPSC 保证每侧至多一个等待的 future）。
///
/// 槽位由核心持有（`producer_wake_` / `consumer_wake_`），被动端专属；等待
/// future 以 `&WakeSlot` 注册 / 注销，提交路径 `signal` 唤醒。原子指针操作，
/// 无需锁。
pub struct WakeSlot(AtomicPtr<Waiter>);

impl WakeSlot {
    pub const fn new() -> Self {
        WakeSlot(AtomicPtr::new(ptr::null_mut()))
    }

    /// 把 `w` 注册为当前等待者。若仍有（陈旧的）等待者未注销则自旋等待其
    /// 注销（等待者在完成或 drop 时必然注销，因此自旋必然终止）。
    pub fn register(&self, w: &Waiter) {
        let p = w as *const Waiter as *mut Waiter;
        loop {
            match self.0.compare_exchange_weak(
                ptr::null_mut(),
                p,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(cur) if cur == p => return, // 已由我们注册
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// 若 `w` 仍是当前注册的等待者，则注销之。
    pub fn deregister(&self, w: &Waiter) {
        let p = w as *const Waiter as *mut Waiter;
        let _ = self
            .0
            .compare_exchange(p, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
    }

    /// 唤醒当前注册的等待者（若有），并清空槽位。
    pub fn signal(&self) {
        let p = self.0.swap(ptr::null_mut(), Ordering::AcqRel);
        if !p.is_null() {
            let w = unsafe { &*p };
            if let Some(waker) = w.waker.as_ref() {
                waker.wake_by_ref();
            }
        }
    }
}

/// 主动端 park 的守卫：保证 `park_producer` / `park_consumer` 在取消 / drop
/// 时注销 WakeSlot 并清 STNDBY。
struct ActiveParkGuard<'a, C> {
    slot: &'a WakeSlot,
    waiter: Waiter,
    core: &'a C,
    registered: bool,
    unarm: fn(&C),
}

impl<C> Drop for ActiveParkGuard<'_, C> {
    fn drop(&mut self) {
        if self.registered {
            self.slot.deregister(&self.waiter);
        }
        (self.unarm)(self.core);
    }
}

/// 被动等待（park）的守卫：持有注册进唤醒槽位的 [`Waiter`]，并保证等待
/// future 在**任何退出路径**上完成收尾。
///
/// 注册进槽位的 `Waiter` 由槽位以**裸指针**持有（`WakeSlot::register`），
/// 因此等待 future 在未完成时被 drop（取消 / 被对端抢占）也必须注销槽位，
/// 否则槽位会留下指向已销毁 `Waiter` 的悬垂指针——下一个等待者的
/// `WakeSlot::register` 会因此自旋（`register` 的文档：「等待者在完成或 drop
/// 时必然注销，因此自旋必然终止」）。同时，park 期间登记的 demand 也必须
/// 复位，否则下一次 `try_set_demand`（CAS null → 非空）会失败并触发
/// 「并发调用」断言。
///
/// 因此本守卫的 [`Drop`] 统一执行三件事：
///
/// 1. 若仍注册在槽位中，则注销（`unregister`）；
/// 2. 退出等待中（`unarm`，清 STNDBY 位）——armed 期间 fire 侧才会访问
///    demand / 槽位，必须与登记配对清除；
/// 3. 复位 demand（`reset_demand`）。
///
/// 正常完成路径上（需求满足 / 终止错误），守卫随 `poll_fn` future 在
/// `.await` 结束时被 drop，同样执行收尾——与取消路径共用同一份逻辑。
struct WaitGuard<'a, E, C> {
    /// 被等待的被动端（`BufProducer` / `BufConsumer`）。
    end: &'a E,
    /// 环形核心：退出等待中（清 STNDBY 位）需要它。
    core: &'a C,
    /// 注册进 `end.wakeslot()` 的等待者（槽位以裸指针引用它，注册期间不得
    /// 移动——它活在 `poll_fn` future 内，而该 future 被 async 状态机钉住）。
    waiter: Waiter,
    /// 当前是否已注册进槽位。
    registered: bool,
    /// 注销槽位：`end.wakeslot().deregister(&waiter)`。
    unregister: fn(&E, &Waiter),
    /// 复位需求：`end.try_reset_demand()`。
    reset_demand: fn(&E),
    /// 退出等待中：`core.unpark_consumer()` / `core.unpark_producer()`。
    unarm: fn(&C),
}

impl<E, C> Drop for WaitGuard<'_, E, C> {
    fn drop(&mut self) {
        if self.registered {
            (self.unregister)(self.end, &self.waiter);
        }
        (self.unarm)(self.core);
        (self.reset_demand)(self.end);
    }
}

use super::circ_buff_::{BufConsumer, BufProducer, DevConsumer, DevProducer};

/// 被动读等待：由 `Consumer::read_async` 调用，固定消费端为 `BufConsumer`。
///
/// # 调用上下文
/// - 先尝试直接读取；失败后给对端 `producer.pump_async(core)` 一次机会；
/// - 若对端是主动端，`pump_async` 会在这里 await 设备；若对端是被动端，
///   它返回 0，本函数进入普通被动 park；
/// - 被 `fire_consumer` 唤醒后回到循环，重新尝试读取。
#[gen_may_cancel_future(CorePassiveRead)]
async fn core_passive_read_async_<'f, P, B, T, C>(
    core: &'f CircCore<P, BufConsumer<T>, B, T>,
    demand: &'f Demand<usize>,
    cancel: &'f mut C,
) -> SomeOf<
    ReclSliceRef<'f, T, ReaderReclaim<'f, CircCore<P, BufConsumer<T>, B, T>>>,
    ConsumerError<usize>,
>
where
    P: Send + Sync + TrProducer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    C: TrCancellationToken + Clone,
{
    loop {
        let x = core.try_read_(demand);
        if x.contains_left() {
            return x;
        };
        let err = x.as_ref().pick_right().expect("");
        if err.err_tag().should_terminate() {
            return x;
        };

        // 给对端一次主动补数据的机会。被动端 pump_async 返回 0；
        // 主动端会在这里 await 设备，设备 Pending 时由设备自己的 waker 唤醒。
        {
            let producer = unsafe { &mut *core.producer_.get() };
            let moved = producer.pump_async(core).may_cancel_with(cancel).await;
            if moved > 0 {
                continue;
            }
        }

        // 普通被动 park：对端没有主动补入数据时，注册到 BufConsumer 的槽位，
        // 等待对端写入提交触发 fire_consumer。
        let consume = unsafe { &*core.consumer_.get() };
        if !consume.try_set_demand(demand) {
            unreachable!("Concurrent call `core_passive_read_async_`")
        }
        core.set_rx_standby();
        let mut guard = WaitGuard {
            end: consume,
            core,
            waiter: Waiter::new(),
            registered: false,
            unregister: |end, waiter| end.wakeslot().deregister(waiter),
            reset_demand: |end| {
                let _ = end.try_reset_demand();
            },
            unarm: |core| core.unset_rx_standby(),
        };
        let res = core::future::poll_fn(|cx| {
            let fn_can_stop = || match core.try_read_at(demand) {
                Ok(_) => true,
                Err(e) => e.err_tag().should_terminate(),
            };
            if fn_can_stop() {
                return Poll::Ready(());
            }
            guard.waiter.waker = Some(cx.waker().clone());
            guard.end.wakeslot().register(&guard.waiter);
            guard.registered = true;
            if fn_can_stop() {
                return Poll::Ready(());
            }
            Poll::Pending
        });
        // 守卫在此已被 drop：槽位注销、armed 清除、demand 复位。
        if res.ok_or(cancel.cancellation()).await.is_err() {
            return SomeOf::new_right(ConsumerError::Cancelled);
        }
        // 被唤醒后回到循环，重新检查 / 再次 pump_async。
    }
}

/// 被动写等待：由 `Producer::write_async` 调用，固定生产端为 `BufProducer`。
///
/// # 调用上下文
/// - 先尝试直接写入；失败后给对端 `consumer.pump_async(core)` 一次机会；
/// - 若对端是主动端，`pump_async` 会在这里 await 设备；若对端是被动端，
///   它返回 0，本函数进入普通被动 park；
/// - 被 `fire_producer` 唤醒后回到循环，重新尝试写入。
#[gen_may_cancel_future(CorePassiveWrite)]
async fn core_passive_write_async_<'f, K, B, T, C>(
    core: &'f CircCore<BufProducer<T>, K, B, T>,
    demand: &'f Demand<usize>,
    cancel: &'f mut C,
) -> SomeOf<
    ReclSliceMut<'f, T, WriterReclaim<'f, CircCore<BufProducer<T>, K, B, T>>>,
    ProducerError<usize>,
>
where
    K: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    C: TrCancellationToken + Clone,
{
    loop {
        let x = core.try_write_(demand);
        if x.contains_left() {
            return x;
        };
        let err = x.as_ref().pick_right().expect("");
        if err.err_tag().should_terminate() {
            return x;
        };

        // 给对端一次主动排空的机会。被动端 pump_async 返回 0；
        // 主动端会在这里 await 设备，设备 Pending 时由设备自己的 waker 唤醒。
        {
            let consumer = unsafe { &mut *core.consumer_.get() };
            let moved = consumer.pump_async(core).may_cancel_with(cancel).await;
            if moved > 0 {
                continue;
            }
        }

        // 普通被动 park：对端没有主动排空时，注册到 BufProducer 的槽位，
        // 等待对端读取提交触发 fire_producer。
        let producer = unsafe { &*core.producer_.get() };
        if !producer.try_set_demand(demand) {
            unreachable!("Concurrent call `core_passive_write_async_`")
        };
        core.set_tx_standby();
        let mut guard = WaitGuard {
            end: producer,
            core,
            waiter: Waiter::new(),
            registered: false,
            unregister: |end, waiter| end.wakeslot().deregister(waiter),
            reset_demand: |end| {
                let _ = end.try_reset_demand();
            },
            unarm: |core| core.unset_tx_standby(),
        };
        let res = core::future::poll_fn(|cx| {
            let can_stop = || match core.try_write_at(demand) {
                Ok(_) => true,
                Err(e) => e.err_tag().should_terminate(),
            };
            if can_stop() {
                return Poll::Ready(());
            }
            guard.waiter.waker = Some(cx.waker().clone());
            guard.end.wakeslot().register(&guard.waiter);
            guard.registered = true;
            if can_stop() {
                return Poll::Ready(());
            }
            Poll::Pending
        });
        // 守卫在此已被 drop：槽位注销、armed 清除、demand 复位。
        if res.ok_or(cancel.cancellation()).await.is_err() {
            return SomeOf::new_right(ProducerError::Cancelled);
        }
        // 被唤醒后回到循环，重新检查 / 再次 pump_async。
    }
}

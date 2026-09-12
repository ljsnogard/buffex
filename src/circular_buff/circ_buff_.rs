//! `CircularBuff` 的**端类型**：被动标记（[`BuffProducer`] / [`BuffConsumer`]）
//! 与主动设备端（[`DeviceProducer`] / [`DeviceConsumer`]）。
//!
//! 完整的设计与使用思路见 [`crate::circular_buff`] 的模块文档。
//!
//! 端类型存放在核心（`CircCore`）内部，实现
//! `TrProducer` / `TrConsumer`
//! 契约。它们**不对外暴露**——真实的访问入口是构建器产出的
//! `Producer` / `Consumer`
//! 半部（借用核心）。主动端（设备驱动）的半部操作返回错误（不对外访问）。

use core::{
    future::IntoFuture,
    marker::{PhantomData, PhantomPinned},
    pin::pin,
    ptr,
    sync::atomic::AtomicPtr,
};

use abs_buff::{
    Demand,
    buffer::{TrBuffSegmMut, TrBuffSegmRef, TrBuffSegmView},
    gen_may_cancel_future,
    io::{TrInput, TrOutput},
    x_deps::abs_cancel,
};
use abs_cancel::{TrCancellationToken, TrMayCancel};
use atomex::AtomexPtrOwned;
use atomic_sync::x_deps::atomex;

use super::{
    abs_comp_::{
        ConsumerHookEvent, ProducerHookEvent, ReceiverReact,
        TrCircBuffCore, TrConsumer, TrProducer,
    },
    core_::{WakeSlot, poll_once},
};

// ---------------------------------------------------------------------------
// 端类型
// ---------------------------------------------------------------------------

struct BuffObsv {
    demand_: AtomexPtrOwned<Demand<usize>>,
}

impl BuffObsv {
    pub const fn new() -> Self {
        BuffObsv {
            demand_: AtomexPtrOwned::new(AtomicPtr::new(ptr::null_mut())),
        }
    }

    /// 假定 Demand 指针为空，设置新的 Demand。返回设置是否成功。
    #[inline]
    pub fn try_set_demand(
        &self,
        demand: &Demand<usize>,
    ) -> bool {
        let p = demand as *const _ as *mut Demand<usize>;
        let p = unsafe { core::ptr::NonNull::new_unchecked(p) };
        self.demand_
            .try_spin_init(p)
            .is_ok()
    }

    /// 假定 Demand 指针非空，重新置空。返回此前存储的指针。
    #[inline]
    pub fn try_reset_demand(
        &self,
    ) -> Result<ptr::NonNull<Demand<usize>>, *mut Demand<usize>> {
        self.demand_.try_reset()
    }
}

/// 被动生产端的端类型：存放进核心的 `P` 参数。
///
/// 携带**等待者的需求**（`TrProducer::set_demand` 登记、`check` 裁决）：
/// 可写空间不足 `demand` 下限时**不唤醒**写者（避免 spurious wake；写者被
/// 唤醒后仍会重查条件）。唤醒槽位由核心持有（见 `core_` 的 `producer_wake_`）。
///
/// # 被动端的三态生命周期（STNDBY armed 协议，SPSC）
///
/// 在任意时刻，被动端处于以下三种状态之一：
///
/// 1. **无需求等待调用**（idle）：没有等待者登记需求（`demand` 为 `None`），
///    `TX_STNDBY` 为 0——**不接受 fire**（fire 直接返回）；
/// 2. **有需求正在登记**（arming）：等待者已写入 `demand`，但 armed 位尚未
///    置位——`TX_STNDBY` 仍为 0，**不接受 fire**（等待者注册后会重查条件，
///    不会丢唤醒）；
/// 3. **等待中**（armed / standing by）：`TX_STNDBY` 为 1，**接受 fire**——
///    fire 侧此时才访问 `demand` 判断兴趣（经状态字 Acquire 读与置位 CAS
///    建立 happens-before，见 `core_` 的 `fire_producer`），感兴趣则唤醒
///    等待者。
///
/// 状态迁移：等待者 park 时 `1 → 2 → 3`（写 demand → CAS 置 `TX_STNDBY` →
/// 注册槽位）；完成 / drop 时 `3 → 1`（注销槽位 → CAS 清 `TX_STNDBY` →
/// 清 demand）。因为 `demand` 只在态 3 被 fire 读取、写入先于 armed 置位、
/// 清除后于 armed 清位，它是**普通字段**（无需原子）。
pub struct BufProducer<T> {
    buf_obsv_: BuffObsv,
    wakeslot_: WakeSlot,
    _unuse_t_: PhantomData<fn() -> T>,
    _pin_buf_: PhantomPinned,
}

impl<T> BufProducer<T> {
    pub(super) const fn new() -> Self {
        BufProducer {
            buf_obsv_: BuffObsv::new(),
            wakeslot_: WakeSlot::new(),
            _unuse_t_: PhantomData,
            _pin_buf_: PhantomPinned,
        }
    }

    #[inline]
    pub(super) fn try_set_demand(&self, demand: &Demand<usize>) -> bool {
        self.buf_obsv_.try_set_demand(demand)
    }

    #[inline]
    pub(super) fn try_reset_demand(
        &self,
    ) -> Result<ptr::NonNull<Demand<usize>>, *mut Demand<usize>> {
        self.buf_obsv_.try_reset_demand()
    }

    /// 被动等待者注册 / 注销 waker 的唤醒槽位（`core_` 的等待 future 使用）。
    #[inline]
    pub(super) fn wakeslot(&self) -> &WakeSlot {
        &self.wakeslot_
    }
}

/// 被动消费端的端类型：与 [`BuffProducer`] 对称，存放进核心的 `C` 参数。
///
/// 三态生命周期同 [`BuffProducer`]（armed 位为 `RX_STNDBY`）：fire 仅在
/// **等待中**（态 3）访问 `demand` 判断兴趣。
pub struct BufConsumer<T> {
    buf_obsv_: BuffObsv,
    wakeslot_: WakeSlot,
    _unuse_t_: PhantomData<fn() -> T>,
    _pin_buf_: PhantomPinned,
}

impl<T> BufConsumer<T> {
    pub(super) const fn new() -> Self {
        BufConsumer {
            buf_obsv_: BuffObsv::new(),
            wakeslot_: WakeSlot::new(),
            _unuse_t_: PhantomData,
            _pin_buf_: PhantomPinned,
        }
    }

    #[inline]
    pub(super) fn try_set_demand(&self, demand: &Demand<usize>) -> bool {
        self.buf_obsv_.try_set_demand(demand)
    }

    #[inline]
    pub(super) fn try_reset_demand(
        &self,
    ) -> Result<ptr::NonNull<Demand<usize>>, *mut Demand<usize>> {
        self.buf_obsv_.try_reset_demand()
    }

    /// 被动等待者注册 / 注销 waker 的唤醒槽位（`core_` 的等待 future 使用）。
    #[inline]
    pub(super) fn wakeslot(&self) -> &WakeSlot {
        &self.wakeslot_
    }
}

/// 主动生产端的端类型：携带输入设备的**实际类型**（`TyInput`），随设备一同
/// 存放进核心。构造完成后由 executor 驱动的输入泵从设备提取数据填入缓冲——
/// 因此**不对外暴露**可访问的写半部（主动端的 `Producer` 半部操作返回错误）。
///
/// # 为什么是具体类型而不是类型擦除
///
/// 本类型直接持有 `input_: TyInput`（构建期把设备 move 进核心），核心因此
/// **无需任何类型擦除**即可在内部持有并驱动设备；`TrInput` 带泛型关联类型、
/// 不能直接 `dyn`，具体化是唯一可行路径。代价是核心必须泛型于端类型
/// （`CircCore<P, C, T>`），以及设备必须 `Send + Sync`（随核心跨线程）。
///
/// # 唤醒槽位（与 `BufProducer` 同构）
///
/// `wakeslot_: WakeSlot` 供 executor 驱动的输入泵 **park** 使用：缓冲满（无
/// 可写空间）且设备还有数据时，泵 armed（`TX_STNDBY`，经核心
/// `CircCore::arm_producer`）并把 waker 注册进本槽位，返回 `Pending`——消费端
/// 完成读取触发 `fire_producer` 时，经 armed 门控后 `check` 裁决并 `signal`
/// 唤醒泵，泵在下一次轮询中继续拉取。`init_async` 即「建立待唤醒结构」的
/// 环节：槽位随端类型一并就绪。
///
/// # 行为
///
/// `react_async`：循环地把 `TrInput` 的数据搬进传入的可写段（经 `abs_buff`
/// 的 `move_items_from_input_async`），直到段满或设备暂无数据。泵由被动端的
/// 异步等待 / `try_*` 重试路径驱动（`await` 设备 future，executor 驱动）。
pub struct DevProducer<TyInput, T>
where
    TyInput: TrInput<T>,
    T: 'static,
{
    input_: TyInput,
    wakeslot_: WakeSlot,
    _use_t: PhantomData<fn() -> T>,
    _pin_: PhantomPinned,
}

impl<TyInput, T> DevProducer<TyInput, T>
where
    TyInput: TrInput<T>,
    T: 'static,
{
    pub(super) fn new(input: TyInput) -> Self {
        DevProducer {
            input_: input,
            wakeslot_: WakeSlot::new(),
            _use_t: PhantomData,
            _pin_: PhantomPinned,
        }
    }

    /// 访问输入设备（双主动流水线 `pipe_async` 直接 await 设备的 `read_async`，
    /// 不经 `react_async`——避免段内滞留半截数据）。
    #[inline]
    pub(super) fn input_mut(&mut self) -> &mut TyInput {
        &mut self.input_
    }

    /// executor 驱动的输入泵 park 时注册 / 注销 waker 的唤醒槽位
    /// （`fire_producer` 经 armed 门控后 `check` 裁决并 `signal` 唤醒）。
    ///
    /// `#[allow(dead_code)]`：当前各驱动路径（构建 / `try_*` / 被动端 park）
    /// 的泵由设备 waker 驱动、不 park 在本槽位上；本槽位是「连续泵」park
    /// 的基础设施，端到端行为由 `tests_::pump_` 的协议测试覆盖。
    #[allow(dead_code)]
    #[inline]
    pub(super) fn wakeslot(&self) -> &WakeSlot {
        &self.wakeslot_
    }

    pub(super) fn init_async<'f, TyCore>(
        &'f mut self,
        core: &'f TyCore,
    ) -> DevProducerInitAsync<'f, TyInput, T, TyCore>
    where
        TyCore: TrCircBuffCore<Data = T>,
    {
        DevProducerInitAsync(self, core)
    }

    pub(super) fn react_async<'a, 'f, TySegm>(
        &'f mut self,
        segm_mut: &'f mut TySegm,
    ) -> DevProducerReactAsync<'a, 'f, TyInput, T, TySegm>
    where
        'a: 'f,
        TySegm: 'a + TrBuffSegmMut<'a, T>,
    {
        // 宏为 where-only 生命周期 'a 追加了 PhantomData 标记字段（位置型）。
        DevProducerReactAsync(self, segm_mut, PhantomData)
    }
}

/// 主动消费端的端类型：携带输出设备的**实际类型**（`TyOutput`），与
/// [`DeviceProducer`] 对称存放进核心的 `C` 参数，不对外暴露。唤醒槽位
/// （`wakeslot_: WakeSlot`，与 `BufConsumer` 同构）供 executor 驱动的输出泵
/// park 使用（armed `RX_STNDBY`，见 [`DeviceProducer`] 的文档）。
///
/// `react_async`：循环地把传入的可读段数据搬到 `TrOutput`（经
/// `move_items_to_output_async`），直到段空或设备暂时不能接收。
pub struct DevConsumer<TyOutput, T>
where
    TyOutput: TrOutput<T>,
    T: 'static,
{
    output_: TyOutput,
    wakeslot_: WakeSlot,
    _use_t_: PhantomData<fn() -> T>,
    _pin_: PhantomPinned,
}

impl<TyOutput, T> DevConsumer<TyOutput, T>
where
    TyOutput: TrOutput<T>,
    T: 'static,
{
    pub(super) fn new(output: TyOutput) -> Self {
        DevConsumer {
            output_: output,
            wakeslot_: WakeSlot::new(),
            _use_t_: PhantomData,
            _pin_: PhantomPinned,
        }
    }

    /// 访问输出设备（双主动流水线 `pipe_async` 直接 await 设备的 `write_async`）。
    #[inline]
    pub(super) fn output_mut(&mut self) -> &mut TyOutput {
        &mut self.output_
    }

    /// executor 驱动的输出泵 park 时注册 / 注销 waker 的唤醒槽位
    /// （`fire_consumer` 经 armed 门控后 `check` 裁决并 `signal` 唤醒）。
    ///
    /// `#[allow(dead_code)]`：同 [`DevProducer::wakeslot`]——「连续泵」park 的
    /// 基础设施，端到端行为由 `tests_::pump_` 的协议测试覆盖。
    #[allow(dead_code)]
    #[inline]
    pub(super) fn wakeslot(&self) -> &WakeSlot {
        &self.wakeslot_
    }

    #[allow(dead_code)] // trait impl 直接构造 DevConsumerInitAsync，未走本方法
    pub(super) fn init_async<'f, TyCore>(
        &'f mut self,
        core: &'f TyCore,
    ) -> DevConsumerInitAsync<'f, TyOutput, T, TyCore>
    where
        TyCore: TrCircBuffCore<Data = T>,
    {
        DevConsumerInitAsync(self, core)
    }

    pub(super) fn react_async<'a, 'f, TySegm>(
        &'f mut self,
        segm_ref: &'f mut TySegm,
    ) -> DevConsumerReactAsync<'a, 'f, TyOutput, T, TySegm>
    where
        'a: 'f,
        TySegm: 'a + TrBuffSegmRef<'a, T>,
        T: 'f,
    {
        DevConsumerReactAsync(self, segm_ref, PhantomData)
    }
}

// ---------------------------------------------------------------------------
// 端契约实现
// ---------------------------------------------------------------------------

impl<T> TrProducer for BufProducer<T>
where
    T: 'static,
{
    type Data = T;

    type InitAsync<'f, TyCore> = core::future::Ready<Result<(), ()>>
    where
        Self: 'f,
        TyCore: 'f + TrCircBuffCore<Data = Self::Data>;

    type ReactAsync<'a, 'f, TySegm> = core::future::Ready<ReceiverReact>
    where
        Self: 'f,
        TySegm: 'a + TrBuffSegmMut<'a, Self::Data>,
        'a: 'f;

    type PumpAsync<'f, TyCore> = core::future::Ready<usize>
    where
        Self: 'f,
        TyCore: 'f + TrCircBuffCore<Data = Self::Data>;

    #[inline]
    fn init_async<'f, TyCore>(
        &'f mut self,
        _core: &'f TyCore,
    ) -> Self::InitAsync<'f, TyCore>
    where
        TyCore: TrCircBuffCore<Data = Self::Data>,
    {
        core::future::ready(Result::Ok(()))
    }

    #[inline]
    fn pump_async<'f, TyCore>(
        &'f mut self,
        _core: &'f TyCore,
    ) -> Self::PumpAsync<'f, TyCore>
    where
        TyCore: TrCircBuffCore<Data = Self::Data>,
    {
        core::future::ready(0)
    }

    #[inline]
    fn is_passive(&self) -> bool {
        true
    }

    fn check(&self, event: ProducerHookEvent) -> bool {
        let ProducerHookEvent::Available(free) = event else {
            return true;
        };
        if free == 0 {
            return false;
        }
        let Option::Some(demand_ptr) = self.buf_obsv_.demand_.load() else {
            return false;
        };
        let demand = unsafe { demand_ptr.as_ref() };
        let min = demand.min().copied().unwrap_or(0);
        if free >= min {
            self.wakeslot_.signal();
            true
        } else {
            false
        }
    }

    #[inline]
    fn react_async<'a, 'f, TySegm>(
        &'f mut self,
        _segm: &'f mut TySegm,
    ) -> Self::ReactAsync<'a, 'f, TySegm>
    where
        'a: 'f,
        TySegm: 'a + TrBuffSegmMut<'a, T>,
    {
        // 被动端由调用者驱动，无反应。
        core::future::ready(ReceiverReact::Continue)
    }
}

impl<T> TrConsumer for BufConsumer<T>
where
    T: 'static,
{
    type Data = T;

    type InitAsync<'f, TyCore> = core::future::Ready<Result<(), ()>>
    where
        Self: 'f,
        TyCore: 'f + TrCircBuffCore<Data = Self::Data>;

    type ReactAsync<'a, 'f, TySegm> = core::future::Ready<ReceiverReact>
    where
        Self: 'f,
        TySegm: 'a + TrBuffSegmRef<'a, Self::Data>,
        'a: 'f;

    type PumpAsync<'f, TyCore> = core::future::Ready<usize>
    where
        Self: 'f,
        TyCore: 'f + TrCircBuffCore<Data = Self::Data>;

    #[inline]
    fn init_async<'f, TyCore>(
        &'f mut self,
        _core: &'f TyCore,
    ) -> Self::InitAsync<'f, TyCore>
    where
        TyCore: TrCircBuffCore<Data = Self::Data>
    {
        core::future::ready(Result::Ok(()))
    }

    #[inline]
    fn pump_async<'f, TyCore>(
        &'f mut self,
        _core: &'f TyCore,
    ) -> Self::PumpAsync<'f, TyCore>
    where
        TyCore: TrCircBuffCore<Data = Self::Data>,
    {
        core::future::ready(0)
    }

    #[inline]
    fn is_passive(&self) -> bool {
        true
    }

    fn check(&self, event: ConsumerHookEvent) -> bool {
        let ConsumerHookEvent::Available(ready) = event else {
            return true;
        };
        if ready == 0 {
            return false;
        }
        let Option::Some(demand_ptr) = self.buf_obsv_.demand_.load() else {
            return false;
        };
        let demand = unsafe { demand_ptr.as_ref() };
        let min = demand.min().copied().unwrap_or(0);
        if ready >= min {
            self.wakeslot_.signal();
            true
        } else {
            false
        }
    }

    #[inline]
    fn react_async<'a, 'f, TySegm>(
        &'f mut self,
        _segm: &'f mut TySegm,
    ) -> Self::ReactAsync<'a, 'f, TySegm>
    where
        'a: 'f,
        TySegm: 'a + TrBuffSegmRef<'a, T>,
    {
        core::future::ready(ReceiverReact::Continue)
    }
}

impl<TyInput, T> TrProducer for DevProducer<TyInput, T>
where
    TyInput: TrInput<T>,
    T: 'static,
{
    type Data = T;

    type InitAsync<'f, TyCore> = DevProducerInitAsync<'f, TyInput, T, TyCore>
    where
        Self: 'f,
        TyCore: 'f + TrCircBuffCore<Data = Self::Data>;

    type ReactAsync<'a, 'f, TySegm> = DevProducerReactAsync<'a, 'f, TyInput, T, TySegm>
    where
        Self: 'f,
        TySegm: 'a + TrBuffSegmMut<'a, Self::Data>,
        'a: 'f;

    type PumpAsync<'f, TyCore> = DevProducerPumpAsync<'f, TyInput, T, TyCore>
    where
        Self: 'f,
        TyCore: 'f + TrCircBuffCore<Data = Self::Data>;

    #[inline]
    fn init_async<'f, TyCore>(
        &'f mut self,
        core: &'f TyCore,
    ) -> DevProducerInitAsync<'f, TyInput, T, TyCore>
    where
        TyCore: TrCircBuffCore<Data = Self::Data>,
    {
        DevProducer::init_async(self, core)
    }

    #[inline]
    fn pump_async<'f, TyCore>(
        &'f mut self,
        core: &'f TyCore,
    ) -> DevProducerPumpAsync<'f, TyInput, T, TyCore>
    where
        TyCore: TrCircBuffCore<Data = Self::Data>,
    {
        DevProducerPumpAsync(self, core)
    }

    #[inline]
    fn wakeslot(&self) -> Option<&WakeSlot> {
        Some(&self.wakeslot_)
    }

    #[inline]
    fn is_passive(&self) -> bool {
        false
    }

    #[inline]
    fn check(&self, event: ProducerHookEvent) -> bool {
        // 有可写空间即值得唤醒（fire 侧已保证本方法只在 armed——泵 park 时
        // 调用）：signal 唤醒 executor 驱动的输入泵，泵重查状态后继续拉取。
        // 注意：fire 不再同步泵——「唤醒泵」是主动端唯一的响应。
        let interested =
            matches!(event, ProducerHookEvent::Available(size) if size > 0);
        if interested {
            self.wakeslot_.signal();
        }
        interested
    }

    fn react_async<'a, 'f, TySegm>(
        &'f mut self,
        segm: &'f mut TySegm,
    ) -> Self::ReactAsync<'a, 'f, TySegm>
    where
        'a: 'f,
        TySegm: 'a + TrBuffSegmMut<'a, T>,
    {
        DevProducer::react_async(self, segm)
    }
}

impl<O, T> TrConsumer for DevConsumer<O, T>
where
    O: TrOutput<T>,
    T: 'static,
{
    type Data = T;

    type InitAsync<'f, C> = DevConsumerInitAsync<'f, O, T, C>
    where
        Self: 'f,
        C: 'f + TrCircBuffCore<Data = Self::Data>;

    type ReactAsync<'a, 'f, S> = DevConsumerReactAsync<'a, 'f, O, T, S>
    where
        Self: 'f,
        S: 'a + TrBuffSegmRef<'a, Self::Data>,
        'a: 'f;

    type PumpAsync<'f, C> = DevConsumerPumpAsync<'f, O, T, C>
    where
        Self: 'f,
        C: 'f + TrCircBuffCore<Data = Self::Data>;

    #[inline]
    fn init_async<'f, C>(
        &'f mut self,
        core: &'f C,
    ) -> Self::InitAsync<'f, C>
    where
        C: TrCircBuffCore<Data = Self::Data>,
    {
        DevConsumerInitAsync(self, core)
    }

    #[inline]
    fn pump_async<'f, C>(
        &'f mut self,
        core: &'f C,
    ) -> DevConsumerPumpAsync<'f, O, T, C>
    where
        C: TrCircBuffCore<Data = Self::Data>,
    {
        DevConsumerPumpAsync(self, core)
    }

    #[inline]
    fn wakeslot(&self) -> Option<&WakeSlot> {
        Some(&self.wakeslot_)
    }

    #[inline]
    fn is_passive(&self) -> bool {
        false
    }

    #[inline]
    fn check(&self, event: ConsumerHookEvent) -> bool {
        // 有数据即值得唤醒；**ProducerClose 也感兴趣**——写端关闭后必须把
        // 残留数据排空（executor 驱动的输出泵被唤醒后搬到空为止）。fire 侧
        // 已保证本方法只在 armed（泵 park）时调用；signal 唤醒泵。
        let interested = matches!(
            event,
            ConsumerHookEvent::Available(size) if size > 0
        ) || matches!(event, ConsumerHookEvent::ProducerClose(_));
        if interested {
            self.wakeslot_.signal();
        }
        interested
    }

    #[inline]
    fn react_async<'a, 'f, S>(
        &'f mut self,
        segm_ref: &'f mut S,
    ) -> Self::ReactAsync<'a, 'f, S>
    where
        'a: 'f,
        S: 'a + TrBuffSegmRef<'a, T>,
    {
        DevConsumer::react_async(self, segm_ref)
    }
}

#[gen_may_cancel_future(DevProducerInit)]
async fn dev_producer_init_async_<'f, I, T, C, K>(
    producer: &'f mut DevProducer<I, T>,
    core_ref: &'f C,
    cancel: &'f mut K,
) -> Result<(), ()>
where
    I: TrInput<T>,
    T: 'static,
    C: TrCircBuffCore<Data = T>,
    K: TrCancellationToken + Clone,
{
    if let Some(mut segm_mut) = core_ref.try_write_init() {
        // 构建期只做一次非阻塞探测：设备 Pending 就放弃本轮，不等设备。
        let may_fut = producer
            .react_async(&mut segm_mut)
            .may_cancel_with(cancel);
        let mut fut = pin!(may_fut.into_future());
        let _ = poll_once(fut.as_mut());
        // segm_mut 在这里 drop；若已搬入数据，drop 会提交 advance_write。
    }
    core_ref.arm_producer();
    Result::Ok(())
}

#[gen_may_cancel_future(DevProducerPump)]
async fn dev_producer_pump_async_<'f, I, T, C, K>(
    producer: &'f mut DevProducer<I, T>,
    core_ref: &'f C,
    cancel: &'f mut K,
) -> usize
where
    I: TrInput<T>,
    T: 'static,
    C: TrCircBuffCore<Data = T>,
    K: TrCancellationToken + Clone,
{
    let mut total = 0usize;
    while let Some(mut segm_mut) = core_ref.try_write_init() {
        let before = segm_mut.least_count();
        let r = producer
            .react_async(&mut segm_mut)
            .may_cancel_with(cancel)
            .await;
        let moved = before - segm_mut.least_count();
        drop(segm_mut); // 提交，触发 fire_consumer
        total += moved;
        if moved == 0 || r == ReceiverReact::Continue {
            break;
        }
    }
    total
}

#[gen_may_cancel_future(DevProducerReact)]
async fn dev_producer_react_async_<'a, 'f, I, T, S, K>(
    producer: &'f mut DevProducer<I, T>,
    segm_mut: &'f mut S,
    cancel: &'f mut K,
) -> ReceiverReact
where
    'a: 'f,
    I: TrInput<T>,
    T: 'static,
    S: 'a + TrBuffSegmMut<'a, T>,
    K: TrCancellationToken + Clone,
{
    let mut moved = 0usize;
    while !segm_mut.is_empty() {
        let demand = Demand::less_than(segm_mut.least_count());
        let x = segm_mut
            .as_segm_mut()
            .move_items_from_input_async(&mut producer.input_, &demand)
            .may_cancel_with(cancel)
            .await;
        // 设备错误：本轮视为无数据。
        if x.as_ref().pick_right().is_some() {
            break;
        }
        let n = x.pick_left().unwrap_or(0);
        if n == 0 {
            break; // 设备暂无数据
        }
        moved += n;
    }
    if moved > 0 {
        ReceiverReact::Reacted
    } else {
        ReceiverReact::Continue
    }
}

#[gen_may_cancel_future(DevConsumerInit)]
async fn dev_consumer_init_async_<'f, O, T, C, K>(
    consumer: &'f mut DevConsumer<O, T>,
    core_ref: &'f C,
    cancel: &'f mut K,
) -> Result<(), ()>
where
    O: TrOutput<T>,
    T: 'static,
    C: TrCircBuffCore<Data = T>,
    K: TrCancellationToken + Clone,
{
    if let Some(mut segm_ref) = core_ref.try_read_init() {
        // 构建期只做一次非阻塞探测：设备 Pending 就放弃本轮，不等设备。
        let may_fut = consumer
            .react_async(&mut segm_ref)
            .may_cancel_with(cancel);
        let mut fut = pin!(may_fut.into_future());
        let _ = poll_once(fut.as_mut());
        // segm_ref 在这里 drop；若已搬出数据，drop 会提交 advance_read。
    }
    core_ref.arm_consumer();
    Result::Ok(())
}

#[gen_may_cancel_future(DevConsumerPump)]
async fn dev_consumer_pump_async_<'f, O, T, C, K>(
    consumer: &'f mut DevConsumer<O, T>,
    core_ref: &'f C,
    cancel: &'f mut K,
) -> usize
where
    O: TrOutput<T>,
    T: 'static,
    C: TrCircBuffCore<Data = T>,
    K: TrCancellationToken + Clone,
{
    let mut total = 0usize;
    while let Some(mut segm_ref) = core_ref.try_read_init() {
        let before = segm_ref.least_count();
        let r = consumer
            .react_async(&mut segm_ref)
            .may_cancel_with(cancel)
            .await;
        let moved = before - segm_ref.least_count();
        drop(segm_ref); // 提交，触发 fire_producer
        total += moved;
        if moved == 0 || r == ReceiverReact::Continue {
            break;
        }
    }
    total
}

#[gen_may_cancel_future(DevConsumerReact)]
async fn dev_consumer_react_async_<'a, 'f, O, T, S, K>(
    consumer: &'f mut DevConsumer<O, T>,
    segm_ref: &'f mut S,
    cancel: &'f mut K,
) -> ReceiverReact
where
    'a: 'f,
    O: TrOutput<T>,
    T: 'static,
    S: 'a + TrBuffSegmRef<'a, T>,
    K: TrCancellationToken + Clone,
{
    let mut moved = 0usize;
    while !segm_ref.is_empty() {
        let demand = Demand::less_than(segm_ref.least_count());
        let x = segm_ref
            .as_segm_ref()
            .move_items_to_output_async(&mut consumer.output_, &demand)
            .may_cancel_with(cancel)
            .await;
        // 设备错误：本轮视为不能接收。
        if x.as_ref().pick_right().is_some() {
            break;
        }
        let n = x.pick_left().unwrap_or(0);
        if n == 0 {
            break; // 设备暂时不能接收
        }
        moved += n;
    }
    if moved > 0 {
        ReceiverReact::Reacted
    } else {
        ReceiverReact::Continue
    }
}
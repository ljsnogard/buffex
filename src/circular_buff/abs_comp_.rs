//! 环形缓冲的抽象组件：核心提交接口（[`TrCircBuffCore`]）、端类型契约
//! （[`TrProducer`] / [`TrConsumer`]）、事件（[`ProducerHookEvent`] /
//! [`ConsumerHookEvent`]）。
//!
//! # 本模块是**内部实现细节**，不对外暴露
//!
//! 本模块位于私有模块（`mod abs_comp_;`）中，其中的 trait 与事件**全部是
//! 环形核心内部使用的契约**：它们被 `CircCore`（泵、事件分发、`react_async`
//! 驱动）与端类型（`circ_buff_`）之间的内部协作所依赖，**不是**给调用者实现
//! 或调用的公开 API。调用者只与 [`super::builder`] 产出的
//! [`Producer`](super::spsc_::Producer) / [`Consumer`](super::spsc_::Consumer)
//! 半部交互。
//!
//! 这里把 trait 声明为（名义上的）`pub`，仅仅是为了满足 Rust 的「公开接口不
//! 能引用更低可见性类型」检查（私有模块已使这些名字对外不可达，名义可见性
//! 不影响实际的隐藏效果）；模块私有才是真正的隐藏手段。
//!
//! 这些 trait 把「环形核心」与「两端」解耦成两层：
//!
//! * [`TrCircBuffCore`] 是**段提交接口**——两段式段（[`super::reclaim_`] 的
//!   `ReclSliceMut` / `ReclSliceRef`）在 drop 时经 `WriterReclaim` /
//!   `ReaderReclaim` 调用它推进读写位置。段层因此只依赖这个窄接口；
//! * [`TrProducer`] / [`TrConsumer`] 是**端契约**——由存放在核心里的端类型
//!   （被动：`BuffProducer` / `BuffConsumer`；
//!   主动：`DeviceProducer` / `DeviceConsumer`）
//!   实现。核心在提交路径上向对端触发事件（fire，**只唤醒不搬运**）；主动端
//!   由 executor 驱动的泵经 `react_async` 拿到一段缓冲区视图完成数据搬运。
//!
//! # 为什么 `react_async` 泛化段参数（而不是关联类型）
//!
//! 若端契约携带 `Buffer<'f>` 关联类型（段），段在 drop 时要提交回核心，就
//! **必须指名核心类型**；而核心又泛型于端类型（`CircCore<P, C, T>`），于是
//! `P::Buffer<'f> = ReclSliceMut<…, WriterReclaim<…, CircCore<Self, C, T>>>`
//! 形成无基例的类型方程——这正是重构前「泛型参数无法稳定」的根源。
//!
//! 解法：端类型**不携带**段类型，`react_async` 的段参数由调用方（核心）按
//! 具体类型传入。段因此可以指名 `CircCore<P, C, T>`（段不在端类型内部，无环）。

use abs_buff::{
    buffer::{TrBuffSegmMut, TrBuffSegmRef},
    x_deps::abs_cancel,
};
use abs_cancel::TrMayCancel;

use super::core_::WakeSlot;

/// 环形核心的「段提交 + 泵协作」接口：段 drop 时按已消费量推进读写位置；
/// 主动端（`DevProducer` / `DevConsumer`）经本接口在 `init_async` 里完成初始
/// 搬运与 armed 登记。
///
/// 这是 [`super::reclaim_`] 的 `WriterReclaim` / `ReaderReclaim` 唯一依赖的
/// 接口——段层通过它把消费量提交回环形，而无需指名核心的具体类型。**对端
/// hook 的触发由核心自身在推进位置时完成**（`advance_write` 后触发消费端
/// 事件、`advance_read` 后触发生产端事件），对段层完全透明。
///
/// `Send + Sync` 是该接口的硬性要求：段可能被搬运到其他线程（`abs_buff`
/// 管道），提交路径必须线程安全。
pub trait TrCircBuffCore
where
    Self: Send + Sync,
{
    type Data;

    fn advance_read(&self, amount: usize);

    fn advance_write(&self, amount: usize);

    // ------------------------------------------------------------------
    // 主动端（Dev ends）的泵协作：`init_async` 的初始搬运 + armed 登记
    // ------------------------------------------------------------------

    fn try_write_init<'f>(
        &'f self,
    ) -> Option<impl 'f + TrBuffSegmMut<'f, Self::Data>>;

    fn try_read_init<'f>(
        &'f self,
    ) -> Option<impl 'f + TrBuffSegmRef<'f, Self::Data>>;

    // 主动端初始化后进入“等待被 fire 唤醒”的 armed 状态。
    // 默认 no-op，CircCore 会实现为设置 STNDBY 位。
    fn arm_producer(&self) {}

    fn arm_consumer(&self) {}
}

/// 生产端 hook 收到的事件（消费端完成读取 / 消费者关闭后触发）。
///
/// 由核心在 `advance_read` / `close_rx` 的提交路径上发给生产端 `P`。携带的
/// 新可写容量只是**参考值**：被动端唤醒等待者后由等待者重新检查条件（防丢失
/// 唤醒）；主动端据此决定是否继续泵设备。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerHookEvent {
    /// 缓冲区中的可供生产的数据有变化，携带新的可写容量
    Available(usize),

    /// 消费者已关闭，携带剩余可写容量
    ConsumerClose(usize),
}

/// 消费端 hook 收到的事件（生产端完成写入 / 生产者关闭后触发）。
///
/// 由核心在 `advance_write` / `close_tx` 的提交路径上发给消费端 `C`。语义与
/// [`ProducerHookEvent`] 对称。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerHookEvent {
    /// 缓冲区中的可供消费的数据有变化，携带新的可读数据量
    Available(usize),

    /// 生产者端已关闭，携带剩余可读数据量
    ProducerClose(usize),
}

/// 端类型对 `react_async` 的反应结果：告诉核心「这次反应是否推进了数据」。
///
/// * [`ReceiverReact::Reacted`]——本端确实消费 / 生产了数据（例如主动泵搬了
///   一批数据）。核心的泵循环据此继续驱动；
/// * [`ReceiverReact::Continue`]——本次无事可做（被动端、或设备暂时无数据 /
///   无法接收），泵循环停止本轮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverReact {
    /// Receiver has reacted upon the given buffer
    Reacted,

    /// Receiver
    Continue,
}

/// 消费端（读侧）契约：由存放在核心中的**端类型**实现。
///
/// # 角色
///
/// 核心 `CircCore<P, C, T>` 持有 `C: TrConsumer`。生产端完成写入 / 关闭后，
/// 核心向 `C` 触发一个 [`ConsumerHookEvent`] 并**先问 [`TrConsumer::check`]
/// 是否感兴趣**——感兴趣才行动：被动端唤醒等待者（重查条件），主动端经
/// [`TrConsumer::react_async`] 把缓冲数据搬到输出设备。
///
/// # 两种实现
///
/// * **被动**（`BuffConsumer`）：`is_passive` 为 `true`，`check` 按
///   [`TrConsumer::set_demand`] 登记的完整需求（`demand.min()`）裁决——不足
///   下限不唤醒，避免 spurious wake；关闭事件例外——EOF 总是唤醒；等待者被
///   唤醒后仍重查条件（见 `spsc_` 的 `Park`）；
/// * **主动**（`DeviceConsumer`）：`is_passive` 为 `false`，`check` 由设备
///   裁决（有数据 / 关闭排空等），核心在事件分发与泵循环中都会调用它。
pub trait TrConsumer {
    type Data;

    type InitAsync<'f, C>: TrMayCancel<'f, MayCancelOutput = Result<(), ()>>
    where
        Self: 'f,
        C: 'f + TrCircBuffCore<Data = Self::Data>;

    type ReactAsync<'a, 'f, S>:
        TrMayCancel<'f, MayCancelOutput = ReceiverReact>
    where
        Self: 'f,
        S: 'a + TrBuffSegmRef<'a, Self::Data>,
        'a: 'f;

    type PumpAsync<'f, C>: TrMayCancel<'f, MayCancelOutput = usize>
    where
        Self: 'f,
        C: 'f + TrCircBuffCore<Data = Self::Data>;

    /// 由“对端”调用的异步泵。被动端返回 Ready(0)，主动端实现真正的设备搬运。
    fn pump_async<'f, C>(
        &'f mut self,
        core: &'f C,
    ) -> Self::PumpAsync<'f, C>
    where
        C: TrCircBuffCore<Data = Self::Data>;

    /// 主动端返回自己的唤醒槽位；被动端默认 None。
    fn wakeslot(&self) -> Option<&WakeSlot> {
        None
    }

    /// 环形缓冲完成构建前，在 builder 中调用且仅调用一次的方法，用于 Consumer
    /// 自身的异步初始化。
    fn init_async<'f, C>(
        &'f mut self,
        core: &'f C,
    ) -> Self::InitAsync<'f, C>
    where
        C: TrCircBuffCore<Data = Self::Data>;

    fn is_passive(&self) -> bool;

    /// 本端对 `event`（携带当前数据量）是否感兴趣。
    ///
    /// 核心在**事件分发**（fire）与**泵的驱动路径**调用本方法：返回 `false`
    /// 则不唤醒 / 不继续泵。被动端按 [`TrConsumer::set_demand`] 登记的等待者
    /// 完整需求（`demand.min()`）裁决——不足下限不唤醒，避免 spurious wake；
    /// 关闭事件例外——EOF 总是值得唤醒；主动端由设备裁决。`&mut self`——端
    /// 类型可在裁决时更新内部状态。
    fn check(&self, event: ConsumerHookEvent) -> bool;

    /// 对事件作出反应：把 `segm` 中的可读数据搬给本端（设备）。
    ///
    /// `TySegm` 泛型化——端类型**不携带**段类型，避免端类型指名核心类型造成
    /// 的类型级循环（见模块文档）。核心以具体段类型（两段式
    /// `ReclSliceRef`）调用本方法；返回的 future 由泵 `await`（executor 驱动）
    /// 或非阻塞单次 poll（同步上下文）驱动。
    fn react_async<'a, 'f, S>(
        &'f mut self,
        segm_ref: &'f mut S,
    ) -> Self::ReactAsync<'a, 'f, S>
    where
        'a: 'f,
        S: 'a + TrBuffSegmRef<'a, Self::Data>,
        Self: 'f;
}

/// 生产端（写侧）契约：由存放在核心中的**端类型**实现。与 [`TrConsumer`]
/// 对称（详见其文档）；`react_async` 把输入设备的数据搬进 `segm`。
pub trait TrProducer {
    type Data;

    type InitAsync<'f, C>: TrMayCancel<'f, MayCancelOutput = Result<(), ()>>
    where
        Self: 'f,
        C: 'f + TrCircBuffCore<Data = Self::Data>;

    type ReactAsync<'a, 'f, S>:
        TrMayCancel<'f, MayCancelOutput = ReceiverReact>
    where
        Self: 'f,
        S: 'a + TrBuffSegmMut<'a, Self::Data>,
        'a: 'f;

    type PumpAsync<'f, C>: TrMayCancel<'f, MayCancelOutput = usize>
    where
        Self: 'f,
        C: 'f + TrCircBuffCore<Data = Self::Data>;

    /// 由“对端”调用的异步泵。被动端返回 Ready(0)，主动端实现真正的设备搬运。
    fn pump_async<'f, C>(
        &'f mut self,
        core: &'f C,
    ) -> Self::PumpAsync<'f, C>
    where
        C: TrCircBuffCore<Data = Self::Data>;

    /// 主动端返回自己的唤醒槽位；被动端默认 None。
    fn wakeslot(&self) -> Option<&WakeSlot> {
        None
    }

    fn init_async<'f, S>(
        &'f mut self,
        core: &'f S,
    ) -> Self::InitAsync<'f, S>
    where
        S: TrCircBuffCore<Data = Self::Data>;

    fn is_passive(&self) -> bool;

    /// 本端对 `event`（携带当前可写量）是否感兴趣。语义同
    /// [`TrConsumer::check`]（被动端按 [`TrProducer::set_demand`] 登记的完整
    /// 需求裁决）。
    fn check(&self, event: ProducerHookEvent) -> bool;

    fn react_async<'a, 'f, S>(
        &'f mut self,
        segm_mut: &'f mut S,
    ) -> Self::ReactAsync<'a, 'f, S>
    where
        'a: 'f,
        S: 'a + TrBuffSegmMut<'a, Self::Data>,
        Self: 'f;
}

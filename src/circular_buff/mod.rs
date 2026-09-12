//! # `CircularBuff` —— 构造期定「模式与用途」的唤醒式环形缓冲器
//!
//! `CircularBuff` 是 [`crate::ring_buffer`] 之外、在同一 `crates/buffex` 下新增的
//! 一个环形缓冲器实现。它与旧的 `RingBuffer` 并行存在、互不干扰，避免破坏既有
//! 代码。
//!
//! 它与 `RingBuffer` 最大的不同在于：**在构建期间就决定好生产端与消费端各自的
//! 工作模式（主动 / 被动）与用途（数据从哪里来、到哪里去）**，之后通过统一的
//! **hook** 机制驱动两端联动，因此无需任何异步运行时（`buffex` 是 `no_std` 的，
//! 主动模式不允许 `spawn` 任何异步任务）。
//!
//! # 模式：被动 vs 主动
//!
//! 生产端与消费端各自独立地从两种模式中选择一种：
//!
//! ## 被动模式（passive）——调用者驱动
//!
//! * **被动生产**：对外提供 `TrBuffTryWrite` 接口（`abs_buff`），调用者自行决定
//!   什么时候把数据写入内部缓冲。
//! * **被动消费**：对外提供 `TrBuffTryRead` 接口（`abs_buff`），调用者异步等待
//!   「内部缓冲区就绪」的 future / parker，就绪后自行读取。
//!
//! 被动端的「等待」就是普通异步等待：等待方把自己注册进核心的 waker 槽位
//! （parker），进入 pending 状态；对端操作触发 hook 把它唤醒，等待方重新检查
//! 条件（必要时再次注册），循环直至满足。
//!
//! ## 主动模式（active）——设备驱动
//!
//! * **主动生产**：构建时接收一个 `TrInput`（输入设备）实例。构造期
//!   （`init_async`）即把 `TrInput` 现有数据灌满缓冲；此后消费端每次读取提交
//!   （`advance_read`）驱动主动生产者**重复拉取**补位。
//! * **主动消费**：构建时接收一个 `TrOutput`（输出设备）实例。生产端每次写入
//!   提交（`advance_write`）驱动主动消费者**重复推送**到输出设备（关闭写端时
//!   排空残留）。
//!
//! 主动端的「搬运」**不自旋、不 spawn**：泵逻辑在主动端的 `react_async` 中
//! （被动端的 `react_async` 是 no-op），由提交路径 / `try_*` 重试 / 关闭排空
//! 驱动，只做**非阻塞单次 poll**（`Pending` 即放弃，见下文「主动模式的驱动」）。
//!
//! # 统一机制：hook（提交路径驱动对端泵 + fire 唤醒）
//!
//! 两种模式共享**同一套核心实现**：一个环形状态机（rp / wp / 容量），外加
//! **两个端类型**（各自携带唤醒槽位）。状态提交后核心做两件事：
//!
//! * **驱动对端泵**：`advance_read`（消费端读取提交）驱动生产端泵重复拉取补位、
//!   `advance_write`（生产端写入提交）驱动消费端泵重复推送排空——泵逻辑在
//!   主动端的 `react_async`（被动端 no-op），**无需判断对端类型**；
//! * **fire 唤醒**：经 **STNDBY armed 协议**门控（`TX_STNDBY` / `RX_STNDBY`）
//!   唤醒注册在端类型唤醒槽位（[`WakeSlot`]）中的等待者 / 泵——被动端等待者
//!   park 时 armed 并注册 waker，`check` 按登记的需求下限裁决；主动端在
//!   `init_async` 时 armed 并注册到自身槽位，供 executor 驱动的泵（如
//!   `Pipeline`）在等待缓冲状态时被唤醒。
//!
//! ```text
//! 生产端完成写入（wp 前进，产生可读数据）  →  驱动【消费端泵】+ fire【消费端】
//!     消费端被动：唤醒等待可读数据的读者（pending future 的 waker）
//!     消费端主动：驱动重复推送到 TrOutput（react_async）
//!
//! 消费端完成读取（rp 前进，释放可写空间）  →  驱动【生产端泵】+ fire【生产端】
//!     生产端被动：唤醒等待可写空间的写者（pending future 的 waker）
//!     生产端主动：驱动重复从 TrInput 拉取补位（react_async）
//! ```
//!
//! ## 设计要点一：hook 对调用者透明
//!
//! 核心内部使用哪个端（被动=唤醒等待者，主动=唤醒泵）是构建期由 [`builder`]
//! 决定并隐藏的：`CircCore<P, C, T, A>` 的泛型参数 `P` / `C` 是**端类型**
//! （决定可访问性），`T` 是元素类型，`A` 是分配器——hook 完全在内部，调用者
//! 不参与构造。
//!
//! ## 设计要点二：主动端不产出半部
//!
//! 模式对调用者唯一可见的影响是**可访问性**：使用了主动模式的那一端由设备驱动，
//! 调用者**根本拿不到它的半部**——`build_async` 只把**被动端**的半部交给调用者
//! （见 [`BuildOutcome`](builder::BuildOutcome)）：被动 × 被动 → 一对半部；
//! 主动生产 × 被动消费 → 仅消费端半部；被动生产 × 主动消费 → 仅生产端半部；
//! 主动 × 主动 → [`Pipeline`]（流水线 future，无半部）。这对应了
//! `RingBuffer` 中「半部不存在」（
//! [`RingBuffer::try_split_io`](crate::ring_buffer::TrRingBuffer::try_split_io)
//! 返回 `None`）的情形——主动端连「返回错误的空半部」都没有，从类型层面
//! 杜绝调用者持有主动端。
//!
//! 主动端的**端类型**（[`DeviceProducer`] / [`DeviceConsumer`]）携带设备的实际
//! 类型并随设备一同存放进核心——因此**无需任何类型擦除**（`TrInput` 带泛型
//! 关联类型、不能直接 `dyn`）即可在内部持有并驱动设备。
//!
//! ## 设计要点三：缓冲归核心所有（统一 `[MaybeUninit<T>]` 视图）
//!
//! 与 `RingBuffer` 的 `RingStorage` 抽象不同，这里不引入存储抽象层：缓冲由
//! 核心**拥有**（[`mm_ptr::Owned`]，分配器 `A` 默认 `CoreAlloc`），内部一律以
//! `[MaybeUninit<T>]` 视图操作（`T` 默认 `u8`）。`no_std` 下 `alloc` 是 stable
//! crate，`TrMalloc` 抽象用于避免 `allocator_api` nightly 特性。
//!
//! # 主动模式的驱动（不自旋、不 spawn，操作驱动）
//!
//! 泵与设备的交互**均不自旋**（旧设计的 `Waker::noop()` 自旋驱动已被弃用）：
//!
//! * **初始搬运**（`DevProducer::init_async`，构建期）：对设备 future 做
//!   **非阻塞尝试**——单次 `poll`，设备就绪即完成；`Pending`（设备需外部
//!   唤醒 / 其它执行体推进）即放弃，**不自旋**；
//! * **提交路径驱动**（`advance_read` / `advance_write`）：消费端读取 /
//!   生产端写入提交后，驱动对端泵**重复**拉取 / 推送（每轮非阻塞单次 poll）；
//! * **操作重试**（`try_*` 的 `Stuffed` / `Drained`）与关闭排空（`close_tx`）：
//!   同样经非阻塞单次 poll 驱动对端泵；
//! * **被动等待**（`read_async` / `write_async` 的 park）：纯等待，不驱动泵——
//!   数据由上述路径先于等待而存在 / 补位（被动端只关心自己的需求）。
//!
//! 因此「搬运」由操作 / 提交路径推进，**不依赖任何自驱动循环或 spawn**。
//!
//! # 全主动流水线（TrInput → 缓冲 → TrOutput）
//!
//! 两端都主动时，数据流由 [`Pipeline`]（一个 **Future**）自动贯通：交给异步
//! 运行时 `spawn` 后，**只要它存活，数据就持续由两端设备驱动流动**——泵循环
//! await 设备的 `read_async` / `write_async`，设备就绪即流动、阻塞即挂起等待，
//! 直到一端出错 / 关闭、或调用者请求断开。若不想持有 `Pipeline` future，请让
//! 至少一端保持被动：被动端的读写会驱动对端的主动泵（`try_*` 重试 / 异步等待
//! 的 park）。
//!
//! # 构建期决策（builder）
//!
//! 模式与用途的决策全部收敛在构建器里，见 [`builder`]。**两端可以任意顺序
//! 设置**——既可以从生产端开始，也可以从消费端开始，还可以一步同时设置两端，
//! 或两端都不设置（默认双端被动）：
//!
//! ```ignore
//! // 双端被动（默认，不 pipe 任何设备）：经典手动管道（两端都可访问）。
//! // `build` 现为异步（`build_async`）：主动端在构建期完成异步初始化，
//! // 需在异步上下文中 await（同步场景可用 `block_on` 等执行器驱动）。
//! let (mut tx, mut rx) = CircularBuffBuilder::with_capacity(4096)?
//!     .build_async().await?;
//!
//! // 显式双端被动：`producer_passive` / `consumer_passive` 可任意换序
//! let (mut tx, mut rx) = CircularBuffBuilder::with_capacity(4096)?
//!     .producer_passive()
//!     .consumer_passive()
//!     .build_async().await?;
//!
//! // 主动生产 × 被动消费：从 TrInput 自动灌入，用户自行读取
//! // （主动生产端不产出半部——只拿到消费端）
//! let mut ready = CircularBuffBuilder::with_capacity(4096)?
//!     .pipe_from_input(input)      // 生产端先行
//!     .consumer_passive();
//! let mut rx = ready.build_async().await?;
//!
//! // 被动生产 × 主动消费：用户自行写入，写后自动搬运到 TrOutput
//! // （消费端先行、生产端后设——顺序与上例对调；主动消费端不产出半部）
//! let mut ready = CircularBuffBuilder::with_capacity(4096)?
//!     .pipe_into_output(output)    // 消费端先行
//!     .producer_passive();         // 生产端后设
//! let mut tx = ready.build_async().await?;
//!
//! // 主动 × 主动：TrInput → 缓冲 → TrOutput 流水线——`build_async` 返回
//! // [`Pipeline`]（Future）：交给运行时 spawn 后持续由设备驱动流动。
//! let mut ready = CircularBuffBuilder::with_capacity(4096)?
//!     .pipe_into_output(output)    // 消费端先行
//!     .pipe_from_input(input);     // 生产端后设
//! let pipeline = ready.build_async().await?;
//!
//! // 一步同时设置两端（等价于上例）
//! let mut ready = CircularBuffBuilder::with_capacity(4096)?
//!     .pipe_between(input, output);
//! let pipeline = ready.build_async().await?;
//! ```
//!
//! 构建器用类型状态（type-state）编码强制「两端模式必须在构建期决定」：
//! `CircularBuffBuilder`（仅容量）→ 任一「单端已定」的中态
//! （[`ProducerSetBuilder`] 生产端已定 / [`ConsumerSetBuilder`] 消费端已定）→
//! [`ReadyBuilder`]（两端已定）→ `build_async`，漏设一端无法编译。构建完成后，
//! 端类型（`P` / `C`）被确定，hook 在内部挂载；`build_async` 的返回类型由两端
//! 模式决定（**主动端不产出半部**，见上文「设计要点二」）。
//!
//! 注意：示例中的 `input` / `output` 是实现了 `TrInput` / `TrOutput` 的设备
//! （move 进缓冲）。示例为示意而保持 `ignore`，完整可运行的用法见 `tests_`
//! 模块。
//!
//! # 实现现状（重构已完成）
//!
//! 按「端类型即 hook」的目标重构完成：hook 从独立的擦除对象（旧 `hook_` 的
//! `ActiveInput` / `ActiveOutput`）改为**端类型**——两端以具体类型存放进核心
//! （`CircCore<P, C, T, A>` 的 `P` / `C`），设备因此**无需类型擦除**；段提交
//! 经 `TrCircBuffCore` 窄接口解耦（`reclaim_`）。
//!
//! 模块划分：
//!
//! * `builder`——类型状态构建链（`CircularBuffBuilder → ProducerSetBuilder /
//!   ConsumerSetBuilder → ReadyBuilder`，两端可任意换序，也可 `pipe_between`
//!   一步同时设置或直接 `build_async` 双端被动），`build_async` 在堆上装配
//!   核心并产出 [`SpscPair`]（拥有型访问模型，无「缓冲聚合体」）；
//! * `spsc_`——公共半部 [`Producer`] /
//!   [`Consumer`]（`Shared<CircCore>` + 异步等待 future）；
//! * `abs_comp_`——**内部**端契约模块（私有，不对外暴露）：`check` /
//!   `react_async` 事件模型（`react_async` 泛化段参数、端类型不携带段类型，
//!   从而解开类型级循环）与 `TrCircBuffCore`；
//!   其中的 trait 仅供核心与端类型内部协作使用，调用者不应实现或调用；
//! * `circ_buff_`——四个端类型（被动 `BuffProducer` / `BuffConsumer`，主动
//!   `DeviceProducer` / `DeviceConsumer` 携带设备）；
//! * `core_`——`CircCore<P, C, T, A>`（自有缓冲 + 原子状态机 + 事件分发 +
//!   STNDBY armed 唤醒协议 + executor 驱动的泵（`await` 设备 / 非阻塞单次
//!   poll，不自旋）；**内部**模块，不对外暴露）；
//! * `reclaim_`——两段式段（`ReclSliceMut` / `ReclSliceRef`）+ 泛型提交器。
//!
//! 旧模型文件（`half_` 借用型半部、`segm_` 旧段、`hook_` 擦除 hook、`abs_`
//! 旧 trait）已删除。
//!
//! 仍待定 / 未完成（与重构正交的长期项）：
//!
//! * **设备错误传播**：当前泵把设备错误视为「本轮无数据」，错误如何跨过 hook
//!   通知对端（例如让被动端感知设备失败）待定；
//! * **被动端取消**：`TrMayCancel` 的 `may_cancel_with` 目前忽略取消 token；
//! * **`T ≠ u8` 与主动模式**：设备元素类型与缓冲元素类型一致（`TrInput<T>`），
//!   泛型上自洽；`T` 非平凡类型时的实践（drop 语义、`Send`/`Sync` 边界）待验证；
//! * **发送/共享边界**：核心按 SPSC + 单泵线程约定实现 `Send + Sync`（见
//!   `core_` 的安全说明），多线程流水测试待补；
//! * **`ring_buffer` 去留**：`reclaim_` 已从 `ring_buffer` 复制为自有实现，
//!   将来删除 `ring_buffer` 前需先迁移 `buffex_iroh` 的使用方。
//!
//! # 与 ring_buffer 的关系
//!
//! `CircularBuff` 不改动 [`crate::ring_buffer`] 的任何既有 API 与实现。两者在
//! 「环形状态机」层面思路一致，但职责不同：`RingBuffer` 面向用户线程 + 运行时
//! （内核）两侧的管道；`CircularBuff` 面向**构造期固定模式与设备**、由 hook
//! 联动的唤醒式缓冲。规划上 `ring_buffer` 将来会被 `CircularBuff` 取代。

mod abs_comp_;
mod circ_buff_;
mod core_;
mod error_;
pub mod reclaim_;
mod spsc_;

pub mod builder;
pub use circ_buff_::{
    BufConsumer, BufProducer, DevConsumer, DevProducer,
};
pub use error_::{ConsumerError, ProducerError};
pub use reclaim_::{ReclSliceMut, ReclSliceRef};
pub use spsc_::{Consumer, Producer, Pipeline, SpscPair};


pub use crate::x_deps::abs_mm::mem_alloc::CoreAlloc;

#[cfg(test)]
mod tests_;

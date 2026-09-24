//! # `channels` —— 以 `circular_buff` 为承载的有界消息队列
//!
//! 本模块提供两类**有界**队列，消息本体就存放在
//! [`circular_buff`](crate::circular_buff) 的环形缓冲里（`circ_buff` 就是队列的
//! 容器），本模块只负责「外观」：把环形缓冲的读写半部包装成 channel 的两端，并
//! 为两端补上异步迭代的形状。
//!
//! | 类型 | 拓扑 | 写端 | 读端 |
//! | --- | --- | --- | --- |
//! | [`SpscChannel`] | 单写者 × 单读者 | [`SpscSender`] | [`SpscReceiver`] |
//! | [`MpscChannel`] | 多写者 × 单读者 | [`MpscSender`]（可克隆） | [`MpscReceiver`] |
//!
//! # 两端都实现 `abs_async_iter::TrAsyncIterator`
//!
//! **两端复用同一个 trait**，不另立门户：
//!
//! * **读端的产出物**是「一段可读数据」（[`SpscReadSegm`] / [`MpscReadSegm`]）：
//!   调用者用 `iter_slices` 就地读取或**借用**消息；
//! * **写端的产出物**是「一段可写空间」（[`SpscWriteSegm`] / [`MpscWriteSegm`]）：
//!   调用者用 `iter_slices_mut` 拿到 `&mut [MaybeUninit<T>]`，在缓冲里**原地构造**
//!   消息。
//!
//! 两端的产出物都在 **drop 时提交**：写段提交「已构造的单元数」、读段提交
//! 「已消费量」。因此 `T` **不需要 `Copy`，也不需要 `Clone`**——段借出的是内存
//! 本身，消息可以在原地构造、也可以就地借用。
//!
//! # 两端各提供两类入口
//!
//! 借鉴 `circ_buff` 自身的 `try_*` / `*_async` 分法，每一端都给出「一次一个」与
//! 「批量 / 原地」两条路：
//!
//! | 入口 | 一次处理 | 消息本体 | 典型场景 |
//! | --- | --- | --- | --- |
//! | `send` / `recv` | 1 个 | 直接 move | 消息已经现成 |
//! | `try_next` / `next_async` | 多个 | 借出段、原地构造 / 读取 | 批量、不可 `Clone` |
//!
//! `try_next` 是**非阻塞**入口；`next_async` 是它的异步对偶，产出
//! `Result<Option<..>, ChannelError>`。
//!
//! # `Ok(None)` 与 `Err(Closing)` 的分工
//!
//! 沿用 `abs_async_iter` 的约定：
//!
//! * `Ok(None)`——**暂态**没有产出：队满（背压）或队空（尚无数据）。调用者可稍后
//!   重试；
//! * `Err(Closing)`——**终止**：写端已全部关闭且已读空（EOF），或读端已关闭。
//!
//! # 多写者：写入位由协作式读写锁排队
//!
//! [`MpscWriter`](MpscSender) 之间经上游 `atomic_sync` 的
//! [`CooperativeRwLock`](atomic_sync::rwlock::cooperative::CooperativeRwLock)
//! 竞争唯一的「写入位」：锁只覆盖**取得写入位**这一小段临界区，不含数据搬运，
//! 因此后续排队者不必等前面的写者写完。
//!
//! # 放弃写入
//!
//! 「先占位、后写入」意味着写者可能在取得写入位之后放弃。本模块采取的立场是
//! **要求消息体自身表达「空」**：写者要么把消息构造完整，要么把该槽位留成
//! `T = Option<U>` 的 `None`，由消费端按业务语义跳过。一次没构造任何单元就 drop
//! 写段，等价于「什么也没发生」（提交量为 0）。这样就不必在 `circ_buff` 的核心上
//! 再加一层「提交游标 + 每笔占位状态」的空洞协议。
//!
//! # 模块划分
//!
//! * [`spsc_`]——SPSC 队列：对 `circ_buff` 被动 × 被动半部的薄包装；
//! * [`mpsc_`]——MPSC 队列：写者经协作式读写锁竞争写入位；
//! * [`error_`]——共享错误类型 [`ChannelError`]。
//!
//! # 依赖边界
//!
//! 本模块只依赖 `core` + `alloc`（`buffex` 是 `no_std`），不需要任何异步运行时。
//! 同步原语**全部来自上游 `atomic_sync`**：异步竞争用其协作式读写锁；本模块不
//! 自行实现任何锁，也不自行实现任何缓冲。

mod error_;
mod mpsc_;
mod spsc_;

pub use error_::ChannelError;
pub use mpsc_::{
    MpscBuildError, MpscChannel, MpscDefaultBuf, MpscReadSegm, MpscReadSegmOf,
    MpscReceiveNextAsync, MpscReceiver, MpscReceiverDefault,
    MpscSendNextAsync, MpscSender, MpscSenderDefault, MpscWriteSegm,
};
pub use spsc_::{
    DefaultBuf, SpscChannel, SpscChildMut, SpscChildRef, SpscReadSegm,
    SpscReceiveNextAsync, SpscReceiver, SpscReceiverDefault,
    SpscSendNextAsync, SpscSender, SpscSenderDefault, SpscWriteSegm,
};

#[cfg(test)]
mod tests_;

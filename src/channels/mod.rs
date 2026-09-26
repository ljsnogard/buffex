//! # `channels` —— 以 `circular_buff` 为承载的有界消息队列
//!
//! 本模块提供两类**有界**队列，消息本体就存放在
//! [`circular_buff`](crate::circular_buff) 的环形缓冲里（`circ_buff` 就是队列的
//! 容器）。本模块只负责「外观」：命名、收敛构建链、补齐异步迭代形状，以及一层
//! **内部槽位协议**。
//!
//! | 类型 | 拓扑 | 写端 | 读端 |
//! | --- | --- | --- | --- |
//! | [`SpscChannel`] | 单写者 × 单读者 | [`SpscSender`] | [`SpscReceiver`] |
//! | [`MpscChannel`] | 多写者 × 单读者 | [`MpscSender`]（可克隆） | [`MpscReceiver`] |
//!
//! # 内部槽位协议（不对外公开）
//!
//! 环形缓冲的每一格承载的不是裸载荷 `T`，而是内部的 `SlotMsg<T>`：**空洞**、
//! **bulk 预告头**、**正常载荷** 三选一。它不外泄——读端读到空洞一律丢弃并视作
//! 已读，读到预告头只更新「本帧还剩几格」，调用者看到的是一串干净的载荷。
//!
//! 它解决两件事：
//!
//! 1. **部分写入仍可交付前缀**：一帧 bulk 被整体提交，没填满的格是空洞，读端按
//!    预告头收完并跳过空洞，于是拿到**已写好的那部分**；
//! 2. **帧边界**：空洞既可能来自一个被放弃的单体消息，也可能来自 bulk 的成员格，
//!    预告头让读端能区分两者，不会把帧内的空洞误当作独立消息。
//!
//! # 异步迭代：读端一律实现；写端只在 MPSC 实现
//!
//! 读端两边都实现 [`TrAsyncIterator`]，产出**一条用户载荷** `T`。
//!
//! 写端**只有 MPSC 实现**（产出 [`MpscWriteTicket`]），因为 MPSC 的票走写锁，
//! 而写锁本来就是它序列化写者的那道防线。
//!
//! **SPSC 的写端不实现**，原因在 trait 的形状而不在 SPSC 的设计：
//!
//! ```text
//! type Item: Sized;                                   // 没有生命周期参数
//! fn next_async(&mut self) -> Self::NextAsync<'_>;
//! ```
//!
//! 「可写空位」本质上是借出来的（`ReclSliceMut<'f, ..>`，`'f` 就是那次
//! `&'f mut Producer`），所以它**装不进** `Item`——`Item` 与 `&mut self` 的借用无关。
//! 更麻烦的是：即使给 `Item` 加上生命周期参数也还不够，`Future::poll` 的 `Output` 是
//! 固定类型，无法把对自身的重借交出去；真正的「借出型流」需要 lending-future 形状。
//!
//! 曾经用「自有写票」（自己持一份核心句柄）绕开这条限制，那是错的：它把
//! `Producer::try_write(&mut self)` 的独占借出换成了独立句柄，票可以与
//! `try_send_bulk` 返回的 [`BulkWriter`] 同时持有重叠的段 ⇒ 别名 UB。
//! SPSC 的等待需求请用 [`SpscSender::send_async`] / [`SpscSender::send_bulk_async`]。
//!
//! # 发送侧的两类入口（两端一致）
//!
//! | 尝试（不等待） | 异步（等待） | 占用格数 | 场景 |
//! | --- | --- | --- | --- |
//! | `try_send` | `send_async` | 1 | 单体消息，载荷现成 |
//! | `try_send_bulk(n)` | `send_bulk_async(n)` | `n + 1` | 一次连续放 `n` 条 |
//!
//! **命名约定**：`try_*` 只**抢一两次**写入位（MPSC）或直接试一次（SPSC），抢不到
//! 就返回 [`ChannelError::Stuffed`] 交给调用者；带 `_async` 的会真正等待——SPSC 等
//! 空间，MPSC 等写入位与空间。两个异步入口都支持 `may_cancel_with`。
//!
//! **批量入口两端形状不同，这是被迫的**：
//!
//! * SPSC [返回一个同步的 `BulkWriter`](SpscSender::try_send_bulk)：SPSC 没有锁，
//!   段按值交出去即可，借用检查器保证「段活着时写端借不出去」；
//! * MPSC [接收一个同步闭包](MpscSender::try_send_bulk)：写许可必须覆盖「取位 →
//!   填充 → 提交」，而 `WriterGuard` 借用一个会话、会话又借用锁——锁就在发送端自己
//!   的共享分配里，把持锁的填充器作为返回值交出去就是自引用，只能靠伪造寿命
//!   （绕过借用检查）实现。会话/守卫分离的设计本意就是让「持有许可」不越出作用域，
//!   所以 MPSC 的填充发生在方法内部。
//!
//! **预告头额外占一格**：`n` 的语义始终是「用户消息条数」，协议开销由实现内部吸收
//! 并计入容量。`T` 不需要 `Copy`，也不需要 `Clone`。
//!
//! # `recv` 的 `Ok(None)` 与 `Err(Closing)`
//!
//! * `Ok(None)`——**暂态**没有数据（可稍后重试）；
//! * `Err(Closing)`——**终止**：写端已全部关闭且已读空（EOF），或读端已关闭。
//!
//! `next_async` 没有 `Option` 这一层（上游 trait 的约定）：暂态情形在 future 内部
//! `await` 底层等待 future，因此**要么产出、要么以错误终止**。
//!
//! # 多写者：写锁覆盖整个写操作
//!
//! [`MpscSender`] 之间经上游 `atomic_sync` 的
//! [`CooperativeRwLock`](atomic_sync::rwlock::cooperative::CooperativeRwLock) 排队。
//! 本版**故意**让锁覆盖「取位 → 写入 → 提交」全程，而不是「取到位置就放锁」：
//! 底层写位置要等段 drop 才推进，提前放锁会让两位写者拿到同一个写位置。代价是写者
//! 之间串行，换来的是**提交顺序 == 取位顺序**，因此单游标就足够，消费端语义不受
//! 生产端竞争影响。详见 `dev-notes/`。
//!
//! # 关闭：异步关闭 + 写者消失即关闭
//!
//! 「消息真的送到读端」这件事由两处保证：
//!
//! * [`MpscSender::close_async`] / [`SpscSender::close_async`]——**先等到写入位**
//!   再关闭。同步关闭在 MPSC 下是有害的：若有写者已占位、尚未提交，读端会在它提交
//!   之前就看到 EOF，那条消息再也送不到。MPSC 的同步 `close` 因此在写入位被占时返回
//!   `Stuffed`（不生效），而不是假装成功；
//! * **写者消失即关闭**：`SpscSender` 的 `Drop`、以及 `MpscShared` 的 `Drop`
//!   （最后一份写者引用消失时）都会关闭写端，读端因此不会永久挂起。
//!
//! # 空洞到底在防什么
//!
//! 写锁覆盖全程，所以**取位 / 写入 / 提交之间没有 `await`**，取消无法打断它；一次
//! `send` 也不会「写一半失败」。因此空洞**不是**为并发竞争准备的，它防的是：
//!
//! * 生产者在填一个连续段（bulk 帧）的中途失败——`BulkWriter` 的 `Drop` 在栈展开时
//!   把未填的格补成空洞，于是本帧仍可被整体提交，读端拿到**已写好的前缀**；
//! * 将来若允许「预留后隔一段时间再填」，同样由空洞兜底。
//!
//! 换句话说：**竞争由锁解决，空洞由放弃/崩溃兜底**，两者职责不重叠。
//!
//! # 模块划分
//!
//! * [`spsc_`]——SPSC 队列；
//! * [`mpsc_`]——MPSC 队列；
//! * [`error_`]——共享错误类型 [`ChannelError`]；
//! * [`segm_`]——段读写小工具与 [`BulkWriter`]；
//! * `slot_` / `slot_` 的协议枚举——**内部**，不外露。
//!
//! # 依赖边界与零堆分配
//!
//! 本模块只依赖 `core` + `alloc`（`buffex` 是 `no_std`），不需要任何异步运行时。
//! 同步原语**全部来自上游 `atomic_sync`**。
//!
//! **运行期不额外分配堆内存**：所有写路径（含 bulk 帧与异步等待）都不使用 `Box`
//! 之类的装箱。数据承载是队列自身的那一块（`circ_buff` 构建期分配一次），此外 MPSC
//! 有且只有一处构建期分配——`Shared<MpscShared>`，用来把**不可克隆**的 `Producer`
//! 半部与写锁共享给各个写者（`MpscSender: Clone`）。
//!
//! 写入位许可（`WriterGuard`）借用的是一个**无状态**的会话
//! （`CooperativeAcqSession` 只是 `&CooperativeRwLock` 的外壳），会话因此可以放在
//! `MpscSender` 自身里——它指向的锁位于那块固定不动的堆分配中，不需要额外装箱。

// 多写者并发填写的下一步：类型与协议已就位并有用例覆盖，但尚未接到
// `MpscSender` 的写路径上，因此在非 test 构建里暂时是「未被使用」。
#[allow(dead_code)]
mod conc_segm_;
mod error_;
mod mpsc_;
mod segm_;
mod slot_;
mod spsc_;

/// 把构建器的容量错误（`BuilderError<usize>`）转成装配期错误
/// （`BuilderError<()>`），只保留分类、丢弃被拒绝的容量数值。
///
/// `BuilderError` 是**外部 crate 的公开类型**且未实现 [`core::fmt::Display`]，
/// 也不允许在本 crate 内加 impl（孤儿规则）；因此这里保留原语义、把泛型参数抹成
/// `()`。
pub(crate) fn map_builder_err_<T>(
    e: crate::circular_buff::builder::BuilderError<T>,
) -> crate::circular_buff::builder::BuilderError<()> {
    use crate::circular_buff::builder::BuilderError;
    match e {
        BuilderError::SizeTooSmall(_) => BuilderError::SizeTooSmall(()),
        BuilderError::SizeTooBig(_) => BuilderError::SizeTooBig(()),
        BuilderError::Cancelled => BuilderError::Cancelled,
        BuilderError::ProducerInit => BuilderError::ProducerInit,
        BuilderError::ConsumerInit => BuilderError::ConsumerInit,
    }
}

pub use error_::ChannelError;
pub use mpsc_::{
    MpscBuildError, MpscChannel, MpscCloseAsync, MpscSendBulkAsync,
    MpscReceiveNextAsync, MpscReceiver, MpscSendAsync, MpscSender,
    MpscSendNextAsync, MpscWriteTicket,
};
pub use segm_::BulkWriter;
pub use spsc_::{
    DefaultBuf, SpscChannel, SpscCloseAsync, SpscReceiveNextAsync,
    SpscReceiver, SpscSendAsync, SpscSendBulkAsync, SpscSender,
};

#[cfg(test)]
mod tests_;

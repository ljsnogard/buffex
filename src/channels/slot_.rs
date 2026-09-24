//! 内部**槽位协议**：环形缓冲每一格承载的「完整消息」。
//!
//! # 为什么需要它
//!
//! 队列要同时表达三件事，而它们都不是「用户载荷」本身：
//!
//! 1. **空洞**——一次预留最终没有被填满时，剩余的格必须有一个**已初始化**的值，
//!    否则按整段提交会让消费端读到未初始化内存。空洞让「整段提交」变得安全；
//! 2. **bulk 帧的边界**——一个连续写段由**同一个生产者**独占（见 `dev-notes/`），
//!    消费端需要知道「接下来的 `len` 格属于同一帧」，才能把帧内的空洞与孤立的
//!    空洞区分开；
//! 3. 将来可能出现的其它控制消息。
//!
//! # 不对外公开
//!
//! 本枚举是**内部协议**：公开 API 只呈现用户载荷 `T`。消费端读到空洞一律**丢弃并
//! 视作已读**，读到帧头则按帧收完，因此空洞与控制信息都不会暴露给最终调用者。

/// 环形缓冲每格承载的完整消息。
///
/// 只在本模块内部与 `channels` 内部使用；对外由
/// [`SpscSender`](super::SpscSender) / [`SpscReceiver`](super::SpscReceiver) 等
/// 类型把载荷 `T` 取出来交给调用者。
pub(super) enum SlotMsg<T> {
    /// **空洞**：预留时写入的默认值，表示「这一格没有有效载荷」。
    ///
    /// 两种来源：bulk 帧中没有被生产者填满的成员格；或整个预留被放弃时补上的格。
    Void,
    /// **正常载荷**：单体消息，或 bulk 帧的成员。
    Payload(T),
    /// **关闭信号**：写端在关闭前放进环里的最后一条协议消息。
    ///
    /// 它让「写端已关闭」成为**带内**信息：读端按顺序读到它时，此前所有已提交的
    /// 载荷必然已经被读到（它们排在它前面），这就是「关闭前必须给消费端发协议
    /// 信号，保证客户端收到」的落点。标志位（`TX_CLOSED`）只用来让写端停止取位。
    Closing,
    /// **bulk 预告头**：紧随其后的 `len` 格属于同一个连续段。
    ///
    /// 由申请该写段的生产者在段的**第一格**写入，因此帧边界对消费端始终可见。
    BulkHead {
        /// 本帧的成员格数。
        len: usize,
    },
}

impl<T> Default for SlotMsg<T> {
    /// 内部协议的默认值即空洞——这正是「预填充用默认值写空洞」的落点。
    #[inline]
    fn default() -> Self {
        SlotMsg::Void
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for SlotMsg<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SlotMsg::Void => write!(f, "Void"),
            SlotMsg::Closing => write!(f, "Closing"),
            SlotMsg::Payload(t) => write!(f, "Payload({t:?})"),
            SlotMsg::BulkHead { len } => write!(f, "BulkHead{{len: {len}}}"),
        }
    }
}

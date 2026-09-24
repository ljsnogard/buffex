//! `channels` 模块的错误类型。
//!
//! 两类队列（[`SpscChannel`](super::SpscChannel) /
//! [`MpscChannel`](super::MpscChannel)）共享**同一个**错误类型
//! [`ChannelError`]：对调用者而言，「队列满 / 队列关闭 / 操作被取消」在两种拓扑
//! 下语义一致，不必为它们各写一份匹配分支。
//!
//! # 与 `circular_buff` 错误类型的关系
//!
//! 底层 [`crate::circular_buff::ProducerError`] / [`ConsumerError`] 是**层次更低**
//! 的错误：它们携带环形缓冲的位置快照（例如 `Stuffed(wp)`），并把「本端是主动
//! 模式」这类**构建期拓扑**问题也混在同一枚举里。通道层对外只关心「能不能继续
//! 下去」，因此在 [`From`] 转换里把位置快照丢弃、把标签收敛到下面列出的几种
//! 语义上。
//!
//! [`ConsumerError`]: crate::circular_buff::ConsumerError

use core::fmt;

use crate::circular_buff::{ConsumerError, ProducerError};

/// 队列操作失败的原因。
///
/// # Examples
///
/// ```
/// use buffex::channels::ChannelError;
///
/// let e = ChannelError::Stuffed;
/// assert_eq!(e.to_string(), "ChannelError::Stuffed");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelError {
    /// 队列内没有可用空间（有界队列的背压）。调用者应在读端消费之后重试。
    Stuffed,

    /// 队列内没有可读数据，且**再也等不到新数据**：读端已关闭，或写端已关闭
    /// 且残留数据已被读空（EOF）。
    ///
    /// 这是 [`TrAsyncIterator::next_async`] 返回 `Result::Err` 与返回
    /// `Result::Ok(Option::None)` 的分界：前者表示「出错，迭代应当终止」，后者
    /// 表示「正常流结束」。
    ///
    /// [`TrAsyncIterator::next_async`]:
    ///     abs_async_iter::TrAsyncIterator::next_async
    Closing,

    /// 本次操作被取消令牌中断。
    Cancelled,

    /// 操作在两层之间**无法安全表达**。
    ///
    /// 典型场景：写者取得了写入位却放弃写入，需要把已占据的位置回退——而回退会
    /// 破坏环形缓冲「提交只能单调推进」的不变量。详见模块文档「放弃写入」与
    /// `dev-notes/`。
    Unsupported,

    /// 参数非法（例如容量为 0 的 `Demand`）。
    Argument,
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelError::Stuffed => write!(f, "ChannelError::Stuffed"),
            ChannelError::Closing => write!(f, "ChannelError::Closing"),
            ChannelError::Cancelled => write!(f, "ChannelError::Cancelled"),
            ChannelError::Unsupported => write!(f, "ChannelError::Unsupported"),
            ChannelError::Argument => write!(f, "ChannelError::Argument"),
        }
    }
}

impl core::error::Error for ChannelError {}

impl<S> From<ProducerError<S>> for ChannelError {
    fn from(e: ProducerError<S>) -> Self {
        match e {
            ProducerError::Stuffed(_) => ChannelError::Stuffed,
            ProducerError::Closing => ChannelError::Closing,
            ProducerError::Cancelled => ChannelError::Cancelled,
            // 「本端是主动模式」说明调用者构建了一个设备驱动的队列，却想按通道
            // 来用：这是拓扑层面的误用，不是暂态背压。
            ProducerError::Unavailable => ChannelError::Unsupported,
            ProducerError::Argument => ChannelError::Argument,
        }
    }
}

impl<S> From<ConsumerError<S>> for ChannelError {
    fn from(e: ConsumerError<S>) -> Self {
        match e {
            ConsumerError::Drained(_) => ChannelError::Closing,
            ConsumerError::Closing => ChannelError::Closing,
            ConsumerError::Cancelled => ChannelError::Cancelled,
            ConsumerError::Unavailable => ChannelError::Unsupported,
            ConsumerError::Argument => ChannelError::Argument,
        }
    }
}

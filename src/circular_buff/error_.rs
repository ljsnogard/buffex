//! `CircularBuff` 的错误类型。
//!
//! * [`TxError`]——写（生产）端的错误，携带失败时写位置/空间快照；
//! * [`RxError`]——读（消费）端的错误，携带失败时读位置/数据快照；
//! * [`EndError`]——端访问错误：主动端不对外暴露，尝试访问时的错误。

use core::fmt;

use abs_buff::error::{IoErrTag, ReadErrTag, TrTaggedError, WriteErrTag};

/// 写（生产）端错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerError<S> {
    /// 缓冲区已满（或可写空间不足 `Demand` 下限），携带当前写位置。
    Stuffed(S),
    /// 写端已关闭，不再接受数据。
    Closing,
    /// 调用者主动取消
    Cancelled,
    /// 本端为主动模式（设备驱动），不对外提供写访问。
    Unavailable,
    /// 参数非法（例如 `Demand` 区间非法）。
    Argument,
}

impl<S: fmt::Debug> fmt::Display for ProducerError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProducerError::Stuffed(p) => write!(f, "ProducerError::Stuffed(at: {p:?})"),
            ProducerError::Closing => write!(f, "ProducerError::Closing"),
            ProducerError::Cancelled => write!(f, "ProducerError::Cancelled"),
            ProducerError::Unavailable => write!(f, "ProducerError::Unavailable"),
            ProducerError::Argument => write!(f, "ProducerError::Argument"),
        }
    }
}

impl<S: fmt::Debug> core::error::Error for ProducerError<S> {}

impl<S: fmt::Debug> TrTaggedError<WriteErrTag> for ProducerError<S> {
    fn err_tag(&self) -> WriteErrTag {
        match self {
            ProducerError::Closing | ProducerError::Cancelled | ProducerError::Unavailable
                => WriteErrTag::Closing,
            ProducerError::Argument => WriteErrTag::Unknown,
            ProducerError::Stuffed(_) => WriteErrTag::Stuffed,
        }
    }
}

/// 读（消费）端错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerError<S> {
    /// 缓冲区已空（或可读数据不足 `Demand` 下限），携带当前读位置。
    Drained(S),
    /// 读端已关闭，不再有数据。
    Closing,
    /// 调用者主动取消
    Cancelled,
    /// 本端为主动模式（设备驱动），不对外提供读访问。
    Unavailable,
    /// 参数非法。
    Argument,
}

impl<S: fmt::Debug> fmt::Display for ConsumerError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConsumerError::Drained(p) => write!(f, "ConsumerError::Drained(at: {p:?})"),
            ConsumerError::Closing => write!(f, "ConsumerError::Closing"),
            ConsumerError::Cancelled => write!(f, "ConsumerError::Cancelled"),
            ConsumerError::Unavailable => write!(f, "ConsumerError::Unavailable"),
            ConsumerError::Argument => write!(f, "ConsumerError::Argument"),
        }
    }
}

impl<S: fmt::Debug> core::error::Error for ConsumerError<S> {}

impl<S: fmt::Debug> TrTaggedError<ReadErrTag> for ConsumerError<S> {
    fn err_tag(&self) -> ReadErrTag {
        match self {
            ConsumerError::Closing | ConsumerError::Cancelled | ConsumerError::Unavailable
                => ReadErrTag::Closing,
            ConsumerError::Argument => ReadErrTag::Unknown,
            ConsumerError::Drained(_) => ReadErrTag::Drained,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineError<S> {
    Tx(ProducerError<S>),
    Rx(ConsumerError<S>),
}

impl<S: fmt::Debug> fmt::Display for PipelineError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::Tx(tx_err) => tx_err.fmt(f),
            PipelineError::Rx(rx_err) => rx_err.fmt(f),
        }
    }
}

impl<S: fmt::Debug> core::error::Error for PipelineError<S> {}

impl<S: fmt::Debug> TrTaggedError<IoErrTag> for PipelineError<S> {
    fn err_tag(&self) -> IoErrTag {
        match self {
            PipelineError::Tx(tx_err) => IoErrTag::Write(tx_err.err_tag()),
            PipelineError::Rx(rx_err) => IoErrTag::Read(rx_err.err_tag()),
        }
    }
}

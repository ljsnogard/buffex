//! Error types shared by the rx / tx ends of the ring buffer.

use core::{error::Error, fmt};

/// Error that may occur while operating the rx end of the ring buffer.
#[derive(Debug)]
pub enum RxError<T> {
    /// Illegal argument.
    Argument,

    /// The input end has closed and the ring buffer is already empty.
    Closing,

    /// The ring buffer is empty and thus temporarily unable to output.
    Drained(T),
}

impl<T> fmt::Display for RxError<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RxError::Argument => write!(f, "RxError::Argument"),
            RxError::Closing => write!(f, "RxError::Closing"),
            RxError::Drained(t) => write!(f, "RxError::Drained({t:?})"),
        }
    }
}

impl<T> Error for RxError<T> where T: fmt::Debug {}

/// Error that may occur while operating the tx end of the ring buffer.
#[derive(Debug)]
pub enum TxError<T> {
    /// Illegal argument.
    Argument,

    /// The output end has closed and the buffer is already full.
    Closing,

    /// The ring buffer is full and thus temporarily unable to input.
    Stuffed(T),
}

impl<T> fmt::Display for TxError<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TxError::Argument => write!(f, "TxError::Argument"),
            TxError::Closing => write!(f, "TxError::Closing"),
            TxError::Stuffed(t) => write!(f, "TxError::Stuffed({t:?})"),
        }
    }
}

impl<T> Error for TxError<T> where T: fmt::Debug {}

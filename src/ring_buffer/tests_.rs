use std::{
    borrow::{Borrow, BorrowMut},
    boxed::Box,
    sync::Arc,
};

use env_logger;
use tokio;

use abs_sync::cancellation::NonCancellableToken;
use atomex::{StrictOrderings, TrCmpxchOrderings};

use crate::{
    ring_buffer::{BuffRx, BuffTx, RingBuffer},
    x_deps::{abs_sync, atomex}
};

#[tokio::test]
async fn single_byte_demo() {
    const ARR_SIZE: usize = 1usize;

    let _ = env_logger::builder().is_test(true).try_init();

    let arr = Box::new([0u8; ARR_SIZE]);
    let ring_buf = Arc::new(RingBuffer::<Box<[u8]>, u8, StrictOrderings>
        ::try_new(arr).unwrap());
    let try_split = RingBuffer::try_split(
        ring_buf,
        Arc::strong_count,
        Arc::weak_count,
    );
    let Result::Ok((tx, rx)) = try_split else {
        unreachable!()
    };
    let rx_task = tokio::task::spawn(rx_work_(rx));
    let tx_task = tokio::task::spawn(tx_work_(tx));

    assert!(tx_task.await.is_ok());
    assert!(rx_task.await.is_ok());

    async fn tx_work_<B, P, O>(mut buff_write: BuffTx<B, P, u8, O>)
    where
        B: Borrow<RingBuffer<P, u8, O>>,
        P: BorrowMut<[u8]>,
        O: TrCmpxchOrderings,
    {
        let mut b = 0u8;
        loop {
            let x = buff_write
                .write_async(ARR_SIZE)
                .may_cancel_with(NonCancellableToken::pinned())
                .await;
            let Result::Ok(buff_iter) = x else {
                let err = x.err().unwrap();
                log::trace!("[single_byte_demo::tx_work_] err: {err:?}");
                break;
            };
            for mut buff in buff_iter.into_iter() {
                buff[0] = b;
                log::trace!("[single_byte_demo::tx_work_] {b}");
                if b == u8::MAX {
                    break;
                } else {
                    b += 1;
                }
            }
        }
        log::trace!("[single_byte_demo::tx_work_] exit at: b({b})");
    }

    async fn rx_work_<B, P, O>(mut buff_read: BuffRx<B, P, u8, O>)
    where
        B: Borrow<RingBuffer<P, u8, O>>,
        P: BorrowMut<[u8]>,
        O: TrCmpxchOrderings,
    {
        let mut b = 0u8;
        loop {
            let x = buff_read
                .read_async(ARR_SIZE)
                .may_cancel_with(NonCancellableToken::pinned())
                .await;
            let Result::Ok(buff_iter) = x else {
                let err = x.err().unwrap();
                log::trace!("[single_byte_demo::rx_work_] err: {err:?}");
                break;
            };
            for buff in buff_iter.into_iter() {
                let x = buff[0];
                log::trace!("[single_byte_demo::rx_work_] {x}");
                assert_eq!(x, b);
                if b == u8::MAX {
                    break;
                } else {
                    b += 1;
                }
            }
        }
        log::trace!("[single_byte_demo::rx_work_] exit at: b({b})");
    }
}

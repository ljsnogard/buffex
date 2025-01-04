use std::{
    borrow::{Borrow, BorrowMut},
    boxed::Box,
    mem::MaybeUninit,
    sync::Arc,
};

use tokio;

use atomex::{StrictOrderings, TrCmpxchOrderings};

use crate::{
    ring_buffer::{BuffRx, BuffTx, RingBuffer},
    x_deps::atomex,
};

const ARR_SIZE: usize = 1usize;
const INDEX: usize = 0usize;

async fn tx_work_<B, P, O>(mut tx: BuffTx<B, P, u8, O>)
where
    B: Borrow<RingBuffer<P, u8, O>>,
    P: BorrowMut<[MaybeUninit<u8>]>,
    O: TrCmpxchOrderings,
{
    let mut b = 0u8;
    loop {
        if b == u8::MAX {
            break;
        }
        let x = tx.write_async(ARR_SIZE).await;
        let Result::Ok(buff_iter) = x else {
            let err = x.err().unwrap();
            log::trace!("[single_byte_demo::tx_work_] err: {err:?}");
            break;
        };
        for mut buff in buff_iter.into_iter() {
            buff[INDEX].write(b);
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

async fn rx_work_<B, P, O>(mut rx: BuffRx<B, P, u8, O>)
where
    B: Borrow<RingBuffer<P, u8, O>>,
    P: BorrowMut<[MaybeUninit<u8>]>,
    O: TrCmpxchOrderings,
{
    let mut b = 0u8;
    loop {
        if b == u8::MAX {
            break;
        }
        log::trace!("[single_byte_demo::rx_work_] b({b})");
        let x = rx.read_async(ARR_SIZE).await;
        let Result::Ok(buff_iter) = x else {
            let err = x.err().unwrap();
            log::trace!("[single_byte_demo::rx_work_] err: {err:?}");
            break;
        };
        for buff in buff_iter.into_iter() {
            let x = buff[INDEX];
            log::trace!("[single_byte_demo::rx_work_] x({x}), b({b})");
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


#[tokio::test]
async fn single_byte_async_smoke() {
    let _ = env_logger::builder().is_test(true).try_init();

    let arr = Box::<[u8]>::new_uninit_slice(ARR_SIZE);
    let ring_buf = Arc::new(
        RingBuffer::<Box<[MaybeUninit<u8>]>, u8, StrictOrderings>
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
}

//! 关闭 / EOF 事件与 hook 行为的测试：
//!
//! * 生产者关闭 → 消费端 hook 收到 `ProducerClose`，读者获得 EOF 语义
//!   （剩余数据可读，读空后返回 `Closing`）；
//! * 消费者关闭 → 生产端 hook 收到 `ConsumerClose`，写者感知对端关闭；
//! * 异步等待在关闭事件下不再永远等待。

use std::{pin::pin, vec};

use abs_buff::{Demand, TrBuffTryRead, TrBuffTryWrite};

use super::{
    super::ConsumerError,
    DefaultBuilder, fill_segm, poll_once, take_segm, Pair, TestWaker,
};

/// 构建被动 × 被动半部对（测试辅助，见 [`super::sync_`] 的说明）。
///
/// `build` 已改为异步（`build_async`）：同步测试用 `block_on` 驱动构建 future。
fn make_pair<const N: usize>() -> Pair {
    let mut ready = DefaultBuilder::with_capacity(N)
        .unwrap()
        .producer_passive()
        .consumer_passive();
    futures_lite::future::block_on(ready.build_async().into_future()).unwrap()
}

/// 生产者关闭（EOF）：触发消费端 hook `ProducerClose`；读者可读尽剩余数据，
/// 读空后返回 `Closing`。
#[test]
fn producer_close_gives_eof() {
    let (mut tx, mut rx) = make_pair::<8>();

    // 写 2 字节后关闭写端。
    let demand = Demand::at_least(2);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[5, 6]);
    drop(ws);
    tx.close();
    assert!(rx.is_producer_closed(), "读者应感知生产者关闭（EOF）");

    // EOF 例外：数据不足下限（2 < 5）也返回剩余部分，而不是等待。
    let demand = Demand::at_least(5);
    let some = TrBuffTryRead::try_read(&mut rx, &demand);
    let mut rs = some.pick_left().expect("关闭后应返回剩余部分（EOF）");
    assert_eq!(rs.least_count(), 2);
    assert_eq!(take_segm(&mut rs, 2), vec![5, 6]);
    drop(rs);

    // 读空后：空 + 关闭 → Closing。
    let demand = Demand::at_least(1);
    let some = TrBuffTryRead::try_read(&mut rx, &demand);
    assert!(
        matches!(some.pick_right(), Some(ConsumerError::Closing)),
        "读空且写端已关闭时应返回 Closing"
    );
}

/// 消费者关闭：触发生产端 hook `ConsumerClose`；写者感知对端关闭，
/// 但仍可继续写入（数据无人消费，环满即止）。
#[compio::test]
async fn consumer_close_fires_event() {
    let (mut tx, mut rx) = make_pair::<8>();

    rx.close_async().await;
    assert!(tx.is_consumer_closed(), "写者应感知消费者关闭");

    // 关闭后写者仍可写（直到写满）。
    let demand = Demand::at_least(2);
    let some = TrBuffTryWrite::try_write(&mut tx, &demand);
    assert!(some.pick_left().is_some(), "消费者关闭后写者仍可写入");
}

/// 异步读等待在生产者关闭时被唤醒并返回 `Closing`，而不是永远 pending。
#[test]
fn read_async_returns_closing_on_eof() {
    let (mut tx, mut rx) = make_pair::<8>();

    // 读者等 3 字节：当前为空 → Pending（已注册 waker）。
    let demand = Demand::at_least(3);
    let fut = rx.read_async(&demand);
    let mut fut = pin!(fut.into_future());
    let (waker, _flag) = TestWaker::make_waker_tuple();
    assert!(poll_once(fut.as_mut(), &waker).is_pending());

    // 生产者关闭 → 消费端 hook（`ProducerClose`）唤醒读者。
    tx.close();

    let res = poll_once(fut.as_mut(), &waker);
    match res {
        std::task::Poll::Ready(r) => {
            assert!(
                r.pick_right().is_some(),
                "空环 + 生产者关闭：读等待应返回 Closing"
            );
        }
        std::task::Poll::Pending => panic!("生产者关闭后读者不得永远等待"),
    }
}

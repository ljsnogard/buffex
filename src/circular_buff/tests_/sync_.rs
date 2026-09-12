//! 被动 × 被动模式的测试：读写往返、`Demand` 语义（不足下限不返回）、
//! 跨末端环绕的两段式段、以及异步等待（hook 唤醒）。

use std::pin::pin;

use std::{vec, vec::Vec};

use abs_buff::{Demand, TrBuffTryRead, TrBuffTryWrite};

use super::{
    super::{ConsumerError, ProducerError},
    DefaultBuilder, fill_segm, poll_once, take_segm, Pair, TestWaker,
};

/// 构建一个容量 `N` 的被动 × 被动半部对（测试辅助）。
///
/// `build` 已改为异步（`build_async`，主动端在构建期完成异步初始化）：同步
/// `#[test]` 用最小单线程执行器（`futures_lite::future::block_on`）把构建
/// future 驱动到完成。`into_future()` 把生成型构建 future（`TrMayCancel`
/// 包装）转成普通 `Future` 供 `block_on` 接收。
fn make_pair<const N: usize>() -> Pair {
    let mut ready = DefaultBuilder::with_capacity(N)
        .unwrap()
        .producer_passive()
        .consumer_passive();
    futures_lite::future::block_on(ready.build_async().into_future()).unwrap()
}

/// 写入 / 读出往返：写 3 字节，读回同样的 3 字节，位置正确推进。
#[test]
fn write_read_roundtrip() {
    let (mut tx, mut rx) = make_pair::<8>();

    // 写 3 字节。
    let demand = Demand::at_least(3);
    let some = TrBuffTryWrite::try_write(&mut tx, &demand);
    let mut ws = some.pick_left().expect("应有 3 格可写空间");
    fill_segm(&mut ws, &[1, 2, 3]);
    drop(ws);
    assert_eq!(rx.data_size(), 3, "写后应有 3 字节可读");

    // 读回 3 字节。
    let demand = Demand::at_least(3);
    let some = TrBuffTryRead::try_read(&mut rx, &demand);
    let mut rs = some.pick_left().expect("应有 3 字节可读");
    let n = rs.least_count();
    let got = take_segm(&mut rs, n);
    drop(rs);
    assert_eq!(got, vec![1, 2, 3]);
    assert_eq!(rx.data_size(), 0, "读后应为空");
}

/// `try_read(&Demand::at_least(4))`：环里只有 2 字节时，**不得**返回 2 字节的
/// 段，而应返回 `Drained` 错误（数量不足下限）。
#[test]
fn try_read_honours_at_least() {
    let (mut tx, mut rx) = make_pair::<8>();

    let demand = Demand::at_least(2);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[1, 2]);
    drop(ws);

    let demand = Demand::at_least(4);
    let some = TrBuffTryRead::try_read(&mut rx, &demand);
    assert!(
        matches!(some.pick_right(), Some(ConsumerError::Drained(_))),
        "数据不足下限时必须返回 Drained，而不是不足量的段"
    );
}

/// `try_write(&Demand::at_least(4))`：可写空间只有 3 格时，**不得**返回 3 格的
/// 段，而应返回 `Stuffed` 错误。
#[test]
fn try_write_honours_at_least() {
    let (mut tx, mut _rx) = make_pair::<8>();

    // 写 5 字节（一次借出整个可写区，只提交 5）：容量 8 → free = 3。
    let demand = Demand::at_least(1);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .expect("应可写");
    assert!(ws.least_count() >= 5, "可写区应至少 5 格");
    fill_segm(&mut ws, &[0; 5]);
    drop(ws);
    assert_eq!(tx.free_size(), 3, "写 5 后应剩 3 格可写空间");

    let demand = Demand::at_least(4);
    let some = TrBuffTryWrite::try_write(&mut tx, &demand);
    assert!(
        matches!(some.pick_right(), Some(ProducerError::Stuffed(_))),
        "可写空间不足下限时必须返回 Stuffed"
    );
}

/// 跨末端环绕：可写 / 可读区域绕到缓冲区开头时，段拆成两段物理空间、
/// 逻辑上仍是一段，数据按顺序填入 / 读出。
#[test]
fn wrap_around_two_pieces() {
    let (mut tx, mut rx) = make_pair::<5>();

    // 写 [1,2,3]：wp = 3。
    let demand = Demand::at_least(3);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[1, 2, 3]);
    drop(ws);

    // 读 2：[1,2] → rp = 2。
    let demand = Demand::at_least(2);
    let mut rs = TrBuffTryRead::try_read(&mut rx, &demand)
        .pick_left()
        .unwrap();
    assert_eq!(take_segm(&mut rs, 2), vec![1, 2]);
    drop(rs);

    // 再写 3：可写区 = [3,5) 两格 + [0,1) 一格（跨末端，两段式写段）。
    let demand = Demand::at_least(4);
    let some = TrBuffTryWrite::try_write(&mut tx, &demand);
    let mut ws = some.pick_left().expect("跨末端也应一次借出全部 4 格");
    let slices: Vec<usize> = ws.iter_slices_mut().map(|s| s.len()).collect();
    assert_eq!(slices, vec![2, 2], "跨末端写段应为两段：[3,5) 与 [0,2)");
    fill_segm(&mut ws, &[10, 11, 12]);
    drop(ws);

    // 读全部 4：可读区 = [2,5) + [0,1)，两段式读段，顺序读出。
    let demand = Demand::at_least(4);
    let mut rs = TrBuffTryRead::try_read(&mut rx, &demand)
        .pick_left()
        .expect("应有 4 字节可读");
    let n = rs.least_count();
    let got = take_segm(&mut rs, n);
    drop(rs);
    assert_eq!(got, vec![3, 10, 11, 12]);
}

/// 读侧异步等待：无数据时 `read_async` 进入 pending 并注册 waker；
/// 写端写入触发消费端 hook，唤醒读者后恢复为 Ready。
#[test]
fn read_async_wakes_on_write() {
    let (mut tx, mut rx) = make_pair::<8>();

    // 读者先等 3 字节：当前为空 → Pending。
    let demand = Demand::at_least(3);
    let fut = rx.read_async(&demand);
    let mut fut = pin!(fut.into_future());
    let (waker, _flag) = TestWaker::make_waker_tuple();
    assert!(
        poll_once(fut.as_mut(), &waker).is_pending(),
        "无数据时读等待必须 pending"
    );

    // 写端写入 → 提交路径触发消费端 hook（被动=唤醒读者）。
    let demand = Demand::at_least(3);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[7, 8, 9]);
    drop(ws);

    // 重新轮询：应 Ready 并借出 3 字节的读段。
    let res = poll_once(fut.as_mut(), &waker);
    let mut rs = match res {
        std::task::Poll::Ready(r) => r.pick_left().expect("读等待应成功"),
        std::task::Poll::Pending => panic!("写入后读者应被唤醒"),
    };
    assert_eq!(rs.least_count(), 3);
    assert_eq!(take_segm(&mut rs, 3), vec![7, 8, 9]);
}

/// 写侧异步等待：环写满时 `write_async` 进入 pending 并注册 waker；
/// 读端读取释放空间，触发生产端 hook，唤醒写者后恢复为 Ready。
#[test]
fn write_async_wakes_on_read() {
    let (mut tx, mut rx) = make_pair::<4>(); // 容量 4

    // 写满 4 字节。
    let demand = Demand::at_least(4);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[1, 2, 3, 4]);
    drop(ws);
    assert_eq!(tx.free_size(), 0, "环应已写满");

    // 写者等 1 格空间：当前满 → Pending。
    let demand = Demand::at_least(1);
    let fut = tx.write_async(&demand);
    let mut fut = pin!(fut.into_future());
    let (waker, _flag) = TestWaker::make_waker_tuple();
    assert!(
        poll_once(fut.as_mut(), &waker).is_pending(),
        "环满时写等待必须 pending"
    );

    // 读端读走 1 字节 → 释放空间 → 生产端 hook 唤醒写者。
    let demand = Demand::exactly(1);
    let mut rs = TrBuffTryRead::try_read(&mut rx, &demand)
        .pick_left()
        .unwrap();
    assert_eq!(take_segm(&mut rs, 1), vec![1]);
    drop(rs);

    let res = poll_once(fut.as_mut(), &waker);
    let mut ws = match res {
        std::task::Poll::Ready(r) => r.pick_left().expect("写等待应成功"),
        std::task::Poll::Pending => panic!("读取后写者应被唤醒"),
    };
    // 容量 4：满环读走 1 格后剩余数据 3、free = 1。
    assert_eq!(ws.least_count(), 1, "读走 1 格后应可写 1 格");
    fill_segm(&mut ws, &[4]);
    drop(ws);
    assert_eq!(rx.data_size(), 4, "读走 1 格后又写回 1 格 → 回到满环");
}

/// 被动端的 `check` 按等待者的需求下限裁决：写入量不足下限时**不唤醒**读者
/// （demand 门控），达到下限才唤醒。
#[test]
fn read_async_demand_gates_wakeup() {
    use std::pin::pin;

    let (mut tx, mut rx) = make_pair::<8>();

    // 读者等 5 字节 → Pending（已登记 demand=5、注册 waker）。
    let demand = Demand::at_least(5);
    let fut = rx.read_async(&demand);
    let mut fut = pin!(fut.into_future());
    let (waker, flag) = TestWaker::make_waker_tuple();
    assert!(poll_once(fut.as_mut(), &waker).is_pending());

    // 只写 2 字节：不足下限 → check 裁决不感兴趣 → 不唤醒。
    let demand = Demand::at_least(2);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[1, 2]);
    drop(ws);
    assert!(
        !flag.load(std::sync::atomic::Ordering::Acquire),
        "不足需求下限时不得唤醒读者"
    );
    assert!(
        poll_once(fut.as_mut(), &waker).is_pending(),
        "不足需求下限时读等待仍应 pending"
    );

    // 再写 3 字节（累计 5）：达到下限 → check 感兴趣 → 唤醒 → Ready。
    let demand = Demand::at_least(3);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[3, 4, 5]);
    drop(ws);
    assert!(
        flag.load(std::sync::atomic::Ordering::Acquire),
        "达到需求下限后应唤醒读者"
    );
    let res = poll_once(fut.as_mut(), &waker);
    let mut rs = match res {
        std::task::Poll::Ready(r) => r.pick_left().expect("读等待应成功"),
        std::task::Poll::Pending => panic!("达到下限后读者应被唤醒"),
    };
    assert_eq!(rs.least_count(), 5);
    assert_eq!(take_segm(&mut rs, 5), vec![1, 2, 3, 4, 5]);
}

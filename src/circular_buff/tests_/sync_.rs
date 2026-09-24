//! 被动 × 被动模式的测试：读写往返、`Demand` 语义（不足下限不返回）、
//! 跨末端环绕的两段式段、以及异步等待（hook 唤醒）。
//!
//! # 运行时与驱动方式
//!
//! 需要 `build_async` / `read_async` / `write_async` 的用例一律写成 `async fn`，
//! 由 [`dual_runtime_test_`](crate::test_support_::dual_runtime_test_) 在 **tokio**
//! 与 **compio** 两种**真实运行时**下各跑一遍；不 `block_on`、不手动构造 waker。
//!
//! 原来「手动 poll 一次断言 `Pending`、对端提交后再 poll 断言 `Ready`」的用例改成
//! `futures_util::join!` 的真实并发：一边等待、一边推进另一端（先 `yield_now`
//! 让等待侧先撞上等待条件），断言最终数据完整送达。

use std::{vec, vec::Vec};

use abs_buff::{Demand, TrBuffTryRead, TrBuffTryWrite};

use super::{
    super::{ConsumerError, ProducerError},
    DefaultBuilder, Pair, fill_segm, take_segm,
};
use crate::test_support_::dual_runtime_test_;

/// 构建一个容量 `N` 的被动 × 被动半部对（测试辅助）。
///
/// `build_async` 在真实运行时里被 `.await` 驱动到完成。
async fn make_pair_<const N: usize>() -> Pair {
    let mut ready = DefaultBuilder::with_capacity(N)
        .unwrap()
        .producer_passive()
        .consumer_passive();
    ready.build_async().await.unwrap()
}

/// 写入 / 读出往返：写 3 字节，读回同样的 3 字节，位置正确推进。
/// - 手段：容量 8 上借出写段写 `[1,2,3]` 并提交，再借出读段读 3 字节。
/// - 判断：读回序列等于写入序列；读后 `data_size` 为 0。
async fn write_read_roundtrip_() {
    let (mut tx, mut rx) = make_pair_::<8>().await;

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

dual_runtime_test_!(write_read_roundtrip_);

/// `try_read(&Demand::at_least(4))`：环里只有 2 字节时，**不得**返回 2 字节的
/// 段，而应返回 `Drained` 错误（数量不足下限）。
/// - 手段：写 2 字节后以 `at_least(4)` 读。
/// - 判断：返回 `ConsumerError::Drained`。
async fn try_read_honours_at_least_() {
    let (mut tx, mut rx) = make_pair_::<8>().await;

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

dual_runtime_test_!(try_read_honours_at_least_);

/// `try_write(&Demand::at_least(4))`：可写空间只有 3 格时，**不得**返回 3 格的
/// 段，而应返回 `Stuffed` 错误。
/// - 手段：容量 8 上写 5 字节（free = 3），再以 `at_least(4)` 借写段。
/// - 判断：返回 `ProducerError::Stuffed`。
async fn try_write_honours_at_least_() {
    let (mut tx, mut _rx) = make_pair_::<8>().await;

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

dual_runtime_test_!(try_write_honours_at_least_);

/// 跨末端环绕：可写 / 可读区域绕到缓冲区开头时，段拆成两段物理空间、
/// 逻辑上仍是一段，数据按顺序填入 / 读出。
/// - 手段：容量 5 上写 3、读 2，再以 `at_least(4)` 写 3、读 4。
/// - 判断：跨末端写段拆成 `[2, 2]` 两段；读回序列为 `[3, 10, 11, 12]`。
async fn wrap_around_two_pieces_() {
    let (mut tx, mut rx) = make_pair_::<5>().await;

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

dual_runtime_test_!(wrap_around_two_pieces_);

/// 读侧异步等待：无数据时 `read_async` 挂起并注册 waker；写端写入触发消费端
/// hook，唤醒读者后完成读取。
/// - 手段：`join!` 并发「等 3 字节的读等待」与「先让出一次、再写 `[7,8,9]`」；
///   让出一次确保读者先撞上等待条件并注册 waker。
/// - 判断：读等待最终成功借出 3 字节，内容为 `[7, 8, 9]`。
async fn read_async_wakes_on_write_() {
    let (mut tx, mut rx) = make_pair_::<8>().await;

    let read = async {
        let demand = Demand::at_least(3);
        let res = rx.read_async(&demand).await;
        let mut rs = res.pick_left().expect("写入后读等待应成功");
        assert_eq!(rs.least_count(), 3);
        assert_eq!(take_segm(&mut rs, 3), vec![7, 8, 9]);
    };
    let write = async {
        // 先让出一次，确保读者已经挂起并注册 waker。
        futures_lite::future::yield_now().await;
        let demand = Demand::at_least(3);
        let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
            .pick_left()
            .expect("应可写 3 格");
        fill_segm(&mut ws, &[7, 8, 9]);
        drop(ws); // 提交 → 触发消费端 hook 唤醒读者
    };
    let ((), ()) = futures_util::join!(read, write);
}

dual_runtime_test_!(read_async_wakes_on_write_);

/// 写侧异步等待：环写满时 `write_async` 挂起并注册 waker；读端读取释放空间，
/// 触发生产端 hook，唤醒写者后完成写入。
/// - 手段：容量 4 写满；`join!` 并发「等 1 格空间的写等待」与「先让出一次、
///   再读走 1 字节」。
/// - 判断：写等待最终拿到恰好 1 格空间并写回 `[4]`，环回到满（`data_size == 4`）。
async fn write_async_wakes_on_read_() {
    let (mut tx, mut rx) = make_pair_::<4>().await; // 容量 4

    // 写满 4 字节。
    let demand = Demand::at_least(4);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[1, 2, 3, 4]);
    drop(ws);
    assert_eq!(tx.free_size(), 0, "环应已写满");

    let write = async {
        let demand = Demand::at_least(1);
        let res = tx.write_async(&demand).await;
        let mut ws = res.pick_left().expect("写等待应成功");
        // 容量 4：满环读走 1 格后剩余数据 3、free = 1。
        assert_eq!(ws.least_count(), 1, "读走 1 格后应可写 1 格");
        fill_segm(&mut ws, &[4]);
        drop(ws); // 提交
    };
    let read = async {
        // 先让出一次，确保写者已经挂起并注册 waker。
        futures_lite::future::yield_now().await;
        let demand = Demand::exactly(1);
        let mut rs = TrBuffTryRead::try_read(&mut rx, &demand)
            .pick_left()
            .unwrap();
        assert_eq!(take_segm(&mut rs, 1), vec![1]);
        drop(rs); // 提交 → 释放空间 → 唤醒写者
    };
    let ((), ()) = futures_util::join!(write, read);
    assert_eq!(rx.data_size(), 4, "读走 1 格后又写回 1 格 → 回到满环");
}

dual_runtime_test_!(write_async_wakes_on_read_);

/// 被动端的 `check` 按等待者的需求下限裁决：写入量不足下限时读等待**不得就绪**
/// （demand 门控），达到下限后才完成。
/// - 手段：阶段一用 `select(读等待, 探测)` 并发——`select` 左优先，若门控失效
///   （不足下限也交出读段），读等待会先就绪、`select` 返回左侧而失败；探测先
///   让出一次、只写 2 字节（不足下限 5）后再让出一次并完成，返回右侧即证明此刻
///   仍未就绪。阶段一结束时丢弃该等待 future（取消），再用 `join!` 让**新**读者
///   与「先让出一次、再补写 3 字节」并发，由运行时 waker 唤醒。
/// - 判断：(1) 只写 2 字节时读等待不得就绪；(2) 补足到 5 字节后成功借出 5
///   字节，内容为 `[1, 2, 3, 4, 5]`。
async fn read_async_demand_gates_wakeup_() {
    let (mut tx, mut rx) = make_pair_::<8>().await;

    // 阶段一：读者等 5 字节；只写 2 字节（不足下限）→ 不得就绪。
    {
        let demand_read = Demand::at_least(5);
        let read = rx.read_async(&demand_read).into_future();
        let probe = async {
            // 先让出一次：确保读者已挂起并注册 waker。
            futures_lite::future::yield_now().await;
            let demand = Demand::at_least(2);
            let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
                .pick_left()
                .expect("应可写 2 格");
            fill_segm(&mut ws, &[1, 2]);
            drop(ws); // 提交：5 > 2，按 demand 门控不应让读者就绪
            // 再让出一次，把控制权交给读等待。
            futures_lite::future::yield_now().await;
        };
        futures_util::pin_mut!(read, probe);
        match futures_util::future::select(read, probe).await {
            futures_util::future::Either::Right(_) => {}
            futures_util::future::Either::Left(_) => {
                panic!("不足需求下限时读等待不得就绪（demand 门控失效）")
            }
        }
    } // 取消阶段一的读等待（drop 收尾），释放 tx / rx 借用。

    // 阶段二：补足到 5 字节（累计）→ 新读者应在真实运行时里被唤醒。
    let read = async {
        let demand = Demand::at_least(5);
        let res = rx.read_async(&demand).await;
        let mut rs = res.pick_left().expect("达到下限后读等待应成功");
        assert_eq!(rs.least_count(), 5);
        assert_eq!(take_segm(&mut rs, 5), vec![1, 2, 3, 4, 5]);
    };
    let write = async {
        // 先让出一次，确保读者已经挂起并注册 waker。
        futures_lite::future::yield_now().await;
        let demand = Demand::at_least(3);
        let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
            .pick_left()
            .expect("应可写 3 格");
        fill_segm(&mut ws, &[3, 4, 5]);
        drop(ws); // 提交 → 达到下限 → 唤醒读者
    };
    let ((), ()) = futures_util::join!(read, write);
}

dual_runtime_test_!(read_async_demand_gates_wakeup_);

//! 被动端异步等待（`core_passive_read_async_` / `core_passive_write_async_`
//! 的 park 机制）的单元测试。
//!
//! # 被测约定
//!
//! 被动端在需求（`Demand`）未满足时进入等待：把 waker 注册进端类型的唤醒
//! 槽位（`BufConsumer::wakeslot_` / `BufProducer::wakeslot_`）并返回
//! `Pending`；对端提交触发 hook 唤醒后重查条件。本模块测试等待侧的两个不变量：
//!
//! 1. **park 返回 Pending**：需求不满足（空环读 / 可写空间不足）时，等待
//!    future 必须挂起，且 demand 已登记、waker 已注册；
//! 2. **drop 收尾可重入**：等待 future 被 drop（取消）后，必须注销槽位并
//!    复位 demand——否则下一次 park 的 `try_set_demand`（CAS null → 非空）
//!    会失败（触发「并发调用」断言），或 `WakeSlot::register` 因槽位残留
//!    悬垂指针而自旋。
//!
//! # 构造与判定
//!
//! 用例一律写成 `async fn`，由
//! [`dual_runtime_test_`](crate::test_support_::dual_runtime_test_) 在 **tokio** 与
//! **compio** 两种**真实运行时**下各跑一遍；不 `block_on`、不手动构造 waker。
//!
//! * **park 的观测**：等待 future 无法用 `await` 观察到「挂起」这一中间态，
//!   于是用 `futures_util::future::select(等待, 让出一次)`——`select` 按
//!   **左优先**顺序在同一运行时任务内先轮询等待 future（它因此登记 demand、
//!   注册**运行时提供的** waker 并返回 `Pending`），随后让出侧完成并胜出。
//!   `select` 返回右侧即证明等待 future 此刻确实挂起；返回的未完成 future
//!   随即被 drop（模拟取消、且不唤醒），这正是原先「先 poll 断言 `Pending`
//!   再 drop」的等价形态。
//! * **可重入的观测**：紧接着**再**做一次同样的 park——若 drop 收尾缺失，
//!   第二次 park 会 panic（demand 未复位）或死锁（槽位未注销、`register`
//!   自旋）。
//! * **唤醒的观测**：读侧最后用 `futures_util::join!` 让「读等待」与「先让出
//!   一次、再提交写入」真实并发，断言数据最终送达——waker 由运行时驱动。

use std::vec;

use abs_buff::{Demand, TrBuffTryWrite};

use super::{DefaultBuilder, Pair, fill_segm, take_segm};
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

/// 把等待 future 驱动到「首次挂起」后原样 drop（模拟取消，且不唤醒）。
///
/// 手段：`select(等待, 让出一次)` 左优先——等待 future 必先被轮询一次
/// （登记 demand、注册运行时 waker），让出侧随后完成并胜出。
/// 返回 `true` 表示等待 future 确实处于挂起态（未被唤醒、未就绪）；
/// 未完成的 future 在 `select` 结果离开作用域时被 drop。
async fn park_then_drop_<F>(fut: F) -> bool
where
    F: core::future::Future,
{
    let stop = async { futures_lite::future::yield_now().await };
    futures_util::pin_mut!(fut, stop);
    matches!(
        futures_util::future::select(fut, stop).await,
        futures_util::future::Either::Right(_)
    )
}

/// 读侧 park：空环上 `read_async` 无法满足需求（`Drained` 且未关闭）时必须
/// 挂起；drop（取消）后必须能**再次** park；随后对端写入要能通过运行时 waker
/// 把它唤醒并交付数据。
/// - 手段：连续两次「park 后 drop」，再用 `join!` 让第三次读等待与「先让出
///   一次、再写入 1 字节」并发。
/// - 判断：(1) 两次 park 都挂起——第二次成功即证明 drop 收尾注销了槽位并复位
///   demand（否则 panic 或死锁）；(2) 第三次读到 `[42]`。
async fn read_async_parks_on_empty_and_reparks_after_drop_() {
    let (mut tx, mut rx) = make_pair_::<8>().await;
    let demand = Demand::at_least(1);

    // 第一次 park：空环 → 必须挂起；随后原样 drop（取消，且不唤醒）。
    assert!(
        park_then_drop_(rx.read_async(&demand).into_future()).await,
        "空环上读等待必须挂起（需求未满足）"
    );

    // 第二次 park：仍应挂起，说明第一次的收尾完整。
    assert!(
        park_then_drop_(rx.read_async(&demand).into_future()).await,
        "drop 后再次读等待仍应能 park（demand 已复位、槽位已注销）"
    );

    // 第三次：真正由对端写入唤醒——验证 park 之后确实收到数据。
    let read = async {
        let res = rx.read_async(&demand).await;
        let mut rs = res.pick_left().expect("写入后读等待应成功");
        assert_eq!(take_segm(&mut rs, 1), vec![42]);
    };
    let write = async {
        // 先让出一次，确保读者已经挂起并注册 waker。
        futures_lite::future::yield_now().await;
        let demand = Demand::at_least(1);
        let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
            .pick_left()
            .expect("应可写");
        fill_segm(&mut ws, &[42]);
        drop(ws); // 提交 → 触发消费端 hook 唤醒读者
    };
    let ((), ()) = futures_util::join!(read, write);
}

dual_runtime_test_!(read_async_parks_on_empty_and_reparks_after_drop_);

/// 写侧 park：可写空间不足需求下限（`Stuffed` 且未关闭）时 `write_async` 必须
/// 挂起；drop（取消）后必须能**再次** park。
/// - 手段：容量 8（可写空间 8）上请求 `Demand::at_least(9)`（下限超过容量，
///   永远无法在本地满足，因此只观察 park / 可重入），连续两次「park 后 drop」。
/// - 判断：两次 park 都挂起——第二次成功即验证写侧 drop 收尾（注销槽位 +
///   复位 demand）完整。
async fn write_async_parks_when_space_insufficient_and_reparks_after_drop_() {
    let (mut tx, mut _rx) = make_pair_::<8>().await;
    let demand = Demand::at_least(9);

    // 第一次 park：free = 8 < 9 → 必须挂起；随后原样 drop（取消，且不唤醒）。
    assert!(
        park_then_drop_(tx.write_async(&demand).into_future()).await,
        "可写空间不足下限时写等待必须挂起"
    );

    // 第二次 park：仍应挂起，说明第一次的收尾完整。
    assert!(
        park_then_drop_(tx.write_async(&demand).into_future()).await,
        "drop 后再次写等待仍应能 park"
    );
}

dual_runtime_test_!(write_async_parks_when_space_insufficient_and_reparks_after_drop_);

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
//!    future 必须挂起（`Poll::Pending`），且 demand 已登记、waker 已注册；
//! 2. **drop 收尾可重入**：等待 future 被 drop（取消）后，必须注销槽位并
//!    复位 demand——否则下一次 park 的 `try_set_demand`（CAS null → 非空）
//!    会失败（触发「并发调用」断言），或 `WakeSlot::register` 因槽位残留
//!    悬垂指针而自旋。
//!
//! # 构造与判定
//!
//! * **构造**：经公共半部链（`Consumer::read_async` / `Producer::write_async`
//!   → `gen_may_cancel_future` 生成的 future → `core_passive_*_async_`）驱动
//!   被测函数；用 [`poll_once`] 轮询、[`TestWaker`] 提供 waker。
//! * **判定**：第一次 park 必须 `Pending`；drop 后**再次 park 必须同样
//!   `Pending`**——若收尾缺失，第二次 park 会 panic（demand 未复位）或死锁
//!   （槽位未注销、`register` 自旋），测试即失败。连续两次 poll（spurious
//!   唤醒）也必须保持 `Pending` 且不报错（`register` 幂等）。

use std::pin::pin;

use abs_buff::Demand;

use super::{
    DefaultBuilder, TestWaker, Pair, poll_once,
};

/// 构建一个容量 `N` 的被动 × 被动半部对（测试辅助）。
///
/// `build` 已改为异步（`build_async`）：同步测试用
/// `futures_lite::future::block_on` 驱动构建 future 到完成。
fn make_pair<const N: usize>() -> Pair {
    let mut ready = DefaultBuilder::with_capacity(N)
        .unwrap()
        .producer_passive()
        .consumer_passive();
    futures_lite::future::block_on(ready.build_async().into_future()).unwrap()
}

/// # 被测约定
/// 读侧 park：空环上 `read_async` 无法满足需求（`Drained` 且未关闭），必须
/// 挂起为 `Pending`——demand 登记进 `BufConsumer`、waker 注册进其唤醒槽位；
/// 对端写入提交触发 hook 后才会被唤醒。
///
/// # 构造
/// 空环（`cap = 8`，未写入任何数据）上请求 `Demand::at_least(1)`；经
/// `rx.read_async(&demand)` 的完整生成 future 链驱动到 `core_passive_read_async_`。
/// 轮询一次断言挂起；随后**不唤醒直接 drop**（模拟取消）；再新建一个等待
/// future 重复同样的 park。
///
/// # 判定
/// (1) 第一次 poll 为 `Pending`；(2) drop 后再 park 仍为 `Pending` 且不
/// panic / 不死锁——这验证 drop 收尾确实注销了槽位并复位了 demand
/// （否则第二次 `try_set_demand` 触发 `unreachable!`，或 `register` 因残留
/// 指针自旋）。
#[test]
fn read_async_parks_on_empty_and_reparks_after_drop() {
    let (mut _tx, mut rx) = make_pair::<8>();

    // 第一次等待：空环 → Pending。用内层作用域让 future（含 pin! 的隐藏
    // 局部）在离开作用域时被 drop，从而触发守卫的 drop 收尾。
    let demand = Demand::at_least(1);
    {
        let fut = rx.read_async(&demand);
        let mut fut = pin!(fut.into_future());
        let (waker, _flag) = TestWaker::make_waker_tuple();
        assert!(
            poll_once(fut.as_mut(), &waker).is_pending(),
            "空环上读等待必须挂起（需求未满足）"
        );
        // spurious 轮询：waker 未变、条件未变，仍应 Pending（register 幂等）。
        assert!(
            poll_once(fut.as_mut(), &waker).is_pending(),
            "条件未变时重复轮询仍应 Pending"
        );
    } // 取消：守卫注销槽位 + 复位 demand。

    // 第二次等待：能再次 park，说明第一次的收尾完整。
    let demand = Demand::at_least(1);
    {
        let fut = rx.read_async(&demand);
        let mut fut = pin!(fut.into_future());
        let (waker, _flag) = TestWaker::make_waker_tuple();
        assert!(
            poll_once(fut.as_mut(), &waker).is_pending(),
            "drop 后再次读等待仍应能 park（demand 已复位、槽位已注销）"
        );
    }
}

/// # 被测约定
/// 写侧 park：可写空间不足需求下限（`Stuffed` 且未关闭）时，`write_async`
/// 必须挂起为 `Pending`——demand 登记进 `BufProducer`、waker 注册进其唤醒
/// 槽位；对端读取提交释放空间后触发 hook 才会被唤醒。
///
/// # 构造
/// 空环（`cap = 8`，可写空间 8）上请求 `Demand::at_least(9)`——下限超过
/// 当前可写空间，`try_write_at` 返回 `Stuffed`（非终止错误），进入 park。
/// 经 `tx.write_async(&demand)` 的完整生成 future 链驱动到
/// `core_passive_write_async_`；轮询断言挂起后 drop，再重复一次 park。
///
/// # 判定
/// 同读侧：(1) 第一次 poll 为 `Pending`；(2) drop 后再次 park 仍为 `Pending`
/// 且不 panic / 不死锁——验证写侧 drop 收尾（注销槽位 + 复位 demand）完整。
#[test]
fn write_async_parks_when_space_insufficient_and_reparks_after_drop() {
    let (mut tx, mut _rx) = make_pair::<8>();

    // 第一次等待：free=8 < 9 → Pending。
    let demand = Demand::at_least(9);
    {
        let fut = tx.write_async(&demand);
        let mut fut = pin!(fut.into_future());
        let (waker, _flag) = TestWaker::make_waker_tuple();
        assert!(
            poll_once(fut.as_mut(), &waker).is_pending(),
            "可写空间不足下限时写等待必须挂起"
        );
    } // 取消：守卫注销槽位 + 复位 demand。

    // 第二次等待：能再次 park，说明第一次的收尾完整。
    let demand = Demand::at_least(9);
    {
        let fut = tx.write_async(&demand);
        let mut fut = pin!(fut.into_future());
        let (waker, _flag) = TestWaker::make_waker_tuple();
        assert!(
            poll_once(fut.as_mut(), &waker).is_pending(),
            "drop 后再次写等待仍应能 park"
        );
    }
}

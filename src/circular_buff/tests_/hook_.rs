//! 关闭 / EOF 事件与 hook 行为的测试：
//!
//! * 生产者关闭 → 消费端 hook 收到 `ProducerClose`，读者获得 EOF 语义
//!   （剩余数据可读，读空后返回 `Closing`）；
//! * 消费者关闭 → 生产端 hook 收到 `ConsumerClose`，写者感知对端关闭；
//! * 关闭后**新的**异步读等待立即返回 `Closing`；「唤醒已在 park 的读者」这条
//!   （库侧 `BufConsumer::check` 已修）需要运行时定时器才能诚实地测，归入
//!   `tests/` 下基于 `abs_art-bridge` 的集成用例。
//!
//! # 运行时与驱动方式
//!
//! 需要 `build_async` / `close_async` / `read_async` 的用例一律写成 `async fn`，
//! 由 [`dual_runtime_test_`](crate::test_support_::dual_runtime_test_) 在 **tokio**
//! 与 **compio** 两种**真实运行时**下各跑一遍。原来「手动 poll 出 `Pending`、
//! 再 poll 出 `Ready`」的写法改成 `futures_util::join!` / `select` 的真实并发：
//! 一边等待、一边推进另一端（先 `yield_now` 让等待侧先撞上等待条件），断言最终
//! 可观测结果。

use std::vec;

use abs_buff::{Demand, TrBuffTryRead, TrBuffTryWrite};

use super::{
    super::ConsumerError,
    DefaultBuilder, Pair, fill_segm, take_segm,
};
use crate::test_support_::dual_runtime_test_;

/// 构建被动 × 被动半部对（测试辅助，见 [`super::sync_`] 的说明）。
///
/// `build_async` 在真实运行时里被 `.await` 驱动到完成。
async fn make_pair_<const N: usize>() -> Pair {
    let mut ready = DefaultBuilder::with_capacity(N)
        .unwrap()
        .producer_passive()
        .consumer_passive();
    ready.build_async().await.unwrap()
}

/// 生产者关闭（EOF）：触发消费端 hook `ProducerClose`；读者可读尽剩余数据，
/// 读空后返回 `Closing`。
/// - 手段：写 2 字节后 `close`，先按「下限 5」再按「下限 1」各读一次。
/// - 判断：不足下限时仍返回剩余 2 字节（EOF 例外）；读空后返回 `Closing`。
async fn producer_close_gives_eof_() {
    let (mut tx, mut rx) = make_pair_::<8>().await;

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

dual_runtime_test_!(producer_close_gives_eof_);

/// 消费者关闭：触发生产端 hook `ConsumerClose`；写者感知对端关闭，
/// 但仍可继续写入（数据无人消费，环满即止）。
/// - 手段：`rx.close_async().await` 后检查写端状态并尝试写入。
/// - 判断：`is_consumer_closed()` 为真；写端仍能借出写段。
async fn consumer_close_fires_event_() {
    let (mut tx, mut rx) = make_pair_::<8>().await;

    rx.close_async().await;
    assert!(tx.is_consumer_closed(), "写者应感知消费者关闭");

    // 关闭后写者仍可写（直到写满）。
    let demand = Demand::at_least(2);
    let some = TrBuffTryWrite::try_write(&mut tx, &demand);
    assert!(some.pick_left().is_some(), "消费者关闭后写者仍可写入");
}

dual_runtime_test_!(consumer_close_fires_event_);

/// 异步读等待在生产者关闭后应尽快以 `Closing`（EOF）结束，而不是永远等待。
/// - 手段：(1) 空环上发起 `read_async`，用 `select(读等待, 让出一次)` 左优先
///   证明它确实挂起（需求未满足、waker 已注册），随后 drop（取消）；(2)
///   `tx.close()` 后新建一个读等待，用同样的 `select` 证明它在首个轮询即就绪，
///   并断言结果为 `ConsumerError::Closing`。
/// - 判断：park 确实发生；关闭后新读等待立即返回 `Closing`。
///
/// # 已知限制：关闭事件**不唤醒**已在 park 的被动读者（未改库代码）
///
/// 本条用例**无法**在真实运行时里表达「已经在等待的那个读者被 `close()` 唤醒」
/// ——该唤醒在库侧根本没有发出：`BufConsumer::check` 对
/// `ProducerClose` / `ConsumerClose` 事件走的是
/// `let ConsumerHookEvent::Available(..) = event else { return true }` 分支，
/// **直接返回而未调用 `wakeslot_.signal()`**（对照同函数的 `Available` 分支以及
/// `DevConsumer::check`）。于是 `Producer::close()` 只置位关闭标志，不会 signal
/// 挂在唤醒槽位上的被动等待者；`core_passive_read_async_` 的 `poll_fn` 因此不会
/// 被重新轮询，读者会一直挂起。旧用例之所以「通过」，只是因为它无条件地又手动
/// `poll` 了一次。
///
/// 本次改造**不修改库代码**（`src/circular_buff/**` 非 `tests_` 文件），故这里只
/// 钉住可观测的 EOF 语义。修复 `check`（关闭事件也 `signal`）后，本用例可进一步
/// 改回 `join!`：让 park 中的读者与「先让出一次、再 `close`」并发并断言读者被
/// 唤醒为 `Closing`。
async fn read_async_returns_closing_on_eof_() {
    let (mut tx, mut rx) = make_pair_::<8>().await;

    // (1) 空环上读等待必须 park。
    {
        let demand = Demand::at_least(3);
        let read = rx.read_async(&demand).into_future();
        let stop = async { futures_lite::future::yield_now().await };
        futures_util::pin_mut!(read, stop);
        assert!(
            matches!(
                futures_util::future::select(read, stop).await,
                futures_util::future::Either::Right(_)
            ),
            "空环上读等待必须挂起（需求未满足）"
        );
    } // 取消该读者（drop 收尾）。

    // (2) 关闭生产者后，新读等待必须立即得到 EOF 语义（而非永远等待）。
    //
    // 注意：这里**只**能证明「关闭后重新发起读等待会看到 EOF」。要证明「已在 park
    // 的读者被 close() 的 signal 唤醒」是另一回事——那要求读者在被关闭之后**只能**
    // 靠那次 signal 被重新轮询。用 `yield_now` 循环当兜底会把整条 select 一起唤醒，
    // 于是又变成「手撸轮询」，会把缺陷盖住（实测：即便撤掉修复，那种写法依然通过）。
    // 真正的回归用例需要**运行时的定时器**做超时（超时即失败，且不引入额外唤醒源），
    // 那属于集成测试层：见 `tests/` 下基于 `abs_art-bridge` 的双后端用例。
    tx.close();
    assert!(rx.is_producer_closed(), "读者应感知生产者关闭（EOF）");

    {
        let demand = Demand::at_least(3);
        let read = rx.read_async(&demand).into_future();
        let probe = async { futures_lite::future::yield_now().await };
        futures_util::pin_mut!(read, probe);
        let res = match futures_util::future::select(read, probe).await {
            futures_util::future::Either::Left((res, _)) => res,
            futures_util::future::Either::Right(_) => {
                panic!("空环 + 生产者关闭：读等待不得永远等待")
            }
        };
        assert!(
            matches!(res.pick_right(), Some(ConsumerError::Closing)),
            "空环 + 生产者关闭：读等待应返回 Closing"
        );
    }
}

dual_runtime_test_!(read_async_returns_closing_on_eof_);

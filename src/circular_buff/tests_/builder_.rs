//! 构建器**顺序灵活性**的测试：生产端 / 消费端可以任意顺序设置
//! （`pipe_from_input` / `pipe_into_output` / `producer_passive` /
//! `consumer_passive`），可以一步同时设置两端（`pipe_between`），也可以两端
//! 都不设置、直接 `build()` 得到双端被动（经典手动管道）。
//!
//! 主动端**不产出半部**：`build` 的返回类型由两端模式决定——双端被动 →
//! [`SpscPair`]；主动生产 × 被动消费 → 仅消费端半部；被动生产 × 主动消费 →
//! 仅生产端半部；主动 × 主动 → `()`。
//!
//! 等价性断言建立在 `pump_` / `sync_` 已有行为之上——换序与 `pipe_between`
//! 必须产生与原有链完全一致的结果。
//!
//! # 运行时与驱动方式
//!
//! 需要 `build_async` 的用例一律写成 `async fn`，由
//! [`dual_runtime_test_`](crate::test_support_::dual_runtime_test_) 在 **tokio**
//! 与 **compio** 两种**真实运行时**下各跑一遍；`.await` 直接使用，不再
//! `block_on`，也不再手动构造 waker 轮询。
//!
//! 全主动流水线（`Pipeline`）是**永不结束**的设备驱动 future，无法被 `await`
//! 到完成：这里用 `futures_util::future::select` 把它与一个「让出若干次」的
//! 探测 future 并发，让运行时按 `select` 的**左优先**顺序反复轮询流水线；
//! 探测 future 完成即代表搬运窗口已经给足，随后断言最终可观测结果。

use std::{
    sync::atomic::Ordering,
    vec,
    vec::Vec,
};

use abs_buff::{Demand, TrBuffTryRead, TrBuffTryWrite};

use crate::circular_buff::{
    builder,
    tests_::{
        DefaultBuilder, Pair, TestInput, TestOutput, fill_segm, take_segm,
    },
};
use crate::test_support_::dual_runtime_test_;

/// 默认双端被动：不 pipe 任何设备，`with_capacity(...).build_async()` 直接得到
/// 一对可用的 Producer / Consumer（经典手动管道）。
/// - 手段：容量 8 构建后写 3 字节、再读 3 字节。
/// - 判断：读出字节与写入一致，且读写后 `data_size` 分别为 3 / 0。
async fn build_without_pipe_defaults_to_passive_pair_() {
    let pair: Pair = DefaultBuilder::with_capacity(8)
        .unwrap()
        .build_async()
        .await
        .unwrap();

    let (mut tx, mut rx) = pair;
    assert_eq!(tx.capacity(), 8);

    // 写 3 字节 → 读回同样的 3 字节（被动 × 被动语义完整）。
    let demand = Demand::at_least(3);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[1, 2, 3]);
    drop(ws);
    assert_eq!(rx.data_size(), 3);

    let demand = Demand::at_least(3);
    let mut rs = TrBuffTryRead::try_read(&mut rx, &demand)
        .pick_left()
        .unwrap();
    assert_eq!(take_segm(&mut rs, 3), vec![1, 2, 3]);
    drop(rs);
    assert_eq!(rx.data_size(), 0);
}

dual_runtime_test_!(build_without_pipe_defaults_to_passive_pair_);

/// 显式双端被动，但**消费端先设、生产端后设**（与传统的
/// `producer_passive().consumer_passive()` 顺序对调）。
/// - 手段：`consumer_passive().producer_passive().build_async()` 后写 2 读 2。
/// - 判断：读出顺序与写入一致——换序不影响被动 × 被动语义。
async fn consumer_first_then_producer_passive_() {
    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .consumer_passive()
        .producer_passive();
    let pair: Pair = ready.build_async().await.unwrap();
    let (mut tx, mut rx) = pair;

    let demand = Demand::at_least(2);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[7, 8]);
    drop(ws);
    let demand = Demand::at_least(2);
    let mut rs = TrBuffTryRead::try_read(&mut rx, &demand)
        .pick_left()
        .unwrap();
    assert_eq!(take_segm(&mut rs, 2), vec![7, 8]);
    drop(rs);
}

dual_runtime_test_!(consumer_first_then_producer_passive_);

/// 消费端先行（`pipe_into_output`）、生产端后设（`pipe_from_input`）：
/// 与 `pipe_from_input(...).pipe_into_output(...)` 完全等价——由设备驱动把
/// 输入全部流到输出。两端主动 → `build_async` 返回 [`Pipeline`] future。
/// - 手段：把永不结束的流水线 future 与「让出若干次」的探测 future 用
///   `select` 并发，由运行时真实轮询（左优先）驱动流水线。
/// - 判断：输出设备收到 0..20 且输入设备被读走 20 字节。
async fn pipe_into_output_then_pipe_from_input_() {
    let input = TestInput::new((0..20).collect());
    let pos = input.pos.clone();
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_into_output(output)
        .pipe_from_input(input);
    let mut pipeline = ready.build_async().await.unwrap();

    let drive = pipeline.pipe_async().into_future();
    let stop = async {
        for _ in 0..64 {
            futures_lite::future::yield_now().await;
            if pos.load(Ordering::Relaxed) == 20 {
                return;
            }
        }
    };
    futures_util::pin_mut!(drive, stop);
    let _ = futures_util::future::select(drive, stop).await;

    assert_eq!(*out_data.lock().unwrap(), (0..20).collect::<Vec<_>>());
    assert_eq!(pos.load(Ordering::Relaxed), 20);
}

dual_runtime_test_!(pipe_into_output_then_pipe_from_input_);

/// `pipe_between(input, output)`：一步同时设置两端，等价于两段式全主动
/// 流水线。两端主动 → `build_async` 返回 [`Pipeline`] future。
/// - 手段：同 [`pipe_into_output_then_pipe_from_input_`]，用 `select` + 让出驱动。
/// - 判断：输出设备收到 0..20 且输入设备被读走 20 字节。
async fn pipe_between_builds_active_pipeline_() {
    let input = TestInput::new((0..20).collect());
    let pos = input.pos.clone();
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_between(input, output);
    let mut pipeline = ready.build_async().await.unwrap();

    let drive = pipeline.pipe_async().into_future();
    let stop = async {
        for _ in 0..64 {
            futures_lite::future::yield_now().await;
            if pos.load(Ordering::Relaxed) == 20 {
                return;
            }
        }
    };
    futures_util::pin_mut!(drive, stop);
    let _ = futures_util::future::select(drive, stop).await;

    assert_eq!(*out_data.lock().unwrap(), (0..20).collect::<Vec<_>>());
    assert_eq!(pos.load(Ordering::Relaxed), 20);
}

dual_runtime_test_!(pipe_between_builds_active_pipeline_);

/// 消费端先行（`pipe_into_output`）+ 生产端被动（`producer_passive`）：
/// 与 `producer_passive().pipe_into_output(...)` 顺序对调的等价形态——
/// 写入缓冲的数据由提交路径（`advance_write`）驱动主动消费者**立即排空**到
/// 输出设备。主动消费端 → 只返回生产端半部。
/// - 手段：构建后一次写入 3 字节并提交。
/// - 判断：输出设备立即拿到 `[1, 2, 3]`，且缓冲被排空。
async fn pipe_into_output_then_passive_producer_() {
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_into_output(output)
        .producer_passive();
    let mut tx = ready.build_async().await.unwrap();

    // 写 3 字节：段 drop 提交 → advance_write 驱动输出泵 → 立即排空。
    let demand = Demand::at_least(3);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[1, 2, 3]);
    drop(ws);
    assert_eq!(*out_data.lock().unwrap(), vec![1, 2, 3], "写入提交即排空");
    assert_eq!(tx.data_size(), 0);
}

dual_runtime_test_!(pipe_into_output_then_passive_producer_);

/// 消费端先行（`consumer_passive`）+ 生产端主动（`pipe_from_input`）：
/// 与 `pipe_from_input(...).consumer_passive()` 顺序对调的等价形态——
/// 构造完成即已泵入，消费驱动补位。主动生产端 → 只返回消费端半部。
/// - 手段：构建（`build_async` 的初始搬运）后读空整个缓冲。
/// - 判断：构造完成即填满 8 格、输入被读走 8 字节；读空后累计字节为 0..10。
async fn consumer_passive_then_pipe_from_input_() {
    let input = TestInput::new((0..10).collect());
    let pos = input.pos.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .consumer_passive()
        .pipe_from_input(input);
    let mut rx = ready.build_async().await.unwrap();

    // 构造完成即已泵入：容量 8 全部可用（REVERSION 约定）→ 填满 8 格。
    assert_eq!(rx.data_size(), 8);
    assert_eq!(pos.load(Ordering::Relaxed), 8);

    let mut total = Vec::new();
    loop {
        let demand = Demand::at_least(1);
        let some = TrBuffTryRead::try_read(&mut rx, &demand);
        let mut rs = match some.pick_left() {
            Some(s) => s,
            None => break,
        };
        let n = rs.least_count();
        total.extend(take_segm(&mut rs, n));
        drop(rs);
    }
    assert_eq!(total, (0..10).collect::<Vec<_>>());
    assert_eq!(pos.load(Ordering::Relaxed), 10);
}

dual_runtime_test_!(consumer_passive_then_pipe_from_input_);

/// 容量校验仍然生效：`with_capacity` 立即执行容量区间检查
/// （`MIN_CAPACITY` = 2 ..= `MAX_CAPACITY` = `(1 << 28) - 1`），越界返回
/// [`BuilderError`]。
///
/// 纯同步用例（不涉及任何异步 API），保持 `#[test]`。
/// - 手段：以 0、1 与 `1 << 28` 构造构建器。
/// - 判断：分别返回 `SizeTooSmall(0)` / `SizeTooSmall(1)` / `SizeTooBig`。
#[test]
fn build_default_still_validates_capacity() {
    let r0 = DefaultBuilder::with_capacity(0);
    assert!(matches!(r0, Err(builder::BuilderError::SizeTooSmall(0))));

    let r1 = DefaultBuilder::with_capacity(1);
    assert!(matches!(r1, Err(builder::BuilderError::SizeTooSmall(1))));

    let too_big = 1usize << 28; // 超出 POS_MASK
    let rb = DefaultBuilder::with_capacity(too_big);
    assert!(matches!(
        rb,
        Err(builder::BuilderError::SizeTooBig(c)) if c == too_big
    ));
}

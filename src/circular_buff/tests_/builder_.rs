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

use std::{
    mem::MaybeUninit,
    pin::pin,
    sync::atomic::Ordering,
    vec,
    vec::Vec,
};

use abs_buff::{Demand, TrBuffTryRead, TrBuffTryWrite,};
use abs_mm::mem_alloc::CoreAlloc;
use mm_ptr::Owned;

use crate::circular_buff::{
    ReclSliceRef,
    builder,
    core_::CircCore,
    reclaim_::ReaderReclaim,
    tests_::{
        DefaultBuilder, Pair, TestInput, TestOutput, TestWaker, fill_segm,
        poll_once, take_segm,
    },
};
use crate::x_deps::{abs_mm, mm_ptr};

/// 默认双端被动：不 pipe 任何设备，`with_capacity(...).build_async()` 直接得到
/// 一对可用的 Producer / Consumer（经典手动管道）。
///
/// `build` 已改为异步（`build_async`）：同步 `#[test]` 用
/// `futures_lite::future::block_on` 驱动构建 future 到完成。
#[test]
fn build_without_pipe_defaults_to_passive_pair() {
    let pair: Pair = futures_lite::future::block_on(
        DefaultBuilder::with_capacity(8).unwrap().build_async(),
    )
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

/// 显式双端被动，但**消费端先设、生产端后设**（与传统的
/// `producer_passive().consumer_passive()` 顺序对调）。
#[test]
fn consumer_first_then_producer_passive() {
    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .consumer_passive()
        .producer_passive();
    let pair: Pair =
        futures_lite::future::block_on(ready.build_async().into_future()).unwrap();
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

/// 消费端先行（`pipe_into_output`）、生产端后设（`pipe_from_input`）：
/// 与 `pipe_from_input(...).pipe_into_output(...)` 完全等价——首个 poll 即由
/// 设备驱动把输入全部流到输出。两端主动 → `build_async` 返回
/// [`Pipeline`] future。
#[test]
fn pipe_into_output_then_pipe_from_input() {
    let input = TestInput::new((0..20).collect());
    let pos = input.pos.clone();
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_into_output(output)
        .pipe_from_input(input);
    let mut pipeline =
        futures_lite::future::block_on(ready.build_async().into_future()).unwrap();

    let (waker, _wake_flag) = TestWaker::make_waker_tuple();
    let fut = pipeline.pipe_async().into_future();
    let mut pinned = pin!(fut);
    let _ = poll_once(pinned.as_mut(), &waker);

    assert_eq!(*out_data.lock().unwrap(), (0..20).collect::<Vec<_>>());
    assert_eq!(pos.load(Ordering::Relaxed), 20);
}

/// `pipe_between(input, output)`：一步同时设置两端，等价于两段式全主动
/// 流水线。两端主动 → `build_async` 返回 [`Pipeline`] future。
#[test]
fn pipe_between_builds_active_pipeline() {
    let input = TestInput::new((0..20).collect());
    let pos = input.pos.clone();
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_between(input, output);
    let mut pipeline =
        futures_lite::future::block_on(ready.build_async().into_future()).unwrap();

    let (waker, _wake_flag) = TestWaker::make_waker_tuple();
    let fut = pipeline.pipe_async().into_future();
    let mut pinned = pin!(fut);
    let _ = poll_once(pinned.as_mut(), &waker);

    assert_eq!(*out_data.lock().unwrap(), (0..20).collect::<Vec<_>>());
    assert_eq!(pos.load(Ordering::Relaxed), 20);
}

/// 消费端先行（`pipe_into_output`）+ 生产端被动（`producer_passive`）：
/// 与 `producer_passive().pipe_into_output(...)` 顺序对调的等价形态——
/// 写入缓冲的数据由提交路径（`advance_write`）驱动主动消费者**立即排空**到
/// 输出设备。主动消费端 → 只返回生产端半部。
#[test]
fn pipe_into_output_then_passive_producer() {
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_into_output(output)
        .producer_passive();
    let mut tx =
        futures_lite::future::block_on(ready.build_async().into_future()).unwrap();

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

/// 消费端先行（`consumer_passive`）+ 生产端主动（`pipe_from_input`）：
/// 与 `pipe_from_input(...).consumer_passive()` 顺序对调的等价形态——
/// 构造完成即已泵入，消费驱动补位。主动生产端 → 只返回消费端半部。
#[test]
fn consumer_passive_then_pipe_from_input() {
    let input = TestInput::new((0..10).collect());
    let pos = input.pos.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .consumer_passive()
        .pipe_from_input(input);
    let mut rx =
        futures_lite::future::block_on(ready.build_async().into_future()).unwrap();

    // 构造完成即已泵入：容量 8 全部可用（REVERSION 约定）→ 填满 8 格。
    assert_eq!(rx.data_size(), 8);
    assert_eq!(pos.load(Ordering::Relaxed), 8);

    let mut total = Vec::new();
    loop {
        let demand = Demand::at_least(1);
        let some = TrBuffTryRead::try_read(&mut rx, &demand);
        let mut rs: ReclSliceRef<'_, u8,
            ReaderReclaim<'_, CircCore<_, _, Owned<[MaybeUninit<u8>], CoreAlloc>>>> =
            match some.pick_left() {
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

/// 容量校验仍然生效：`with_capacity` 立即执行容量区间检查
/// （`MIN_CAPACITY` = 2 ..= `MAX_CAPACITY` = `(1 << 28) - 1`），越界返回
/// [`BuilderError`]。
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

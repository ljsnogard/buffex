//! `CircularBuff` 的测试模块。
//!
//! * [`sync_`]——被动 × 被动：读写往返、`Demand` 语义、跨末端环绕、异步等待；
//! * [`pump_`]——主动模式：输入泵、输出泵、全主动流水线；
//! * [`socket_pump_`]——真实 tokio socket 接入：被动生产 × 主动消费的数据滞留
//!   （`#[ignore]`）、全被动环 + 调用方泵（可用）、主动生产 × 被动消费的泵停滞
//!   （`#[ignore]`）；
//! * [`hook_`]——关闭 / EOF 事件与被动唤醒；
//! * [`builder_`]——构建器顺序灵活性：两端任意换序、`pipe_between`、
//!   默认双端被动；
//! * [`pos_tests_`]——`IoPos` 位置状态（REVERSION 约定）的单元测试。
//!
//! 本文件提供测试共用的辅助：测试设备（[`TestInput`] / [`TestOutput`]）、
//! 段操作（[`fill_segm`] / [`take_segm`]）与最小执行器。

mod builder_;
mod hook_;
mod park_tests_;
mod pos_tests_;
mod pump_;
mod socket_pump_;
mod sync_;

use core::{mem::MaybeUninit, pin::Pin};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    vec::Vec,
};

use abs_buff::buffer::{TrBuffSegmMut, TrBuffSegmRef, TrReclaim};

use mm_ptr::Owned;

use super::{
    builder,
    CoreAlloc, ReclSliceMut, ReclSliceRef, SpscPair,
};

/// 被动 × 被动 `build` 产出的半部对（元素 `u8`、分配器 `CoreAlloc` 的具体类型）。
pub(super) type Pair = SpscPair<Owned<[MaybeUninit<u8>], CoreAlloc>>;

/// 测试用构建器：默认缓冲（`Owned`）+ 默认分配器（`CoreAlloc`）、元素 `u8`。
///
/// 显式给出缓冲类型参数 `B`，避免 `CircularBuffBuilder::with_capacity` 的
/// 类型推断在半部链（`producer_passive` / `consumer_passive`）上无法确定 `B`。
pub(super) type DefaultBuilder =
   builder::CircularBuffBuilder<Owned<[MaybeUninit<u8>], CoreAlloc>>;

// ---------------------------------------------------------------------------
// 测试设备（TrInput / TrOutput）
// ---------------------------------------------------------------------------

// 测试设备直接复用 abs_buff 的实现，避免各 crate 各抄一份后逐渐漂移
// （此前本文件的 `ReadySegm` 就停留在 `abs_cancel` v0.1 的 trait 形态上）：
// * `ReadySegm` 是 abs_buff 的正式公开类型（立即就绪的 `SomeOf` future）；
// * `TestErr` / `TestInput` / `TestOutput` 来自 abs_buff 的共享测试模块，
//   由 dev-dependencies 里的 `segm-tests` feature 打开。
pub(super) use abs_buff::ReadySegm;
pub(super) use abs_buff_testkit::{TestErr, TestInput, TestOutput};

// ---------------------------------------------------------------------------
// 段操作辅助（两段式 ReclSliceMut / ReclSliceRef）
// ---------------------------------------------------------------------------

/// 把 `data` 全部写入写段（经 `move_items_from_buff`，u8 位拷贝）。
pub(super) fn fill_segm<R>(segm: &mut ReclSliceMut<'_, u8, R>, data: &[u8])
where
    R: TrReclaim,
{
    assert!(
        data.len() <= segm.least_count(),
        "fill: len({}) > segm({})",
        data.len(),
        segm.least_count()
    );
    let mut staging: Vec<MaybeUninit<u8>> =
        data.iter().map(|&b| MaybeUninit::new(b)).collect();
    // SAFETY: 测试数据为 u8，位拷贝搬入段中，staging 无剩余需 drop 的内容。
    let moved = TrBuffSegmMut::move_items_from_buff(segm, &mut staging);
    assert_eq!(moved, data.len());
}

/// 从读段取出 `len` 个单元（经 `move_items_to_buff`）；段 drop 时读位置
/// 推进 `len`。
pub(super) fn take_segm<R>(
    segm: &mut ReclSliceRef<'_, u8, R>,
    len: usize,
) -> Vec<u8>
where
    R: TrReclaim,
{
    assert!(
        len <= segm.least_count(),
        "take: len({}) > segm({})",
        len,
        segm.least_count()
    );
    let mut dst: Vec<MaybeUninit<u8>> = Vec::with_capacity(len);
    dst.resize(len, MaybeUninit::uninit());
    // SAFETY: 测试数据为 u8，位拷贝搬出安全。
    let moved = TrBuffSegmRef::move_items_to_buff(segm, &mut dst);
    assert_eq!(moved, len);
    dst.into_iter()
        .map(|m| unsafe { m.assume_init() })
        .collect()
}

// ---------------------------------------------------------------------------
// 最小执行器（异步等待测试用）
// ---------------------------------------------------------------------------

/// 测试 waker：唤醒时置位一个 `AtomicBool`。
pub(super) struct TestWaker(Arc<AtomicBool>);

impl TestWaker {
    /// 创建 waker 与其唤醒标志（测试轮询后检查标志以确认被唤醒）。
    pub(super) fn make_waker_tuple() -> (Waker, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let waker = Waker::from(Arc::new(TestWaker(flag.clone())));
        (waker, flag)
    }
}

impl Wake for TestWaker {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

/// 轮询一次 future：返回其 `Poll` 结果（配合 [`TestWaker`] 检查唤醒）。
pub(super) fn poll_once<F: core::future::Future>(
    fut: Pin<&mut F>,
    waker: &Waker,
) -> Poll<F::Output> {
    let mut cx = Context::from_waker(waker);
    fut.poll(&mut cx)
}

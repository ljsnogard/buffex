//! `CircularBuff` 的测试模块。
//!
//! * [`sync_`]——被动 × 被动：读写往返、`Demand` 语义、跨末端环绕、异步等待；
//! * [`pump_`]——主动模式：输入泵、输出泵、全主动流水线；
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
mod sync_;

use core::{fmt, mem::MaybeUninit, pin::Pin};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    vec::Vec,
};

use abs_buff::{
    buffer::{TrBuffSegmMut, TrBuffSegmRef, TrReclaim},
    error::{ReadErrTag, TrTaggedError, WriteErrTag},
    io::{TrInput, TrOutput},
    x_deps::{
        abs_cancel::{TrCancellationToken, TrMayCancel},
        anylr::SomeOf,
    },
};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Boom 仅作为错误类型存在，测试设备从不真的失败
pub(super) enum TestErr {
    Boom,
}

impl fmt::Display for TestErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestErr::Boom => write!(f, "boom"),
        }
    }
}

impl core::error::Error for TestErr {}

impl TrTaggedError<ReadErrTag> for TestErr {
    fn err_tag(&self) -> ReadErrTag {
        ReadErrTag::Unknown
    }
}

impl TrTaggedError<WriteErrTag> for TestErr {
    fn err_tag(&self) -> WriteErrTag {
        WriteErrTag::Unknown
    }
}

/// 一个立即就绪的 `TrMayCancel` future（测试设备的异步操作返回它）。
pub(super) struct ReadySegm<S, E>(Option<SomeOf<S, E>>);

impl<S, E> ReadySegm<S, E> {
    fn new(value: SomeOf<S, E>) -> Self {
        ReadySegm(Option::Some(value))
    }
}

impl<S, E> core::future::Future for ReadySegm<S, E> {
    type Output = SomeOf<S, E>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        Poll::Ready(this.0.take().expect("ready future polled once"))
    }
}

impl<'f, S: 'f, E: 'f> TrMayCancel<'f> for ReadySegm<S, E> {
    type MayCancelFuture<'g, C> = ReadySegm<S, E>
    where
        Self: 'g,
        C: TrCancellationToken + Clone,
        C: 'f,
        C: 'g,
        'g: 'f;
    type MayCancelOutput = SomeOf<S, E>;

    fn may_cancel_with<'g, C>(
        self,
        _cancel: &'g mut C,
    ) -> Self::MayCancelFuture<'g, C>
    where
        Self: 'g,
        'g: 'f,
        C: TrCancellationToken + Clone,
    {
        self
    }
}

/// 测试输入设备：内部数据与读取位置放在 `Arc` 里，**设备被缓冲拥有期间**，
/// 测试仍能通过自己持有的 `Arc` 观察进度（设备 move 进核心后测试无法直接
/// 访问它）。
pub(super) struct TestInput {
    pub data: Arc<Mutex<Vec<u8>>>,
    pub pos: Arc<AtomicUsize>,
}

impl TestInput {
    pub fn new(data: Vec<u8>) -> Self {
        TestInput {
            data: Arc::new(Mutex::new(data)),
            pos: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl TrInput<u8> for TestInput {
    type ReadAsync<'f> = ReadySegm<usize, TestErr> where Self: 'f;
    type Err = TestErr;

    fn read_async<'f>(
        &'f mut self,
        target: &'f mut [MaybeUninit<u8>],
    ) -> Self::ReadAsync<'f> {
        let data = self.data.lock().unwrap();
        let pos = self.pos.load(Ordering::Relaxed);
        let n = core::cmp::min(target.len(), data.len() - pos);
        for (i, slot) in target[..n].iter_mut().enumerate() {
            *slot = MaybeUninit::new(data[pos + i]);
        }
        self.pos.store(pos + n, Ordering::Relaxed);
        ReadySegm::new(SomeOf::new_left(n))
    }
}

/// 测试输出设备：收下的数据放在 `Arc` 里，测试可随时观察。
pub(super) struct TestOutput {
    pub data: Arc<Mutex<Vec<u8>>>,
}

impl TestOutput {
    pub fn new() -> Self {
        TestOutput {
            data: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl TrOutput<u8> for TestOutput {
    type WriteAsync<'f> = ReadySegm<usize, TestErr> where Self: 'f;
    type Err = TestErr;

    fn write_async<'f>(
        &'f mut self,
        source: &'f [MaybeUninit<u8>],
    ) -> Self::WriteAsync<'f> {
        let n = source.len();
        let mut data = self.data.lock().unwrap();
        for m in source {
            // SAFETY: 测试数据为 u8，无 drop 需求。
            data.push(unsafe { m.assume_init_read() });
        }
        ReadySegm::new(SomeOf::new_left(n))
    }
}

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

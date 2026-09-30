//! `ring::Ring` 的冒烟测试。
//!
//! # 运行时与驱动方式
//!
//! 需要异步的用例一律写成普通 `async fn`，紧随其后用
//! [`dual_runtime_test_`](crate::test_support_::dual_runtime_test_) 生成 **tokio** 与
//! **compio** 两个**真实运行时**下的测试方法；不在用例里 `block_on`、也不手动构造
//! waker 轮询——那样测到的是「假想的世界」，而不是真实运行时里由 waker 驱动的行为。
//!
//! # 覆盖范围
//!
//! 覆盖环形缓冲的常见冒烟场景：读写往返、空 / 满边界（含 REVERSION 约定下「满环」
//! 与「空环」的区分）、`Demand` 的下限 / 上限语义、跨末端的两段式段、多轮 FIFO 顺序，
//! 以及异步入口在条件不满足时确实 park、在取消令牌已就绪时立即返回 `Cancelled`。
//!
//! 自 `Ring::split` 起，读写两端可以拆成 [`RingWriter`] / [`RingReader`] 分别持有，
//! 因而能像 `circular_buff` 那样写「一端 park、另一端提交后唤醒」的 `join!` 并发用例
//! （见「分拆后的并发读写」一节）。
//!
//! # 已知缺陷：park 不能被复用、等待空间的写者会被提前唤醒
//!
//! `half_` 里的 `RingHalf_` 的 `wake_slot_` **只设不清**，且 `opt_demand_` 在 park
//! future 中途 drop 时**不 reset**；再叠加 `Producer::handle_event` 用 `data_size()`
//! （而非 `free_size()`）判定空间是否足够，会让「同一个半部第二次 park」「等待更多空间
//! 的写者被提前唤醒」时，`ring_*_async` 的循环在**一次 poll 内同步空转**（不让出也不
//! 返回，表现为任务挂死）。本节末尾的 `known_bug_*` 用例以「查询预算令牌」把空转截断
//! 成**有界失败**，既是缺陷复现，也是修复后的验收标准（**当前实现下它们会失败**）。
//!
//! `Ring` 目前没有对外的 `close`，故不含 EOF 用例。

use core::{
    future::{self, IntoFuture},
    mem::MaybeUninit,
};

use std::{
    boxed::Box,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    vec,
    vec::Vec,
};

use abs_buff::{
    Demand,
    buffer::{TrBuffSegmMut, TrReclaim},
    x_deps::abs_cancel::{CancelledToken, TrMayCancel, TrCancellationToken},
};

use crate::{
    ring::{reclaim::ReclSliceRef, *},
    test_support_::dual_runtime_test_,
};

// ---------------------------------------------------------------------------
// 测试辅助
// ---------------------------------------------------------------------------

/// 测试用缓冲类型：`Box<[MaybeUninit<u8>]>`（元素 `u8`）。
type TestBuff = Box<[MaybeUninit<u8>]>;

/// 测试用 `Ring` 具体类型。
type TestRing = Ring<TestBuff, u8>;

/// `Ring::split` 出的写端（借用 `Ring`）。
type TestWriter<'f> =
    RingWriter<&'f TestRing, TestBuff, u8>;

/// `Ring::split` 出的读端（借用 `Ring`）。
type TestReader<'f> =
    RingReader<&'f TestRing, TestBuff, u8>;

/// 构造一个指定容量的测试环（容量须落在 `Ring` 允许的 `[2, MAX_CAPACITY]` 内）。
fn new_ring_(capacity: usize) -> TestRing {
    let buff = Box::<[u8]>::new_uninit_slice(capacity);
    Ring::new_unchecked(buff)
}

/// 借出写段、把 `data` 位拷贝进环，drop 提交后返回实际写入的元素数。
async fn fill_bytes_(ring: &mut TestRing, demand: &Demand<usize>, data: &[u8]) -> usize {
    let some = ring.write_async(demand).await;
    let mut segm = some.pick_left().expect("应能借出写段");
    let moved = segm.move_items_from_as_buff(data);
    drop(segm); // 提交：按已写入量推进写位置
    moved
}

/// 借出读段、取出 `len` 个元素，drop 提交后返回取出的字节序列。
async fn take_bytes_(ring: &mut TestRing, demand: &Demand<usize>, len: usize) -> Vec<u8> {
    let some = ring.read_async(demand).await;
    let mut segm = some.pick_left().expect("应能借出读段");
    let got = take_segm_bytes_(&mut segm, len).await;
    drop(segm); // 提交：按已消费量推进读位置
    got
}

/// 从读段取出 `len` 个元素（段 drop 时按已消费量推进读位置）。
async fn take_segm_bytes_<R>(segm: &mut ReclSliceRef<'_, u8, R>, len: usize) -> Vec<u8>
where
    R: TrReclaim,
{
    assert!(len <= segm.least_count(), "取出量超过段内可读量");

    let mut staging = Vec::<MaybeUninit<u8>>::with_capacity(len);
    staging.resize(len, MaybeUninit::uninit());
    let moved = unsafe { segm.move_items_to_buff(&mut staging) };
    assert_eq!(moved, len, "应恰好取出 len 个元素");

    // SAFETY: 上面已把前 `len` 个 `MaybeUninit<u8>` 初始化；`u8` 为位拷贝且无 drop 资源。
    staging
        .into_iter()
        .map(|m| unsafe { m.assume_init() })
        .collect()
}

/// 通过写端借出写段并写入 `data`，drop 提交后返回实际写入的元素数。
async fn tx_write_(tx: &mut TestWriter<'_>, demand: &Demand<usize>, data: &[u8]) -> usize {
    let some = tx.write_async(demand).await;
    let mut segm = some.pick_left().expect("应能借出写段");
    let moved = segm.move_items_from_as_buff(data);
    drop(segm); // 提交：推进写位置并触发消费端事件
    moved
}

/// 通过读端借出读段并取出 `len` 个元素，drop 提交后返回取出的字节序列。
async fn rx_read_(rx: &mut TestReader<'_>, demand: &Demand<usize>, len: usize) -> Vec<u8> {
    let some = rx.read_async(demand).await;
    let mut segm = some.pick_left().expect("应能借出读段");
    let got = take_segm_bytes_(&mut segm, len).await;
    drop(segm); // 提交：推进读位置并触发生产端事件
    got
}

/// 测试用取消令牌：`is_cancelled()` 被查询满 `budget_` 次之后才「取消」。
///
/// # 用途
///
/// `wake_slot_` 只设不清的缺陷会让 park future 第二次被 poll 时立即就绪，读 / 写循环
/// 因此在**一次 poll 内同步空转**、不让出也不返回——直接跑会挂死。把预算调到很小的值，
/// 空转就会在有限次查询后以 `Cancelled` 结束，于是缺陷表现为**有界失败**而不是挂死。
///
/// `cancellation()` 返回永不就绪的 future：正常实现里首次 park 必须继续挂起，不能被这个
/// 令牌「提前放行」，否则用例在修复后也无法通过。
#[derive(Clone)]
struct BudgetToken {
    checks_: Arc<AtomicUsize>,
    budget_: usize,
}

impl BudgetToken {
    /// 构造一个预算为 `budget` 次的令牌（父令牌与子令牌共享同一计数）。
    fn new(budget: usize) -> Self {
        BudgetToken {
            checks_: Arc::new(AtomicUsize::new(0)),
            budget_: budget,
        }
    }

    /// 已发生的取消状态查询次数（含父令牌与子令牌）。
    fn checks(&self) -> usize {
        self.checks_.load(Ordering::Acquire)
    }
}

impl TrCancellationToken for BudgetToken {
    type Cancellation = future::Pending<()>;
    type ChildToken = BudgetToken;

    fn is_cancelled(&self) -> bool {
        self.checks_.fetch_add(1, Ordering::AcqRel) + 1 >= self.budget_
    }

    fn can_be_cancelled(&self) -> bool {
        true
    }

    fn child_token(&self) -> Self::ChildToken {
        self.clone()
    }

    fn cancellation(self) -> Self::Cancellation {
        future::pending()
    }
}

// ---------------------------------------------------------------------------
// 基础读写（保留的起步用例）
// ---------------------------------------------------------------------------

/// 同步读写入口在真实运行时下的最简往返。
/// - 测试目标：`Ring::try_write` / `try_read` 借出的读写段能把数据原样搬进、搬出。
/// - 测试手段：容量 8 上借写段写入 `[1, 2, 4, 16]` 并提交，再借读段取回。
/// - 判定标准：取回的字节序列与写入序列完全一致。
async fn smoke_test_sync_() {
    const BUFF_SIZE: usize = 8;
    let buff = Box::<[u8]>::new_uninit_slice(BUFF_SIZE);
    let mut ring = Ring::new_unchecked(buff);
    {
        let w_demand = Demand::less_than(BUFF_SIZE);
        let w_x = ring.try_write(&w_demand);
        let mut w_segm = w_x.pick_left().unwrap();

        let msg = [1u8, 2u8, 4u8, 16u8];
        w_segm.move_items_from_as_buff(&msg);
    }
    {
        let r_demand = Demand::less_than(BUFF_SIZE);
        let r_x = ring.try_read(&r_demand);
        let mut r_segm = r_x.pick_left().unwrap();

        let mut msg = [MaybeUninit::<u8>::uninit(); BUFF_SIZE];
        let size = unsafe { r_segm.move_items_to_buff(&mut msg) };

        let vec: Vec<u8> = msg
            .iter()
            .map(|m| unsafe { m.assume_init_read() })
            .collect();
        assert_eq!(
            &vec.as_slice()[..size],
            [1u8, 2u8, 4u8, 16u8].as_slice(),
        )
    }
}

dual_runtime_test_!(smoke_test_sync_);

/// 异步读写入口在真实运行时下的最简往返。
/// - 测试目标：`Ring::write_async` / `read_async` 在数据 / 空间就绪时就地完成。
/// - 测试手段：容量 8 上 `write_async` 写入 `[1, 2, 4, 16]`，再 `read_async` 取回。
/// - 判定标准：取回的字节序列与写入序列完全一致。
async fn smoke_test_async_() {
    const BUFF_SIZE: usize = 8;
    let buff = Box::<[u8]>::new_uninit_slice(BUFF_SIZE);
    let mut ring = Ring::new_unchecked(buff);
    {
        let w_demand = Demand::less_than(BUFF_SIZE);
        let w_x = ring.write_async(&w_demand).await;
        let mut w_segm = w_x.pick_left().unwrap();

        let msg = [1u8, 2u8, 4u8, 16u8];
        w_segm.move_items_from_as_buff(&msg);
    }
    {
        let r_demand = Demand::less_than(BUFF_SIZE);
        let r_x = ring.read_async(&r_demand).await;
        let mut r_segm = r_x.pick_left().unwrap();

        let mut msg = [MaybeUninit::<u8>::uninit(); BUFF_SIZE];
        let size = unsafe { r_segm.move_items_to_buff(&mut msg) };

        let vec: Vec<u8> = msg
            .iter()
            .map(|m| unsafe { m.assume_init_read() })
            .collect();
        assert_eq!(
            &vec.as_slice()[..size],
            [1u8, 2u8, 4u8, 16u8].as_slice(),
        )
    }
}

dual_runtime_test_!(smoke_test_async_);

// ---------------------------------------------------------------------------
// 边界与 Demand 语义
// ---------------------------------------------------------------------------

/// 空环上的 `try_read` 必须返回 `Drained`，而不是空段或挂起。
/// - 测试目标：无数据时读入口不给出空段（否则调用方会拿到 0 长度却以为成功）。
/// - 测试手段：新建容量 8 的空环，直接以 `at_least(1)` 调用 `try_read`。
/// - 判定标准：返回 `ConsumerError::Drained`；`data_size == 0`、`free_size == 8`。
async fn try_read_empty_returns_drained_() {
    let mut ring = new_ring_(8);

    let demand = Demand::at_least(1);
    let some = ring.try_read(&demand);
    assert!(
        matches!(some.pick_right(), Some(ConsumerError::Drained(_))),
        "空环读应返回 Drained"
    );
    assert_eq!(ring.data_size(), 0, "空环数据量应为 0");
    assert_eq!(ring.free_size(), 8, "空环可写空间应为整个容量");
}

dual_runtime_test_!(try_read_empty_returns_drained_);

/// 满环上的 `try_write` 必须返回 `Stuffed`；且「满」不能被误判成「空」。
/// - 测试目标：REVERSION 约定下满环（`wp == rp && rv`）与空环（`wp == rp && !rv`）可区分。
/// - 测试手段：容量 4 上写满 4 个元素，断言状态并再试写一次；随后读空、再次写满。
/// - 判定标准：满环 `data_size == 4`、`free_size == 0`、再写返回 `Stuffed`；读空后
///   `data_size == 0`、`free_size == 4`，并且还能再次写满（位置可复用）。
async fn try_write_full_returns_stuffed_() {
    let mut ring = new_ring_(4);

    let written = fill_bytes_(&mut ring, &Demand::at_least(4), &[1, 2, 3, 4]).await;
    assert_eq!(written, 4, "空环应能一次写满");
    assert_eq!(ring.data_size(), 4, "满环应报告 capacity 个可读元素");
    assert_eq!(ring.free_size(), 0, "满环应无剩余可写空间");

    let demand = Demand::at_least(1);
    let some = ring.try_write(&demand);
    assert!(
        matches!(some.pick_right(), Some(ProducerError::Stuffed(_))),
        "满环写应返回 Stuffed"
    );

    let got = take_bytes_(&mut ring, &Demand::at_least(4), 4).await;
    assert_eq!(got, vec![1, 2, 3, 4], "满环读出应完整且有序");
    assert_eq!(ring.data_size(), 0, "读空后数据量应为 0");
    assert_eq!(ring.free_size(), 4, "读空后应恢复全部可写空间");

    let written = fill_bytes_(&mut ring, &Demand::at_least(4), &[5, 6, 7, 8]).await;
    assert_eq!(written, 4, "读空后应能再次写满");
}

dual_runtime_test_!(try_write_full_returns_stuffed_);

/// 数据量不足 `Demand` 下限时，`try_read` 必须返回 `Drained` 而不是部分段。
/// - 测试目标：读侧的 `at_least` 下限门控。
/// - 测试手段：容量 8 上写 2 个元素，先以 `at_least(4)` 读，再以 `at_least(2)` 读。
/// - 判定标准：不足下限返回 `Drained` 且数据仍在（`data_size == 2`）；下限满足时取回两个元素。
async fn try_read_honours_at_least_() {
    let mut ring = new_ring_(8);
    assert_eq!(fill_bytes_(&mut ring, &Demand::at_least(2), &[1, 2]).await, 2);

    let demand = Demand::at_least(4);
    let some = ring.try_read(&demand);
    assert!(
        matches!(some.pick_right(), Some(ConsumerError::Drained(_))),
        "数据不足下限时必须返回 Drained"
    );
    assert_eq!(ring.data_size(), 2, "被拒绝的读不应消费数据");

    let got = take_bytes_(&mut ring, &Demand::at_least(2), 2).await;
    assert_eq!(got, vec![1, 2], "满足下限后应取回全部数据");
}

dual_runtime_test_!(try_read_honours_at_least_);

/// 可写空间不足 `Demand` 下限时，`try_write` 必须返回 `Stuffed` 而不是部分段。
/// - 测试目标：写侧的 `at_least` 下限门控。
/// - 测试手段：容量 8 上写 5 个元素（剩 3 格），先以 `at_least(4)` 借写段，
///   再以 `at_least(3)` 借写段。
/// - 判定标准：不足下限返回 `Stuffed`；满足下限时能借出写段。
async fn try_write_honours_at_least_() {
    let mut ring = new_ring_(8);
    assert_eq!(fill_bytes_(&mut ring, &Demand::at_least(5), &[0u8; 5]).await, 5);
    assert_eq!(ring.free_size(), 3, "写 5 后应剩 3 格可写空间");

    let demand = Demand::at_least(4);
    let some = ring.try_write(&demand);
    assert!(
        matches!(some.pick_right(), Some(ProducerError::Stuffed(_))),
        "可写空间不足下限时必须返回 Stuffed"
    );

    let demand = Demand::at_least(3);
    let some = ring.try_write(&demand);
    assert!(some.pick_left().is_some(), "满足下限时应能借出写段");
}

dual_runtime_test_!(try_write_honours_at_least_);

/// `Demand` 上界限制单次借出的元素数，且未消费的段 drop 后不推进位置。
/// - 测试目标：`less_than` 上界语义与「空借出（offset 为 0）不消费」。
/// - 测试手段：容量 8 上写 5 个元素；以 `less_than(2)` 借读段，断言段长后直接 drop；
///   再以 `less_than(2)` 取走 2 个。
/// - 判定标准：单次段长恰为 2；drop 未消费的段后 `data_size` 仍为 5；取走后剩 3。
async fn try_read_limits_count_by_max_() {
    let mut ring = new_ring_(8);
    assert_eq!(fill_bytes_(&mut ring, &Demand::less_than(5), &[1, 2, 3, 4, 5]).await, 5);
    assert_eq!(ring.data_size(), 5);

    let demand = Demand::less_than(2);
    let some = ring.try_read(&demand);
    let segm = some.pick_left().expect("应能借出读段");
    assert_eq!(segm.least_count(), 2, "上界为 2 时应只借出 2 个元素");
    drop(segm); // offset 仍为 0：不得消费任何元素
    assert_eq!(ring.data_size(), 5, "未消费的读段 drop 后数据量应不变");

    let got = take_bytes_(&mut ring, &Demand::less_than(2), 2).await;
    assert_eq!(got, vec![1, 2]);
    assert_eq!(ring.data_size(), 3, "取走 2 个后应剩 3 个");
}

dual_runtime_test_!(try_read_limits_count_by_max_);

/// 构造入口拒绝小于最小容量的缓冲区。
/// - 测试目标：`Ring::try_new` 的容量校验（`MIN_CAPACITY == 2`）。
/// - 测试手段：分别以容量 1 和容量 2 的 `Box<[MaybeUninit<u8>]>` 调用 `try_new`。
/// - 判定标准：容量 1 返回 `Err(1)`；容量 2 构造成功且 `capacity() == 2`。
async fn try_new_rejects_too_small_capacity_() {
    let buff = Box::<[u8]>::new_uninit_slice(1);
    let x: Result<TestRing, usize> = Ring::try_new(buff);
    assert_eq!(x.err(), Some(1usize), "容量 1 应被拒绝");

    let buff = Box::<[u8]>::new_uninit_slice(2);
    let x: Result<TestRing, usize> = Ring::try_new(buff);
    let ring = x.expect("容量 2 应被接受");
    assert_eq!(ring.capacity(), 2);
}

dual_runtime_test_!(try_new_rejects_too_small_capacity_);

// ---------------------------------------------------------------------------
// 跨末端环绕
// ---------------------------------------------------------------------------

/// 写 / 读区域跨过缓冲区物理末端时，段被拆成两段物理空间，逻辑上仍是一段。
/// - 测试目标：跨末端环绕时段的物理拆分与数据顺序。
/// - 测试手段：容量 5 上写 3、读 2（此时 `wp == 3`、`rp == 2`），再以 `at_least(4)`
///   借写段（应为 `[3,5)` 与 `[0,2)` 两段）写入 3 个元素；随后以 `at_least(4)` 借读段
///   （应为 `[2,5)` 与 `[0,1)` 两段）读空。
/// - 判定标准：写段物理长度为 `[2, 2]`、读段为 `[3, 1]`；读回序列为 `[3, 10, 11, 12]`；
///   读空后 `data_size == 0`。
async fn wrap_around_two_pieces_() {
    let mut ring = new_ring_(5);

    // 写 [1, 2, 3]：wp = 3。
    assert_eq!(fill_bytes_(&mut ring, &Demand::at_least(3), &[1, 2, 3]).await, 3);
    // 读 [1, 2]：rp = 2，剩 1 个可读、4 格可写。
    assert_eq!(take_bytes_(&mut ring, &Demand::at_least(2), 2).await, vec![1, 2]);

    // 再借 4 格：物理上跨末端，应为 [3,5) 两格 + [0,2) 两格。
    let demand = Demand::at_least(4);
    let some = ring.write_async(&demand).await;
    let mut segm = some.pick_left().expect("跨末端也应一次借出全部 4 格");
    let lens: Vec<usize> = segm.iter_slices_mut().map(|s| s.len()).collect();
    assert_eq!(lens, vec![2, 2], "跨末端写段应拆成 [3,5) 与 [0,2)");
    assert_eq!(segm.move_items_from_as_buff(&[10u8, 11, 12]), 3);
    drop(segm);
    assert_eq!(ring.data_size(), 4, "写入 3 个后应共有 4 个可读元素");

    // 可读区同样跨末端：应为 [2,5) 三格 + [0,1) 一格。
    let demand = Demand::at_least(4);
    let some = ring.read_async(&demand).await;
    let segm = some.pick_left().expect("应有 4 个元素可读");
    let lens: Vec<usize> = segm.iter_slices().map(|s| s.len()).collect();
    assert_eq!(lens, vec![3, 1], "跨末端读段应拆成 [2,5) 与 [0,1)");
    drop(segm); // offset 为 0：不消费

    let got = take_bytes_(&mut ring, &Demand::at_least(4), 4).await;
    assert_eq!(got, vec![3, 10, 11, 12], "跨末端读出应保持写入顺序");
    assert_eq!(ring.data_size(), 0, "读空后数据量应为 0");
}

dual_runtime_test_!(wrap_around_two_pieces_);

/// 多轮写入 / 读出后仍保持严格 FIFO，位置在环绕与跨末端之间正确推进。
/// - 测试目标：反复「写入 3 个、读空」时 REVERSION 置位 / 清除与环绕位置不串数据。
/// - 测试手段：容量 4 上循环 40 轮，每轮写入 3 个由轮次派生的字节并立即读空。
/// - 判定标准：每轮写满 3、读回顺序与写入一致、读空后 `data_size == 0`。
async fn wrap_around_fifo_cycles_() {
    const CAP: usize = 4;
    let mut ring = new_ring_(CAP);

    for i in 0..40u32 {
        let base = (i * 3) as u8;
        let payload = [base, base.wrapping_add(1), base.wrapping_add(2)];

        let written = fill_bytes_(&mut ring, &Demand::at_least(3), &payload).await;
        assert_eq!(written, 3, "第 {i} 轮应写入 3 个元素");
        assert_eq!(ring.data_size(), 3, "第 {i} 轮写入后应有 3 个可读元素");

        let got = take_bytes_(&mut ring, &Demand::at_least(3), 3).await;
        assert_eq!(got, payload.to_vec(), "第 {i} 轮 FIFO 顺序应保持");
        assert_eq!(ring.data_size(), 0, "第 {i} 轮读空后数据量应为 0");
    }
}

dual_runtime_test_!(wrap_around_fifo_cycles_);

// ---------------------------------------------------------------------------
// 异步入口
// ---------------------------------------------------------------------------

/// 数据 / 空间都就绪时，异步读写入口在真实运行时里直接完成往返。
/// - 测试目标：`write_async` / `read_async` 的即时完成路径与 `try_*` 数据一致。
/// - 测试手段：容量 8 上 `write_async(&at_least(3))` 写入 `[1, 2, 3]`，
///   再 `read_async(&at_least(3))` 取回。
/// - 判定标准：写后 `data_size == 3`；读出序列为 `[1, 2, 3]`；读后 `data_size == 0`。
async fn async_write_read_roundtrip_() {
    let mut ring = new_ring_(8);

    {
        let demand = Demand::at_least(3);
        let some = ring.write_async(&demand).await;
        let mut segm = some.pick_left().expect("空环应可写");
        assert_eq!(segm.move_items_from_as_buff(&[1u8, 2, 3]), 3);
    }
    assert_eq!(ring.data_size(), 3, "异步写提交后应有 3 个可读元素");

    {
        let demand = Demand::at_least(3);
        let some = ring.read_async(&demand).await;
        let mut segm = some.pick_left().expect("应有 3 个元素可读");
        let got = take_segm_bytes_(&mut segm, 3).await;
        assert_eq!(got, vec![1, 2, 3], "异步读应取回写入序列");
    }
    assert_eq!(ring.data_size(), 0, "异步读提交后应已读空");
}

dual_runtime_test_!(async_write_read_roundtrip_);

/// 数据量低于 `Demand` 下限时，`read_async` 必须 park 而不是就绪。
/// - 测试目标：读侧异步等待在「不足下限」时不虚假就绪（并验证 park 不消费数据）。
/// - 测试手段：写 2 个元素后以 `at_least(5)` 发起 `read_async`，与「让出一次」的探测
///   并发：探测放在 `select` 左侧，因此第二次轮询只会看到探测就绪并短路，被 park 的
///   读等待不会被再次轮询（`ring` 的唤醒槽位当前只设不清，二次轮询会空转）。
/// - 判定标准：`select` 返回左侧（探测先完成）证明读等待仍挂起；`data_size` 仍为 2。
async fn read_async_parks_when_insufficient_() {
    let mut ring = new_ring_(8);
    assert_eq!(fill_bytes_(&mut ring, &Demand::at_least(2), &[7, 8]).await, 2);

    let demand = Demand::at_least(5); // 只有 2 个，低于下限
    {
        let read = ring.read_async(&demand).into_future();
        let probe = async { futures_lite::future::yield_now().await };
        futures_util::pin_mut!(probe, read);
        match futures_util::future::select(probe, read).await {
            futures_util::future::Either::Left(_) => {}
            futures_util::future::Either::Right(_) => {
                panic!("数据不足下限时读等待不得就绪（Demand 门控失效）")
            }
        }
    } // 读等待在此 drop，归还对 `ring` 的可变借用

    assert_eq!(ring.data_size(), 2, "park 不应消费数据");
}

dual_runtime_test_!(read_async_parks_when_insufficient_);

/// 可写空间为 0 时，`write_async` 必须 park 而不是就绪。
/// - 测试目标：写侧异步等待在满环时不虚假就绪（并验证 park 不释放空间）。
/// - 测试手段：容量 4 写满后以 `at_least(1)` 发起 `write_async`，与「让出一次」的探测
///   并发（探测在 `select` 左侧，保证被 park 的写等待只被轮询一次）。
/// - 判定标准：`select` 返回左侧证明写等待仍挂起；`free_size` 仍为 0。
async fn write_async_parks_when_full_() {
    let mut ring = new_ring_(4);
    assert_eq!(fill_bytes_(&mut ring, &Demand::at_least(4), &[1, 2, 3, 4]).await, 4);

    let demand = Demand::at_least(1);
    {
        let write = ring.write_async(&demand).into_future();
        let probe = async { futures_lite::future::yield_now().await };
        futures_util::pin_mut!(probe, write);
        match futures_util::future::select(probe, write).await {
            futures_util::future::Either::Left(_) => {}
            futures_util::future::Either::Right(_) => {
                panic!("满环时写等待不得就绪")
            }
        }
    } // 写等待在此 drop，归还对 `ring` 的可变借用

    assert_eq!(ring.free_size(), 0, "park 不应释放可写空间");
}

dual_runtime_test_!(write_async_parks_when_full_);

/// 取消令牌已就绪时，异步读写入口立即返回 `Cancelled`，不进入 park。
/// - 测试目标：`gen_may_cancel_future` 生成的 future 的主动取消路径。
/// - 测试手段：空环上以 `CancelledToken` 驱动 `read_async`；写满后以同样的令牌驱动
///   `write_async`。
/// - 判定标准：读侧返回 `ConsumerError::Cancelled`；写侧返回 `ProducerError::Cancelled`。
async fn async_ops_honour_cancellation_() {
    let mut ring = new_ring_(8);

    // 读侧：空环 + 已取消令牌 → 立即 Cancelled（未注册 waker）。
    let demand = Demand::at_least(1);
    let some = ring
        .read_async(&demand)
        .may_cancel_with(CancelledToken::new())
        .await;
    assert!(
        matches!(some.pick_right(), Some(ConsumerError::Cancelled)),
        "读等待应被取消令牌中止"
    );

    // 写侧：写满 + 已取消令牌 → 立即 Cancelled。
    assert_eq!(fill_bytes_(&mut ring, &Demand::at_least(8), &[0u8; 8]).await, 8);
    let demand = Demand::at_least(1);
    let some = ring
        .write_async(&demand)
        .may_cancel_with(CancelledToken::new())
        .await;
    assert!(
        matches!(some.pick_right(), Some(ProducerError::Cancelled)),
        "满环上的写等待应被取消令牌中止"
    );
}

dual_runtime_test_!(async_ops_honour_cancellation_);

// ---------------------------------------------------------------------------
// 分拆后的并发读写（调用者日常用法）
// ---------------------------------------------------------------------------

/// 分拆出的写端 / 读端可分别持有，同步读写往返与两端状态查询都正常。
/// - 测试目标：`Ring::split` 之后 `RingWriter::try_write` / `RingReader::try_read`
///   的日常往返，以及两端的 `producer_state` / `consumer_state` 视图。
/// - 测试手段：容量 8 上用写端写 3 个元素并查状态；再用读端读回 3 个并查状态。
/// - 判定标准：写后读端看到 `data == 3`、写端看到 `free == 5`；读回序列为
///   `[1, 2, 3]`；读空后读端看到 `data == 0`。
async fn split_tx_rx_roundtrip_() {
    let mut ring = new_ring_(8);
    let (mut tx, mut rx) = Ring::split(&mut ring);

    {
        let demand = Demand::at_least(3);
        let mut segm = tx.try_write(&demand).pick_left().expect("应能借出写段");
        assert_eq!(segm.move_items_from_as_buff(&[1u8, 2, 3]), 3);
    }
    assert_eq!(rx.consumer_state(), Some((3, false)), "读端应看到 3 个可读元素");
    assert_eq!(tx.producer_state(), Some((5, false)), "写端应看到 5 格可写空间");

    {
        let demand = Demand::at_least(3);
        let mut segm = rx.try_read(&demand).pick_left().expect("应能借出读段");
        assert_eq!(take_segm_bytes_(&mut segm, 3).await, vec![1, 2, 3]);
    }
    assert_eq!(rx.consumer_state(), Some((0, false)), "读空后读端应看到 0");
}

dual_runtime_test_!(split_tx_rx_roundtrip_);

/// 读端先 park，写端提交后在同一任务里把它唤醒。
/// - 测试目标：分拆后「一端 park、另一端提交」的读侧唤醒路径（`read_async`）。
/// - 测试手段：`join!` 并发「读端等 3 个元素」与「写端立即提交 `[7, 8, 9]`」。把 park
///   的读端放在 `join!` 第一位，保证它先挂起；写端不额外 `yield`，避免给读等待制造
///   一次「条件尚未满足」的轮询（当前实现会因此空转，见 `known_bug_*`）。
/// - 判定标准：读等待被唤醒并取回 `[7, 8, 9]`。
async fn read_async_wakes_on_write_() {
    let mut ring = new_ring_(8);
    let (mut tx, mut rx) = Ring::split(&mut ring);

    let read = async {
        let demand = Demand::at_least(3);
        assert_eq!(rx_read_(&mut rx, &demand, 3).await, vec![7, 8, 9]);
    };
    let write = async {
        let demand = Demand::at_least(3);
        let mut segm = tx.try_write(&demand).pick_left().expect("应能借出写段");
        assert_eq!(segm.move_items_from_as_buff(&[7u8, 8, 9]), 3);
    };
    futures_util::join!(read, write);
}

dual_runtime_test_!(read_async_wakes_on_write_);

/// 写端先 park（满环），读端取走后把它唤醒。
/// - 测试目标：分拆后「一端 park、另一端提交」的写侧唤醒路径（`write_async`）。
/// - 测试手段：容量 4 写满；`join!` 并发「写端等 1 格空间」与「读端取走 1 个元素」，
///   把 park 的写端放在第一位、读端不 `yield`。
/// - 判定标准：写等待被唤醒、拿到恰好 1 格空间并写回 1 个元素。
async fn write_async_wakes_on_read_() {
    let mut ring = new_ring_(4);
    let (mut tx, mut rx) = Ring::split(&mut ring);
    {
        let demand = Demand::at_least(4);
        let mut segm = tx.try_write(&demand).pick_left().expect("应能借出写段");
        assert_eq!(segm.move_items_from_as_buff(&[1u8, 2, 3, 4]), 4);
    }

    let write = async {
        let demand = Demand::at_least(1);
        let some = tx.write_async(&demand).await;
        let mut segm = some.pick_left().expect("读走 1 格后写等待应被唤醒");
        assert_eq!(segm.least_count(), 1, "读走 1 格后应恰好可写 1 格");
        assert_eq!(segm.move_items_from_as_buff(&[9u8]), 1);
    };
    let read = async {
        let demand = Demand::less_than(1);
        assert_eq!(rx_read_(&mut rx, &demand, 1).await, vec![1]);
    };
    futures_util::join!(write, read);
}

dual_runtime_test_!(write_async_wakes_on_read_);

/// 取消一个挂起的读等待后，已经就绪的数据仍可正常取走。
/// - 测试目标：读等待被取消（future drop）不会破坏环的读写状态。
/// - 测试手段：空环上发起 `read_async`，用 `select` + 让出一次证明它挂起后取消；
///   写端写入 3 个元素；读端以**同步** `try_read` 取回。
/// - 判定标准：取回的序列为 `[1, 2, 3]`；写后读端状态显示 3 个可读元素。
///
/// 说明：这里刻意**不**再次 park（第二次 park 的前景见 `known_bug_*`）；`demand` 也刻意
/// 活到函数结束，避免缺陷二（drop 时不 `reset_demand_`）留下的悬垂指针。
async fn cancel_pending_read_then_read_ready_() {
    let mut ring = new_ring_(8);
    let (mut tx, mut rx) = Ring::split(&mut ring);

    let demand = Demand::at_least(3);
    {
        let read = rx.read_async(&demand).into_future();
        let probe = async { futures_lite::future::yield_now().await };
        futures_util::pin_mut!(probe, read);
        assert!(
            matches!(
                futures_util::future::select(probe, read).await,
                futures_util::future::Either::Left(_)
            ),
            "空环上读等待应先 park（探测先完成）"
        );
    } // read drop = 取消

    assert_eq!(tx_write_(&mut tx, &Demand::at_least(3), &[1, 2, 3]).await, 3);
    assert_eq!(rx.consumer_state(), Some((3, false)));

    let demand = Demand::at_least(3);
    let mut segm = rx.try_read(&demand).pick_left().expect("应能借出读段");
    assert_eq!(take_segm_bytes_(&mut segm, 3).await, vec![1, 2, 3]);
}

dual_runtime_test_!(cancel_pending_read_then_read_ready_);

// ---------------------------------------------------------------------------
// 已知缺陷复现（当前实现下**故意失败**）
// ---------------------------------------------------------------------------
//
// 以下用例以**调用者日常用法**复现三个互相放大的缺陷：
//
// 1. `wake_slot_` 只设不清：park future 首次 poll 注册 waker 后，第二次 poll 直接返回
//    `Ready(None)`（不挂起）；`ring_*_async` 的循环于是「重试 → 再 park → 立即就绪」
//    同步空转，不让出也不返回。
// 2. `opt_demand_` 在 park future 中途 drop（取消）时不 `reset`：旧 demand 残留，对端
//    `handle_event` 可能按过时下限判定唤醒，并留下悬垂指针。
// 3. `Producer::handle_event` 用 `data_size()` 而非 `free_size()` 判定空间：等待更多
//    空间的写者会被提前唤醒（满环读走 1 格、写者要 3 格时就会发生）。
//
// 直接跑会挂死，故用 [`BudgetToken`] 把空转截断成**有界失败**：缺陷在有限次查询后以
// `Cancelled` 暴露；修复后（第一次 park 的取消要清理唤醒槽位与 demand、park 只在真正被
// 唤醒时重试、写端按 `free_size` 判定）这两个用例应当通过。

/// 已知缺陷复现（读侧）：同一个读端第二次 park 会同步空转。
/// - 测试目标：期望「挂起的读等待被取消后，可以再次挂起并正常被写端唤醒」。
/// - 测试手段：空环上读等待 park 一次后取消；随后在 `join!` 中让读端**再次**等待 1 个
///   元素、写端提交 1 个元素；用查询预算令牌把可能出现的空转截断。
/// - 判定标准（期望行为）：第二次读等待被写端唤醒，取回 `[1]`。
///   当前实现：`wake_slot_` 仍指向已取消的等待者，第二次 park 立即 `Ready(None)`，读循环
///   在**一次 poll 内**反复查询取消状态直到预算耗尽，最终以 `Cancelled` 失败。
async fn known_bug_reader_second_wait_busy_loops_() {
    let mut ring = new_ring_(8);
    let (mut tx, mut rx) = Ring::split(&mut ring);

    // 第一次等待：park 后取消（drop）。demand 活到函数结束，规避缺陷二的悬垂指针 UB。
    let first_demand = Demand::at_least(1);
    {
        let read = rx.read_async(&first_demand).into_future();
        let probe = async { futures_lite::future::yield_now().await };
        futures_util::pin_mut!(probe, read);
        assert!(
            matches!(
                futures_util::future::select(probe, read).await,
                futures_util::future::Either::Left(_)
            ),
            "空环上首次读等待应先 park"
        );
    }

    let token = BudgetToken::new(64);
    let read = async {
        let demand = Demand::at_least(1);
        let some = rx
            .read_async(&demand)
            .may_cancel_with(token.clone())
            .await;
        let mut segm = some.pick_left().unwrap_or_else(|| {
            panic!(
                "第二次读等待未挂起：在一次 poll 内查询取消状态 {} 次后中止。\
                 根因是 wake_slot_ 只设不清（park future 第二次 poll 立即 Ready(None)），\
                 叠加 opt_demand_ 在 park 中途 drop 时不 reset。",
                token.checks()
            )
        });
        assert_eq!(take_segm_bytes_(&mut segm, 1).await, vec![1]);
    };
    let write = async {
        assert_eq!(tx_write_(&mut tx, &Demand::at_least(1), &[1]).await, 1);
    };
    futures_util::join!(read, write);
}

dual_runtime_test_!(known_bug_reader_second_wait_busy_loops_);

/// 已知缺陷复现（写侧）：等待「更多空间」的写者被提前唤醒后会同步空转。
/// - 测试目标：期望「写者需要 3 格空间时，只有真正腾出 3 格才被唤醒」。
/// - 测试手段：容量 4 写满；写端等待 3 格空间；读端先取走 1 格（只腾出 1 格，不应唤醒
///   写者），让出一次后再取走 2 格（此时才腾出 3 格）；用查询预算令牌把可能出现的空转
///   截断。
/// - 判定标准（期望行为）：写者最终拿到 3 格空间并写回 3 个元素。
///   当前实现：读走 1 格后 `Producer::handle_event` 按 `data_size() >= 3` 误判为「空间
///   足够」而提前唤醒写者；写者重试仍失败，park 又因 `wake_slot_` 未清立即就绪，循环同步
///   空转，直到预算耗尽以 `Cancelled` 失败。
///
/// 同一判定错误还有**反向**表现：若读端一次读走 3 格（`data_size` 降为 1），
/// `data_size() < 3` 会让 `handle_event` 直接返回——真正腾出的 3 格空间反而**不唤醒**写者，
/// 写者永久挂起。该表现是「漏唤醒」，park 之后不再查询取消状态，预算令牌无法截断，故本
/// 用例只钉住「提前唤醒 → 空转」这一半（漏唤醒可由同一处 `free_size()` 修复一并解决）。
async fn known_bug_writer_busy_loops_on_spurious_wake_() {
    let mut ring = new_ring_(4);
    let (mut tx, mut rx) = Ring::split(&mut ring);
    {
        let demand = Demand::at_least(4);
        let mut segm = tx.try_write(&demand).pick_left().expect("应能借出写段");
        assert_eq!(segm.move_items_from_as_buff(&[1u8, 2, 3, 4]), 4);
    }

    let token = BudgetToken::new(64);
    let write = async {
        let demand = Demand::at_least(3);
        let some = tx
            .write_async(&demand)
            .may_cancel_with(token.clone())
            .await;
        let mut segm = some.pick_left().unwrap_or_else(|| {
            panic!(
                "等待 3 格空间的写者被提前唤醒：在一次 poll 内查询取消状态 {} 次后中止。\
                 根因是 Producer::handle_event 用 data_size 而非 free_size 判定空间，\
                 叠加 wake_slot_ 只设不清。",
                token.checks()
            )
        });
        assert_eq!(segm.move_items_from_as_buff(&[9u8, 9, 9]), 3);
    };
    let read = async {
        // 只取走 1 格：不是写者需要的 3 格。
        let demand = Demand::less_than(1);
        assert_eq!(rx_read_(&mut rx, &demand, 1).await, vec![1]);
        // 让出一次：给被提前唤醒的写者一次重新轮询的机会（缺陷在此暴露）。
        futures_lite::future::yield_now().await;
        // 再取走 2 格：此时才真正腾出 3 格。
        let demand = Demand::less_than(2);
        assert_eq!(rx_read_(&mut rx, &demand, 2).await, vec![2, 3]);
    };
    futures_util::join!(write, read);
}

dual_runtime_test_!(known_bug_writer_busy_loops_on_spurious_wake_);

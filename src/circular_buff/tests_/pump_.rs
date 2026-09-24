//! 主动端的测试：输入泵（`pipe_from_input`）、输出泵（`pipe_into_output`）、
//! 以及全主动流水线（`Pipeline` future）。主动端不 `spawn` 任何任务：数据由
//! executor 驱动的泵搬运（被动端异步等待 / `try_*` 重试 / 构建期初始泵），
//! fire 只唤醒不搬运；全主动流水线由设备驱动的 `Pipeline` future 持续搬运。
//!
//! 主动端**不产出半部**：`build_async` 只把被动端的半部交给调用者（主动生产 ×
//! 被动消费 → 仅消费端；被动生产 × 主动消费 → 仅生产端；主动 × 主动 →
//! `Pipeline` future）。
//!
//! # 运行时与驱动方式
//!
//! 需要 `build_async` / `read_async` / `write_async` 的用例一律写成 `async fn`，
//! 由 [`dual_runtime_test_`](crate::test_support_::dual_runtime_test_) 在 **tokio**
//! 与 **compio** 两种**真实运行时**下各跑一遍；不 `block_on`、不手动构造 waker。
//!
//! 全主动流水线（`Pipeline`）是**永不结束**的设备驱动 future，无法被 `await`
//! 到完成：这类用例把它与一个「让出若干次 / 直到可观测结果成立」的探测 future
//! 用 `futures_util::future::select` 并发，由运行时按 `select` 的**左优先**顺序
//! 反复轮询流水线；探测 future 完成即代表搬运窗口已给足，随后断言最终结果。
//! 需要「等待侧先生效、再被对端唤醒」的用例则用 `futures_util::join!` + 一次
//! `yield_now().await`，让等待侧先撞上等待条件。
//!
//! 设备 move 进核心后测试无法直接访问，经 `Arc` 观察其内部状态。

use std::{pin::Pin, sync::atomic::Ordering, vec, vec::Vec};

use abs_buff::{
    Demand, TrBuffTryRead, TrBuffTryWrite,
    io::{TrInput, TrOutput},
    x_deps::{
        abs_cancel::{TrCancellationToken, TrMayCancel},
        anylr::SomeOf,
    },
};

use mm_ptr::Owned;

use super::{
    super::{
        ConsumerError, ProducerError,
        abs_comp_::{ConsumerHookEvent, TrConsumer},
        core_::{CircCore, Waiter},
        BufProducer, CoreAlloc, DevConsumer,
    },
    DefaultBuilder, ReadySegm, TestErr, TestInput, TestOutput, TestWaker, fill_segm,
    take_segm,
};
use crate::test_support_::dual_runtime_test_;

/// 主动生产 × 被动消费：构造（`init_async`）即把 `TrInput` 现有数据灌满缓冲；
/// 消费端每读取一次，读取提交（`advance_read`）驱动主动生产者**重复拉取**补满
/// 空位；输入耗尽后停止。
/// - 手段：输入 20 字节、容量 8；构建后循环读空，每次提交后再看缓冲是否补满。
/// - 判断：读回序列为 0..20；输入被读走 20、缓冲读空为 0；未耗尽时每次提交都
///   补满到 8。
///
/// `build_async` 只返回消费端半部——主动生产端由设备驱动，不产出写半部。
async fn pipe_from_input_fills_and_refills_() {
    let input = TestInput::new((0..20).collect());
    let data = input.data.clone();
    let pos = input.pos.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_from_input(input)
        .consumer_passive();
    let mut rx = ready.build_async().await.unwrap();

    // 构造完成即已填满：容量 8 全部可用（REVERSION 约定，不再空一槽）→ 8 格。
    assert_eq!(rx.data_size(), 8);
    assert_eq!(pos.load(Ordering::Relaxed), 8, "输入设备已被读走 8 字节");

    // 边读边补：读取提交 → advance_read 驱动输入泵重复拉取，读空后立即补满。
    let mut total = Vec::new();
    loop {
        let demand = Demand::at_least(1);
        let some = TrBuffTryRead::try_read(&mut rx, &demand);
        let mut rs = match some.pick_left() {
            Some(s) => s,
            None => break, // 输入已耗尽且缓冲已空
        };
        let n = rs.least_count();
        let got = take_segm(&mut rs, n);
        drop(rs); // 提交 → advance_read → 驱动输入泵补位
        total.extend(got);
        // 输入未耗尽时，读取提交后缓冲应立即补满。
        let p = pos.load(Ordering::Relaxed);
        if p < 20 {
            assert_eq!(rx.data_size(), 8, "advance_read 应驱动输入泵补满空位");
        }
    }
    assert_eq!(total, (0..20).collect::<Vec<_>>(), "读回全部输入");
    assert_eq!(rx.data_size(), 0);
    assert_eq!(pos.load(Ordering::Relaxed), 20, "输入设备已全部读完");
    assert_eq!(data.lock().unwrap().len(), 20);
}

dual_runtime_test_!(pipe_from_input_fills_and_refills_);

/// 被动生产 × 主动消费：写入提交（`advance_write`）驱动主动消费者**重复推送**
/// 到 `TrOutput`——写入后立即排空。
/// - 手段：构建后写 3 字节再连续三轮各写 2 字节。
/// - 判断：每次提交后输出设备立即拿到全部数据，缓冲始终为 0。
///
/// `build_async` 只返回生产端半部——主动消费端由设备驱动，不产出读半部。
async fn pipe_into_output_drains_on_write_() {
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .producer_passive()
        .pipe_into_output(output);
    let mut tx = ready.build_async().await.unwrap();

    // 写 3 字节 → 写段 drop 提交 → advance_write 驱动输出泵排空。
    let demand = Demand::at_least(3);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .unwrap();
    fill_segm(&mut ws, &[1, 2, 3]);
    drop(ws);
    assert_eq!(
        *out_data.lock().unwrap(),
        vec![1, 2, 3],
        "写入提交后应立即排空到输出设备"
    );
    assert_eq!(tx.data_size(), 0, "排空后缓冲应为空");

    // 连续多次写入：每次都即时排空。
    for chunk in 0..3u8 {
        let demand = Demand::at_least(2);
        let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
            .pick_left()
            .unwrap();
        fill_segm(&mut ws, &[chunk * 10 + 1, chunk * 10 + 2]);
        drop(ws);
    }
    assert_eq!(
        *out_data.lock().unwrap(),
        vec![1, 2, 3, 1, 2, 11, 12, 21, 22]
    );
}

dual_runtime_test_!(pipe_into_output_drains_on_write_);

/// 主动 × 主动：`TrInput → 缓冲 → TrOutput` 流水线。`build_async` 返回
/// [`Pipeline`] future——由设备驱动把当前可用的输入全部流到输出。
/// - 手段：容量 8、输入 20 字节；用 `select(流水线, 探测)` 让运行时左优先反复
///   轮询流水线，探测直到输入被读空为止。
/// - 判断：输出设备收到 0..20；输入被读走 20 字节。
async fn pipe_both_active_pipeline_() {
    let input = TestInput::new((0..20).collect());
    let pos = input.pos.clone();
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_from_input(input)
        .pipe_into_output(output);
    let mut pipeline = ready.build_async().await.unwrap();

    // 首个轮询即由输入泵 + 输出泵把整个流水线跑完（非阻塞设备立即就绪）。
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
    assert_eq!(pos.load(Ordering::Relaxed), 20, "输入设备已全部读完");
}

dual_runtime_test_!(pipe_both_active_pipeline_);

/// 阻塞式输入设备：无数据时 `read_async` 挂起（注册 waker）；调用方经 `Arc`
/// 压入数据并唤醒后，下一次 poll 即有数据可读。用于验证「由设备驱动」的
/// 流水线流动。
struct BlockingInput {
    data: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    pos: usize,
    waker: std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
}

impl BlockingInput {
    #[allow(clippy::type_complexity)]
    fn new() -> (
        Self,
        std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    ) {
        let data = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let waker = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            BlockingInput {
                data: data.clone(),
                pos: 0,
                waker: waker.clone(),
            },
            data,
            waker,
        )
    }
}

/// [`BlockingInput`] 的读 future（设备侧的 `poll` 实现：无数据即注册 waker 并
/// 挂起——这是**被测设备的语义**，不是测试在手动驱动被测 future）。
struct BlockingRead<'f> {
    input: &'f mut BlockingInput,
    target: &'f mut [core::mem::MaybeUninit<u8>],
}

impl core::future::Future for BlockingRead<'_> {
    type Output = SomeOf<usize, TestErr>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        let this = &mut *self;
        let data = this.input.data.lock().unwrap();
        let avail = data.len().saturating_sub(this.input.pos);
        if avail == 0 {
            // 无数据：注册 waker 并挂起；数据到达时由 push 侧唤醒。
            *this.input.waker.lock().unwrap() = Some(cx.waker().clone());
            return core::task::Poll::Pending;
        }
        let n = core::cmp::min(this.target.len(), avail);
        for (i, slot) in this.target[..n].iter_mut().enumerate() {
            *slot = core::mem::MaybeUninit::new(data[this.input.pos + i]);
        }
        this.input.pos += n;
        core::task::Poll::Ready(SomeOf::new_left(n))
    }
}

impl<'a> TrMayCancel<'a> for BlockingRead<'a> {
    type MayCancelFuture<'f, C> = BlockingRead<'a>
    where
        'f: 'a,
        Self: 'f,
        C: 'f + TrCancellationToken;

    type MayCancelOutput = SomeOf<usize, TestErr>;

    fn may_cancel_with<C>(self, _: C) -> Self::MayCancelFuture<'a, C>
    where
        C: 'a + TrCancellationToken,
    {
        self
    }
}

impl abs_buff::io::TrInput<u8> for BlockingInput {
    type ReadAsync<'f> = BlockingRead<'f> where Self: 'f;
    type Err = TestErr;

    fn read_async<'f>(
        &'f mut self,
        target: &'f mut [core::mem::MaybeUninit<u8>],
    ) -> Self::ReadAsync<'f> {
        BlockingRead {
            input: self,
            target,
        }
    }
}

/// 双端全主动：数据流动**由两端设备驱动**——`Pipeline` future 存活期间持续
/// 搬运（`pipeline.pipe_async()` 交出由设备驱动的流水线 future）。
/// - 手段：让流水线与「探测」future 在同一个运行时任务里用 `select` 并发；
///   探测先让出一次（确认无数据时输出为空），再分两批压入数据并**唤醒设备**，
///   每次唤醒后让出一次，把控制权交回运行时——由运行时 waker 驱动流水线。
/// - 判断：无数据时输出为空；每批数据被压入并唤醒后，输出设备依次拿到
///   `[1,2,3]` 与 `[1,2,3,4,5]`（数据自动流动，无需任何显式 drive）。
async fn pipeline_flows_driven_by_devices_() {
    let (input, in_data, in_waker) = BlockingInput::new();
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_between(input, output);
    let mut pipeline = ready.build_async().await.unwrap();

    let drive = pipeline.pipe_async().into_future();
    let probe = async {
        // 先让出一次：确保流水线已被轮询并挂在输入设备上（注册 waker）。
        futures_lite::future::yield_now().await;
        assert!(
            out_data.lock().unwrap().is_empty(),
            "输入设备无数据时不应有输出"
        );

        // 设备就绪（压入数据 + 唤醒）：流水线由输入设备驱动，数据自动流到输出。
        in_data.lock().unwrap().extend_from_slice(&[1, 2, 3]);
        if let Some(w) = in_waker.lock().unwrap().take() {
            w.wake();
        }
        futures_lite::future::yield_now().await;
        assert_eq!(
            *out_data.lock().unwrap(),
            vec![1, 2, 3],
            "设备就绪后数据应由运行时 waker 驱动自动流动"
        );

        // 第二批数据：同样自动流动（输入设备再次就绪）。
        in_data.lock().unwrap().extend_from_slice(&[4, 5]);
        if let Some(w) = in_waker.lock().unwrap().take() {
            w.wake();
        }
        futures_lite::future::yield_now().await;
        assert_eq!(*out_data.lock().unwrap(), vec![1, 2, 3, 4, 5]);
    };
    futures_util::pin_mut!(drive, probe);
    let _ = futures_util::future::select(drive, probe).await;
}

dual_runtime_test_!(pipeline_flows_driven_by_devices_);

/// 主动生产端在输入耗尽后停止泵入；再次消费时不再有数据。
/// - 手段：输入仅 3 字节、容量 8；构建后读空。
/// - 判断：累计读出 `[1,2,3]`；缓冲为 0；输入被读走 3 字节。
async fn pipe_from_input_stops_when_exhausted_() {
    let input = TestInput::new(vec![1, 2, 3]);
    let pos = input.pos.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_from_input(input)
        .consumer_passive();
    let mut rx = ready.build_async().await.unwrap();

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
    assert_eq!(total, vec![1, 2, 3]);
    assert_eq!(rx.data_size(), 0);
    assert_eq!(pos.load(Ordering::Relaxed), 3);
}

dual_runtime_test_!(pipe_from_input_stops_when_exhausted_);

/// 主动端的半部错误类型（编译期检查 Unavailable 变体存在且可达）。
#[allow(dead_code)]
fn _assert_unavailable_errors() {
    let _ = ProducerError::<usize>::Unavailable;
    let _ = ConsumerError::<usize>::Unavailable;
}

/// 一个「数据迟到」的输入设备：`gate` 置位后才开始供数，并记录 `read_async`
/// 的调用次数（用于验证半部操作的自动驱动）。
struct GatedInput {
    data: Vec<u8>,
    pos: usize,
    gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl abs_buff::io::TrInput<u8> for GatedInput {
    type ReadAsync<'f>
        = super::ReadySegm<usize, super::TestErr>
    where
        Self: 'f;
    type Err = super::TestErr;

    fn read_async<'f>(
        &'f mut self,
        target: &'f mut [core::mem::MaybeUninit<u8>],
    ) -> Self::ReadAsync<'f> {
        use std::sync::atomic::Ordering as O;
        self.calls.fetch_add(1, O::Relaxed);
        if !self.gate.load(O::Acquire) || self.pos >= self.data.len() {
            return super::ReadySegm::new(
                abs_buff::x_deps::anylr::SomeOf::new_left(0),
            );
        }
        let n = core::cmp::min(target.len(), self.data.len() - self.pos);
        for (i, slot) in target[..n].iter_mut().enumerate() {
            *slot = core::mem::MaybeUninit::new(self.data[self.pos + i]);
        }
        self.pos += n;
        super::ReadySegm::new(abs_buff::x_deps::anylr::SomeOf::new_left(n))
    }
}

/// 对端（生产端）为主动时，`try_read` **自动**驱动一轮输入泵：缓冲为空、
/// 设备数据「迟到」（构造后才可用）时，单次 `try_read` 即拉到数据——
/// 调用者无需任何手动 drive（无后台任务模型下「操作即事件」）。
/// - 手段：门控输入设备（构造期门未开）；开门后立即 `try_read`。
/// - 判断：开门前缓冲为 0、泵调用 1 次；开门后单次 `try_read` 拿到 `[7,8,9]`；
///   供完后再次 `try_read` 返回 `Drained`。
async fn try_read_auto_drives_active_producer_() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize},
    };

    let gate = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let input = GatedInput {
        data: vec![7, 8, 9],
        pos: 0,
        gate: gate.clone(),
        calls: calls.clone(),
    };

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_from_input(input)
        .consumer_passive();
    let mut rx = ready.build_async().await.unwrap();

    // 构造期 start() 泵了一轮，但门未开 → 缓冲为空。
    assert_eq!(rx.data_size(), 0);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // 数据「迟到」：门打开后，单次 try_read 自动驱动输入泵并读到数据。
    gate.store(true, Ordering::Release);
    let demand = Demand::at_least(3);
    let some = TrBuffTryRead::try_read(&mut rx, &demand);
    let mut rs = some.pick_left().expect("try_read 应自动拉到数据");
    assert_eq!(rs.least_count(), 3);
    assert_eq!(take_segm(&mut rs, 3), vec![7, 8, 9]);
    drop(rs);

    // 设备已供完：后续 try_read 自动驱动一次（读到 Drained），不阻塞。
    assert!(
        calls.load(Ordering::Relaxed) >= 3,
        "try_read 应自动驱动输入泵"
    );
    let demand = Demand::at_least(1);
    let some = TrBuffTryRead::try_read(&mut rx, &demand);
    assert!(
        matches!(some.pick_right(), Some(ConsumerError::Drained(_))),
        "设备无更多数据时 try_read 返回 Drained"
    );
}

dual_runtime_test_!(try_read_auto_drives_active_producer_);

/// 写端关闭（`ProducerClose` 事件）：`close_tx` 先驱动输出泵排空残留再触发
/// 事件——写端关闭后不再有新数据，残留必须送达输出设备。
/// - 手段：写 5 字节、再写 2 字节，最后 `close`。
/// - 判断：每次提交后输出设备立即拿到数据；`close` 后输出保持完整 7 字节。
async fn close_tx_drains_remaining_output_() {
    let output = TestOutput::new();
    let out_data = output.data.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .producer_passive()
        .pipe_into_output(output);
    let mut tx = ready.build_async().await.unwrap();

    // 写 5 字节（一次借出可写区，全部写入并提交）——advance_write 已排空。
    let demand = Demand::at_least(5);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .expect("应有 5 格可写空间");
    fill_segm(&mut ws, &[1, 2, 3, 4, 5]);
    drop(ws);
    assert_eq!(
        *out_data.lock().unwrap(),
        vec![1, 2, 3, 4, 5],
        "写入提交即排空"
    );

    // 再写 2 字节后 close：写入已排空，close 的排空路径对空缓冲是 no-op。
    let demand = Demand::at_least(2);
    let mut ws = TrBuffTryWrite::try_write(&mut tx, &demand)
        .pick_left()
        .expect("应有 2 格可写空间");
    fill_segm(&mut ws, &[6, 7]);
    drop(ws);
    assert_eq!(*out_data.lock().unwrap(), vec![1, 2, 3, 4, 5, 6, 7]);

    tx.close();
    assert_eq!(*out_data.lock().unwrap(), vec![1, 2, 3, 4, 5, 6, 7]);
}

dual_runtime_test_!(close_tx_drains_remaining_output_);

/// 主动生产 × 被动消费的**异步读**：构造（`init_async` 初始搬运）已把缓冲填满，
/// 因此第一次 `read_async` **无需任何泵驱动**即成功——等待逻辑保持纯粹（只
/// 关心本端需求）；随后每次读取提交（`advance_read`）驱动输入泵补位，后续
/// `read_async` 同样立即成功。
/// - 手段：容量 8、输入 20 字节；用 `select(读等待, 让出一次)` 左优先断言读等待
///   在首轮即就绪（若 park 则会走到让出侧而失败——有界失败，不永久挂起）。
/// - 判断：第一次读借出 8 字节 `0..8`；提交后缓冲立即补满（`data_size == 8`、
///   输入被读走 16）。
async fn read_async_with_active_producer_has_data_() {
    let input = TestInput::new((0..20).collect());
    let pos = input.pos.clone();

    let mut ready = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_from_input(input)
        .consumer_passive();
    let mut rx = ready.build_async().await.unwrap();

    // 构造即已填满：第一次 read_async 必有数据（无需等待 / 泵驱动）。
    // 内层作用域：等待 future 持有 `&mut rx`，读完后立即释放借用。
    {
        let demand = Demand::at_least(8);
        let read = rx.read_async(&demand).into_future();
        let probe = async { futures_lite::future::yield_now().await };
        futures_util::pin_mut!(read, probe);
        let res = match futures_util::future::select(read, probe).await {
            futures_util::future::Either::Left((res, _)) => res,
            futures_util::future::Either::Right(_) => {
                panic!("构造已填满：read_async 不应 park")
            }
        };
        let mut rs = res.pick_left().expect("构造已填满：第一次读应有数据");
        assert_eq!(rs.least_count(), 8);
        assert_eq!(take_segm(&mut rs, 8), (0..8).collect::<Vec<_>>());
        drop(rs); // 提交 → advance_read → 驱动输入泵补位
    }

    // 读取提交后缓冲立即补满（advance_read 驱动重复拉取）。
    assert_eq!(rx.data_size(), 8, "advance_read 应驱动输入泵补满空位");
    assert_eq!(pos.load(Ordering::Relaxed), 16, "输入设备已被读走 16 字节");
}

dual_runtime_test_!(read_async_with_active_producer_has_data_);

// ---------------------------------------------------------------------------
// 双头测试设备：同时实现 TrInput 与 TrOutput，观察真实数据流动
// ---------------------------------------------------------------------------

/// 双头测试设备：**同时实现 [`TrInput`] 与 [`TrOutput`]**，可被两个管道
/// （`pipe_async`）分别接到两个其他设备，充当「中间人」，让测试在设备内部
/// 观察到真实的数据流动。
///
/// * 作为 `TrOutput`（上游 pipe 的消费端）：把收到的数据追加进 `seen_in`
///   （流入记录），并**转发**到 `out_buf`（供 `TrInput` 侧读取），同时唤醒
///   等待中的读者——模拟「收到数据 → 有数据可读」的真实语义；
/// * 作为 `TrInput`（下游 pipe 的生产端）：从 `out_buf` 供数（追加进
///   `seen_out` 流出记录）；`out_buf` 为空时返回 `Pending` 并注册 waker
///   （**阻塞式设备语义**，与真实连接如 iroh 一致：无数据即挂起，而非空转）。
///
/// 内部状态全部经 `Arc` 共享，因此 `Clone` 后可同时放入两个管道。
#[derive(Clone)]
struct DualHeadDevice {
    /// 作为 `TrOutput` 收到的全部数据（上游流入本设备的记录）。
    seen_in: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    /// 作为 `TrInput` 供出的全部数据（本设备转交给下游的记录）。
    seen_out: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    /// 转发缓冲：`TrOutput` 收到的数据追加于此，`TrInput` 从这里读取。
    out_buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    /// `TrInput` 侧的读取位置。
    out_pos: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// 等待中的读者（`TrInput` 侧）——`out_buf` 从空变有数据时被唤醒。
    reader_waker: std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
}

impl DualHeadDevice {
    fn new() -> Self {
        DualHeadDevice {
            seen_in: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            seen_out: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            out_buf: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            out_pos: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            reader_waker: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

/// 双头设备作为 `TrInput` 的读 future：`out_buf` 有数据即拷贝并推进读取位置；
/// 为空则注册 waker 并挂起（阻塞式设备语义——设备侧的 `poll` 实现，不是测试
/// 在手动驱动被测 future）。
struct DualHeadRead<'f> {
    dev: &'f mut DualHeadDevice,
    target: &'f mut [core::mem::MaybeUninit<u8>],
}

impl core::future::Future for DualHeadRead<'_> {
    type Output = SomeOf<usize, TestErr>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        let this = &mut *self;
        let buf = this.dev.out_buf.lock().unwrap();
        let pos = this.dev.out_pos.load(Ordering::Relaxed);
        let avail = buf.len().saturating_sub(pos);
        if avail == 0 {
            // 暂无数据：注册 waker 挂起；`TrOutput` 写入时唤醒。
            *this.dev.reader_waker.lock().unwrap() = Some(cx.waker().clone());
            return core::task::Poll::Pending;
        }
        let n = core::cmp::min(this.target.len(), avail);
        for (i, slot) in this.target[..n].iter_mut().enumerate() {
            *slot = core::mem::MaybeUninit::new(buf[pos + i]);
        }
        this.dev.out_pos.store(pos + n, Ordering::Relaxed);
        this.dev
            .seen_out
            .lock()
            .unwrap()
            .extend_from_slice(&buf[pos..pos + n]);
        core::task::Poll::Ready(SomeOf::new_left(n))
    }
}

impl<'a> TrMayCancel<'a> for DualHeadRead<'a> {
    type MayCancelFuture<'f, C> = DualHeadRead<'f>
    where
        'f: 'a,
        Self: 'f,
        C: 'f + TrCancellationToken;

    type MayCancelOutput = SomeOf<usize, TestErr>;

    fn may_cancel_with<C>(self, _: C) -> Self::MayCancelFuture<'a, C>
    where
        C: 'a + TrCancellationToken,
    {
        self
    }
}

impl TrInput<u8> for DualHeadDevice {
    type ReadAsync<'f> = DualHeadRead<'f> where Self: 'f;
    type Err = TestErr;

    fn read_async<'f>(
        &'f mut self,
        target: &'f mut [core::mem::MaybeUninit<u8>],
    ) -> Self::ReadAsync<'f> {
        DualHeadRead { dev: self, target }
    }
}

impl TrOutput<u8> for DualHeadDevice {
    type WriteAsync<'f> = ReadySegm<usize, TestErr> where Self: 'f;
    type Err = TestErr;

    fn write_async<'f>(
        &'f mut self,
        source: &'f [core::mem::MaybeUninit<u8>],
    ) -> Self::WriteAsync<'f> {
        let n = source.len();
        let mut chunk: Vec<u8> = source
            .iter()
            .map(|m| unsafe { m.assume_init_read() })
            .collect();
        self.seen_in.lock().unwrap().extend_from_slice(&chunk);
        self.out_buf.lock().unwrap().append(&mut chunk);
        // 收到数据 → 有数据可读：唤醒等待中的读者（下游 pipe 的输入等待）。
        if let Some(w) = self.reader_waker.lock().unwrap().take() {
            w.wake();
        }
        ReadySegm::new(SomeOf::new_left(n))
    }
}

/// 双主动流水线（`pipe_async`）**由两端设备驱动**：数据真实地在
/// 「源设备 → 双头设备 → 终端设备」之间流动，且双头设备（同时实现
/// `TrInput` 与 `TrOutput`）内部可观察到流经的数据（`seen_in` / `seen_out`）。
/// - 手段：`source`（`TestInput`，20 字节）经 `pipe1` 接到双头设备的 `TrOutput`
///   侧；双头设备的 `TrInput` 侧经 `pipe2` 接到 `sink`（`TestOutput`）。两个
///   永不结束的 pipe future 在同一运行时任务里用 `join!` 并发，并与「探测」
///   future 用 `select` 并发——探测让出直到终端设备收满 20 字节为止。
/// - 判断：(1) `seen_in == 0..20`——源数据全部流入双头设备；(2) `sink.data ==
///   0..20` 且 `seen_out == 0..20`——数据经双头设备转交终端。三处断言共同证明
///   数据真的流经双头设备，而非停留在任一管道内部。
async fn dual_head_device_observes_cross_pipe_flow_() {
    let source = TestInput::new((0..20).collect());
    let sink = TestOutput::new();
    let out_data = sink.data.clone(); // sink 将 move 进 pipe2，观察侧保留 Arc
    let dual = DualHeadDevice::new();

    // pipe1：source（TrInput）→ dual（TrOutput）——数据流入双头设备。
    let mut ready1 = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_from_input(source)
        .pipe_into_output(dual.clone());
    let mut pipe1 = ready1.build_async().await.unwrap();
    // pipe2：dual（TrInput）→ sink（TrOutput）——数据从双头设备流出。
    let mut ready2 = DefaultBuilder::with_capacity(8)
        .unwrap()
        .pipe_from_input(dual.clone())
        .pipe_into_output(sink);
    let mut pipe2 = ready2.build_async().await.unwrap();

    // 两个管道并发推进：pipe2 先挂起在双头设备上（阻塞式设备），pipe1 随后把
    // 源数据搬进双头设备并唤醒 pipe2 的读者。
    let drive = async {
        futures_util::join!(
            pipe2.pipe_async().into_future(),
            pipe1.pipe_async().into_future()
        );
    };
    let stop = async {
        for _ in 0..64 {
            futures_lite::future::yield_now().await;
            if out_data.lock().unwrap().len() == 20 {
                return;
            }
        }
    };
    futures_util::pin_mut!(drive, stop);
    let _ = futures_util::future::select(drive, stop).await;

    assert_eq!(
        *dual.seen_in.lock().unwrap(),
        (0..20).collect::<Vec<_>>(),
        "pipe1 的数据应全部流入双头设备（seen_in 可观察）"
    );
    assert_eq!(
        *out_data.lock().unwrap(),
        (0..20).collect::<Vec<_>>(),
        "pipe2 的数据应全部流到终端设备"
    );
    assert_eq!(
        *dual.seen_out.lock().unwrap(),
        (0..20).collect::<Vec<_>>(),
        "双头设备已转交全部数据（seen_out 可观察）"
    );
}

dual_runtime_test_!(dual_head_device_observes_cross_pipe_flow_);

// ---------------------------------------------------------------------------
// Dev 端唤醒槽位 + STNDBY armed 协议（executor 驱动的泵 park 机制）
// ---------------------------------------------------------------------------

/// 主动端（`DevProducer` / `DevConsumer`）携带与被动端同构的唤醒槽位
/// （`wakeslot_`）。executor 驱动的泵在「无事可做」（缓冲满 / 空、设备阻塞）
/// 时把 waker 注册进主动端自身的槽位并 armed（`TX_STNDBY` / `RX_STNDBY`，
/// 经核心 `arm_producer` / `arm_consumer`）；对端提交路径的 fire 侧**只唤醒
/// 不搬运**——armed 才 `check`，`check` 感兴趣即 `signal` 唤醒泵。
///
/// 本用例测的是 `check` / `signal` 的**同步协议**（不涉及任何 future 驱动），
/// 因此保持 `#[test]`：用 [`TestWaker`] 充当唤醒的**接收端**（`AtomicBool`
/// 置位），不轮询任何 future。
/// - 手段：把已注册 waker 的 `Waiter` 注册进槽位，再分别触发端级 `check`、
///   核心级 `advance_write`、以及未 armed 的门控路径。
/// - 判断：(1) `check` 感兴趣并置位标志；(2) armed 后写入提交经
///   `fire_consumer → check → signal` 置位；(3) 未 armed 时 fire 不置位。
#[test]
fn dev_wakeslot_and_stndby_protocol() {
    // (1) 端级：check 感兴趣 → signal 自身唤醒槽位。
    let output = TestOutput::new();
    let dev = DevConsumer::new(output);
    let (waker, flag) = TestWaker::make_waker_tuple();
    let mut waiter = Waiter::new();
    waiter.waker = Some(waker);
    dev.wakeslot().register(&waiter); // 模拟泵已 park（注册进主动端槽位）
    assert!(
        dev.check(ConsumerHookEvent::Available(3)),
        "有数据可消费时 check 应感兴趣"
    );
    assert!(
        flag.load(Ordering::Acquire),
        "check 感兴趣时应 signal 唤醒已注册的泵"
    );
    dev.wakeslot().deregister(&waiter);

    // (2) 核心级：armed 后写入提交 → fire_consumer → check → signal。
    let output = TestOutput::new();
    let dev = DevConsumer::new(output);
    let (waker, flag) = TestWaker::make_waker_tuple();
    let mut waiter = Waiter::new();
    waiter.waker = Some(waker);
    dev.wakeslot().register(&waiter);
    let buffer = Owned::new_uninit_slice(8, CoreAlloc);
    let core = CircCore::new(BufProducer::new(), dev, buffer);
    core.set_rx_standby(); // 泵 armed（RX_STNDBY）——fire 侧才可能唤醒
    let demand = Demand::at_least(3);
    let mut ws = core
        .try_write_(&demand)
        .pick_left()
        .expect("应可写");
    fill_segm(&mut ws, &[1, 2, 3]);
    drop(ws); // 段 drop → advance_write → fire_consumer(Available(3))
    assert!(
        flag.load(Ordering::Acquire),
        "armed 后写入提交应经 fire_consumer → check → signal 唤醒泵"
    );

    // (3) 未 armed：fire 门控不唤醒（无等待者 / 泵未 park）。
    let output = TestOutput::new();
    let dev = DevConsumer::new(output);
    let (waker, flag) = TestWaker::make_waker_tuple();
    let mut waiter = Waiter::new();
    waiter.waker = Some(waker);
    dev.wakeslot().register(&waiter);
    let buffer = Owned::new_uninit_slice(8, CoreAlloc);
    let core = CircCore::new(BufProducer::new(), dev, buffer);
    // 不 arm_consumer：fire_consumer 应直接返回，不访问 check / 槽位。
    let demand = Demand::at_least(3);
    let mut ws = core
        .try_write_(&demand)
        .pick_left()
        .expect("应可写");
    fill_segm(&mut ws, &[1, 2, 3]);
    drop(ws);
    assert!(
        !flag.load(Ordering::Acquire),
        "未 armed 时 fire 门控应不唤醒"
    );
}

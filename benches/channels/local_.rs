//! 单线程运行时 + `LocalSet`：生产者与消费者各是一个本地任务。
//!
//! 为什么用 `spawn_local` 而不是 `tokio::spawn`：buffex 由
//! `gen_may_cancel_future` 产出的 future 不满足 `Send`（`tests/async_runtime.rs`
//! 已记录 rust#100013），`tokio::spawn` 无法编译；`spawn_local` 不要求 `Send`，
//! 于是两种实现都能跑**同一形状**的驱动，通道成为唯一变量。
//!
//! 与 asyncband 官方基准的差别也在这里：官方基准用多线程运行时 + `tokio::spawn`，
//! 而这里是单线程 + 本地任务。为了让对比公平，**两侧实现都不 spawn 到多线程**。

use divan::Bencher;
use divan::counter::ItemsCount;

use super::adapters::BandMpsc;
use super::adapters::BuffexMpsc;
use super::adapters::BuffexSpsc;
use super::support::BATCH_MESSAGES;
use super::support::BOUNDED_CAPACITY;
use super::support::Chan;
use super::support::EXPECTED_CHECKSUM;
use super::support::MultiChan;
use super::support::PRODUCER_COUNTS;
use super::support::current_thread_runtime;
use super::support::run_local_multi_batch;
use super::support::run_local_single_batch;

/// 测「异步就绪往返」：起两个本地任务完成一次收发握手要多少时间。
///
/// - 手段：通道只建一次；每个样本把两端移进两个 `spawn_local` 任务、各做一次
///   异步收发，再把端点收回来供下一样本复用——于是样本里只有任务调度与通道
///   握手，不含通道构建。
/// - 判断：消费者断言载荷为 `7`；端点必须能如约归还（否则 panic）。
#[divan::bench(
    types = [BuffexSpsc, BuffexMpsc, BandMpsc],
    sample_size = 256,
    name = "local_round_trip"
)]
fn local_round_trip_<C: Chan>(bencher: Bencher) {
    let runtime = current_thread_runtime();
    let local = tokio::task::LocalSet::new();
    let (tx, rx) = runtime.block_on(C::make_(BOUNDED_CAPACITY));
    let mut endpoints = Some((tx, rx));
    bencher.bench_local(|| {
        let (tx, rx) = endpoints.take().expect("端点应当已归还");
        endpoints = Some(local.block_on(&runtime, round_trip_once_::<C>(tx, rx)));
    });
}

/// 一对端点上的一次异步往返，结束后把两端原样交还。
async fn round_trip_once_<C: Chan>(mut tx: C::Tx, mut rx: C::Rx) -> (C::Tx, C::Rx) {
    let producer = tokio::task::spawn_local(async move {
        C::send_(&mut tx, 7).await;
        tx
    });
    let consumer = tokio::task::spawn_local(async move {
        let value = C::recv_(&mut rx).await;
        assert_eq!(value, 7, "往返载荷应当一致");
        rx
    });
    let (tx, rx) = tokio::join!(producer, consumer);
    (
        tx.expect("生产者任务不应 panic"),
        rx.expect("消费者任务不应 panic"),
    )
}

/// 测 SPSC 的异步吞吐：单生产者 × 单消费者，每批 [`BATCH_MESSAGES`] 条。
///
/// - 手段：单线程运行时 + `LocalSet`，两端各一个本地任务；预热轮先跑一遍并核对
///   校验和，随后由 divan 采样（`counter` 按条数计，报出的是每条消息的耗时）。
/// - 判断：预热轮校验和必须等于 [`EXPECTED_CHECKSUM`]，否则「快」没有意义。
#[divan::bench(
    types = [BuffexSpsc, BandMpsc],
    sample_count = 50,
    sample_size = 1,
    counter = ItemsCount::new(BATCH_MESSAGES),
    name = "local_spsc_throughput"
)]
fn local_spsc_throughput_<C: Chan>(bencher: Bencher) {
    let runtime = current_thread_runtime();
    let local = tokio::task::LocalSet::new();
    let warmup = local.block_on(&runtime, run_local_single_batch::<C>());
    assert_eq!(warmup, EXPECTED_CHECKSUM, "预热轮校验和不符");
    bencher.bench_local(|| local.block_on(&runtime, run_local_single_batch::<C>()));
}

/// 测 MPSC 的异步吞吐：1 或 8 个生产者 × 单消费者，每批 [`BATCH_MESSAGES`] 条。
///
/// - 手段：同上，但生产者任务按 `PRODUCER_COUNTS` 档位起多个，每个负责一段
///   互不重叠的连续序号。
/// - 判断：预热轮校验和必须等于 [`EXPECTED_CHECKSUM`]——多生产者合起来仍是
///   完整的 `0..BATCH_MESSAGES`，一条不多一条不少。
#[divan::bench(
    types = [BuffexMpsc, BandMpsc],
    args = PRODUCER_COUNTS,
    sample_count = 50,
    sample_size = 1,
    counter = ItemsCount::new(BATCH_MESSAGES),
    name = "local_mpsc_throughput"
)]
fn local_mpsc_throughput_<C: MultiChan>(bencher: Bencher, producers: usize) {
    let runtime = current_thread_runtime();
    let local = tokio::task::LocalSet::new();
    let warmup = local.block_on(&runtime, run_local_multi_batch::<C>(producers));
    assert_eq!(warmup, EXPECTED_CHECKSUM, "预热轮校验和不符");
    bencher.bench_local(|| local.block_on(&runtime, run_local_multi_batch::<C>(producers)));
}

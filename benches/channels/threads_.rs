//! 多 OS 线程驱动：写端各自一个线程，读端留在主线程。
//!
//! 每个写端线程自带一个单线程运行时并 `block_on` 整个发送循环；读端在主线程
//! 的运行时里 `block_on` 接收循环。这是最贴近真实吞吐的形状（跨线程唤醒真的
//! 发生了），代价是引入线程调度噪声。
//!
//! [`ThreadBatch`](super::support::ThreadBatch) 让线程、运行时与通道跨样本复用，
//! 因此样本内只有消息往返——这一点与 asyncband 官方的 `RepeatedBatch` 一致。

use divan::Bencher;
use divan::black_box;
use divan::counter::ItemsCount;

use super::adapters::BandMpsc;
use super::adapters::BuffexMpsc;
use super::adapters::BuffexSpsc;
use super::support::BATCH_MESSAGES;
use super::support::Chan;
use super::support::EXPECTED_CHECKSUM;
use super::support::MultiChan;
use super::support::PRODUCER_COUNTS;
use super::support::ThreadBatch;

/// 测 SPSC 的跨线程吞吐：1 个写端线程 × 主线程读端。
///
/// - 手段：写端线程循环发送一批 [`BATCH_MESSAGES`] 条、批间在屏障上等待；主线程
///   读端在同一屏障放行后收满整批。线程与通道只建一次。
/// - 判断：预热轮校验和必须等于 [`EXPECTED_CHECKSUM`]。
#[divan::bench(
    types = [BuffexSpsc, BandMpsc],
    sample_count = 50,
    sample_size = 1,
    counter = ItemsCount::new(BATCH_MESSAGES),
    name = "thread_spsc_throughput"
)]
fn thread_spsc_throughput_<C: Chan>(bencher: Bencher) {
    let mut batch = ThreadBatch::<C>::new();
    let warmup = batch.run();
    assert_eq!(warmup, EXPECTED_CHECKSUM, "预热轮校验和不符");
    bencher.bench_local(|| black_box(batch.run()));
}

/// 测 MPSC 的跨线程吞吐：1 或 8 个写端线程 × 主线程读端。
///
/// - 手段：每个写端线程负责一段互不重叠的连续序号；其余同上。
/// - 判断：预热轮校验和必须等于 [`EXPECTED_CHECKSUM`]——跨线程也要求一条不丢。
#[divan::bench(
    types = [BuffexMpsc, BandMpsc],
    args = PRODUCER_COUNTS,
    sample_count = 50,
    sample_size = 1,
    counter = ItemsCount::new(BATCH_MESSAGES),
    name = "thread_mpsc_throughput"
)]
fn thread_mpsc_throughput_<C: MultiChan>(bencher: Bencher, producers: usize) {
    let mut batch = ThreadBatch::<C>::with_producers(producers);
    let warmup = batch.run();
    assert_eq!(warmup, EXPECTED_CHECKSUM, "预热轮校验和不符");
    bencher.bench_local(|| black_box(batch.run()));
}

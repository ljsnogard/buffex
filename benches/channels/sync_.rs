//! 同步就绪路径：不触发等待的「发一条 + 收一条」往返。
//!
//! 这一组测的是通道自身的**每次操作开销**——没有任务、没有 waker、没有线程
//! 切换，纯粹是 `try_send` / `recv` 的取位、写格、读格与提交成本。

use divan::Bencher;
use divan::black_box;

use super::adapters::BandMpsc;
use super::adapters::BuffexMpsc;
use super::adapters::BuffexSpsc;
use super::support::BOUNDED_CAPACITY;
use super::support::Chan;
use super::support::current_thread_runtime;

/// 测「已就绪」状态下一次收发往返的耗时。
///
/// - 手段：在容量 [`BOUNDED_CAPACITY`] 的通道上反复执行「尝试发一条 + 尝试收一条」，
///   队列始终在「空 → 一条 → 空」之间循环，因此两端都不会等待。
/// - 判断：每次尝试发送都必须成功、每次接收都必须拿到值（失败即断言 panic），
///   divan 报出的即单次往返的耗时分布。
#[divan::bench(
    types = [BuffexSpsc, BuffexMpsc, BandMpsc],
    sample_size = 512,
    name = "try_round_trip"
)]
fn try_round_trip_<C: Chan>(bencher: Bencher) {
    let runtime = current_thread_runtime();
    let (mut tx, mut rx) = runtime.block_on(C::make_(BOUNDED_CAPACITY));
    bencher.bench_local(|| {
        assert!(
            C::try_send_(&mut tx, black_box(usize::MAX)),
            "就绪路径的发送不应失败"
        );
        black_box(C::try_recv_(&mut rx)).expect("就绪路径应当收到数据");
    });
}

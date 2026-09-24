//! 把 buffex 与 asyncband 的通道适配到 [`Chan`] / [`MultiChan`]。
//!
//! # 对标关系
//!
//! | 基准里的一行 | 对应实现 | 说明 |
//! | --- | --- | --- |
//! | [`BuffexMpsc`] | `buffex::channels::MpscChannel` | 多写者 × 单读者有界队列 |
//! | [`BandMpsc`] | `asyncband::mpsc::bounded` | 同上；**也是 SPSC 一行的基线** |
//! | [`BuffexSpsc`] | `buffex::channels::SpscChannel` | 单写者 × 单读者有界队列 |
//!
//! # SPSC 一行为什么对 `BandMpsc`
//!
//! asyncband（0.7.2 与 main）都**没有** SPSC 实现：0.7.2 只有 `mpsc`，
//! main 新增的是 `spmc`（单生产者 × 多消费者）。因此 SPSC 一行取
//! `asyncband::mpsc::bounded` 的**单生产者用法**作为最接近的同族基线：两者在
//! 「有界 + 单消费者」上语义一致，差别只在 asyncband 没有 SPSC 专用优化路径。
//! 结论解读时必须记住这一点：SPSC 行的比值是「buffex SPSC vs asyncband MPSC」，
//! 而不是「SPSC vs SPSC」。

use asyncband::mpsc::BoundedReceiver as BandReceiver;
use asyncband::mpsc::BoundedSender as BandSender;
use asyncband::mpsc::bounded as band_bounded;
use buffex::channels::MpscChannel;
use buffex::channels::MpscReceiver;
use buffex::channels::MpscSender;
use buffex::channels::SpscChannel;
use buffex::channels::SpscReceiver;
use buffex::channels::SpscSender;
use buffex::circular_buff::CoreAlloc;
use buffex::x_deps::abs_async_iter::TrAsyncIterator;

use super::support::Chan;
use super::support::MultiChan;

/// buffex 的多写者 × 单读者有界队列（写者上界取 8，与生产者档位上限一致）。
pub struct BuffexMpsc;

/// buffex 的单写者 × 单读者有界队列。
pub struct BuffexSpsc;

/// asyncband 的有界 mpsc（同时充当 SPSC 一行的基线）。
pub struct BandMpsc;

impl Chan for BuffexMpsc {
    type Tx = MpscSender<usize, CoreAlloc>;
    type Rx = MpscReceiver<usize, CoreAlloc>;

    async fn make_(capacity: usize) -> (Self::Tx, Self::Rx) {
        MpscChannel::<usize, 8>::with_capacity(capacity)
            .expect("容量合法")
            .into_parts()
            .await
            .expect("装配成功")
    }

    fn try_send_(tx: &mut Self::Tx, value: usize) -> bool {
        tx.try_send(value).is_ok()
    }

    fn try_recv_(rx: &mut Self::Rx) -> Option<usize> {
        rx.recv().ok().flatten()
    }

    async fn send_(tx: &mut Self::Tx, value: usize) {
        tx.send_async(value).await.expect("发送应当成功");
    }

    async fn recv_(rx: &mut Self::Rx) -> usize {
        rx.next_async().await.expect("接收应当成功")
    }
}

impl MultiChan for BuffexMpsc {
    fn clone_tx_(tx: &Self::Tx) -> Self::Tx {
        tx.clone()
    }
}

impl Chan for BuffexSpsc {
    type Tx = SpscSender<usize, CoreAlloc>;
    type Rx = SpscReceiver<usize, CoreAlloc>;

    async fn make_(capacity: usize) -> (Self::Tx, Self::Rx) {
        SpscChannel::with_capacity(capacity)
            .expect("容量合法")
            .into_parts::<usize, CoreAlloc>()
            .await
            .expect("装配成功")
    }

    fn try_send_(tx: &mut Self::Tx, value: usize) -> bool {
        tx.try_send(value).is_ok()
    }

    fn try_recv_(rx: &mut Self::Rx) -> Option<usize> {
        rx.recv().ok().flatten()
    }

    async fn send_(tx: &mut Self::Tx, value: usize) {
        tx.send_async(value).await.expect("发送应当成功");
    }

    async fn recv_(rx: &mut Self::Rx) -> usize {
        rx.next_async().await.expect("接收应当成功")
    }
}

impl Chan for BandMpsc {
    type Tx = BandSender<usize>;
    type Rx = BandReceiver<usize>;

    async fn make_(capacity: usize) -> (Self::Tx, Self::Rx) {
        band_bounded(capacity)
    }

    fn try_send_(tx: &mut Self::Tx, value: usize) -> bool {
        tx.try_send(value).is_ok()
    }

    fn try_recv_(rx: &mut Self::Rx) -> Option<usize> {
        rx.try_recv().ok()
    }

    async fn send_(tx: &mut Self::Tx, value: usize) {
        tx.send(value).await.expect("发送应当成功");
    }

    async fn recv_(rx: &mut Self::Rx) -> usize {
        rx.recv().await.expect("接收应当成功")
    }
}

impl MultiChan for BandMpsc {
    fn clone_tx_(tx: &Self::Tx) -> Self::Tx {
        tx.clone()
    }
}

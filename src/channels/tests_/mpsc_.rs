//! MPSC 队列（[`MpscChannel`](crate::channels::MpscChannel)）的测试。

use core::future::IntoFuture;
use std::{string::String, string::ToString, vec, vec::Vec};

use abs_async_iter::TrAsyncIterator;

use crate::channels::{ChannelError, MpscChannel, MpscReceiver, MpscSender};
use crate::test_support_::dual_runtime_test_;

/// 装配一个默认形态的 MPSC 队列（测试公共前置）。
async fn new_mpsc_<T: Send + Sync + 'static, const S: usize>(
    cap: usize,
) -> (MpscSender<T>, MpscReceiver<T>) {
    MpscChannel::<T, S>::with_capacity(cap)
        .expect("容量合法")
        .into_parts()
        .await
        .expect("装配成功")
}

/// 测试 MPSC 队列「共享同一块环形缓冲」的基本读写闭环。
/// - 手段：以「2 个写者、容量 8」构建，克隆写端得到两个独立写者，各发 1 条。
/// - 判断：读出顺序与发出顺序一致——证明多个写端指向同一块承载。
async fn mpsc_two_senders_share_one_buffer_() {
    let (mut tx1, mut rx) = new_mpsc_::<u8, 2>(8).await;
    let mut tx2 = tx1.clone();
    tx1.try_send(1).expect("写者 1");
    tx2.try_send(2).expect("写者 2");
    assert_eq!(rx.recv().expect("接收"), Some(1));
    assert_eq!(rx.recv().expect("接收"), Some(2));
    assert_eq!(rx.recv().expect("队空"), None);
}

dual_runtime_test_!(mpsc_two_senders_share_one_buffer_);


/// 测试写者数量上界为 0 时构造期即报错。
/// - 手段：以 `MpscChannel::<u8, 0>::with_capacity(8)` 构造。
/// - 判断：返回 [`ChannelError::Argument`]。
async fn mpsc_rejects_zero_senders_() {
    let e = MpscChannel::<u8, 0>::with_capacity(8).err();
    assert!(matches!(
        e,
        Some(crate::channels::MpscBuildError::Channel(
            ChannelError::Argument
        ))
    ));
}

dual_runtime_test_!(mpsc_rejects_zero_senders_);


/// 测试**写操作不可分割**：没有任何公开 API 能「只取写入位、不提交」。
/// - 手段：两个写者交错发送 6 条（各自 3 条），全部发完后再统一读取。
/// - 判断：6 条一条不少、一条不重，且容量记账正确。
async fn mpsc_writes_are_indivisible_() {
    let (mut tx1, mut rx) = new_mpsc_::<u8, 2>(8).await;
    let mut tx2 = tx1.clone();
    for i in 0..3u8 {
        tx1.try_send(i).expect("写者 1");
        tx2.try_send(100 + i).expect("写者 2");
    }
    assert_eq!(tx1.data_size(), 6, "6 条都已提交");
    let mut got = Vec::new();
    while let Some(v) = rx.recv().expect("接收") {
        got.push(v);
    }
    assert_eq!(got.len(), 6, "一条不少、一条不重");
    assert_eq!(rx.data_size(), 0);
}

dual_runtime_test_!(mpsc_writes_are_indivisible_);


/// 测试两端都实现 [`TrAsyncIterator`]（读端产出载荷，写端产出写票）。
/// - 手段：用写票发送；用读端 `next_async` 取载荷；关闭后再取一次。
/// - 判断：按序拿到载荷；关闭并读空后返回 [`ChannelError::Closing`]。
async fn mpsc_both_ends_implement_async_iter_() {
    let (mut tx, mut rx) = new_mpsc_::<u8, 1>(8).await;
    async {
        let ticket = tx.next_async().await.expect("应当拿到写票");
        ticket.send(5).expect("发送");
    }.await;
    let got = async {
        rx.next_async().await.expect("应当拿到载荷")
    }.await;
    assert_eq!(got, 5);

    async {
        tx.close_async().into_future().await.expect("关闭成功")
    }.await;
    let closed = async { rx.next_async().await }.await;
    assert!(matches!(closed, Err(ChannelError::Closing)));
}

dual_runtime_test_!(mpsc_both_ends_implement_async_iter_);


/// 测试 MPSC 的 bulk 帧语义与 SPSC 一致（含部分填充与空洞丢弃）。
/// - 手段：声称 3 条的帧只填 2 条，再发一条单体。
/// - 判断：读端拿到已写前缀与随后的单体消息，空洞被静默丢弃。
async fn mpsc_bulk_partial_delivers_written_prefix_() {
    let (mut tx, mut rx) = new_mpsc_::<String, 2>(16).await;
    let written = tx
        .try_send_bulk(3, |w| {
            w.push("x".to_string());
            w.push("y".to_string());
            assert_eq!(w.written(), 2);
        })
        .expect("预留成功");
    assert_eq!(written, 2);
    tx.try_send("z".to_string()).expect("单体");

    assert_eq!(rx.recv().expect("接收"), Some("x".to_string()));
    assert_eq!(rx.recv().expect("接收"), Some("y".to_string()));
    assert_eq!(rx.recv().expect("接收"), Some("z".to_string()));
    assert_eq!(rx.recv().expect("队空"), None);
}

dual_runtime_test_!(mpsc_bulk_partial_delivers_written_prefix_);


/// 测试 bulk 填充**期间**写许可确实被占：另一位写者此刻进不来。
/// - 手段：写者 1 在 `try_send_bulk` 的闭包里填充，同时在闭包内让写者 2 `try_send`。
/// - 判断：闭包内写者 2 返回 [`ChannelError::Stuffed`]（写入位被占），闭包结束后写者
///   2 才能写入。这钉住「填充发生在写许可保护之下」，也说明两段不可能重叠。
async fn mpsc_bulk_fill_holds_the_write_slot_() {
    let (mut tx1, mut rx) = new_mpsc_::<u8, 2>(16).await;
    let mut tx2 = tx1.clone();
    tx1.try_send_bulk(2, |w| {
        assert!(w.push(1));
        assert_eq!(
            tx2.try_send(9).expect_err("填充期间写入位被写者 1 占着"),
            ChannelError::Stuffed
        );
    })
    .expect("预留成功");
    tx2.try_send(2).expect("填充结束后可以写");

    assert_eq!(rx.recv().expect("接收"), Some(1));
    assert_eq!(rx.recv().expect("接收"), Some(2));
}

dual_runtime_test_!(mpsc_bulk_fill_holds_the_write_slot_);


/// 测试队满时的背压表现。
/// - 手段：以容量 2 写满后用 `try_send` 再发一条。
/// - 判断：返回 [`ChannelError::Stuffed`]。
async fn mpsc_full_reports_stuffed_() {
    let (mut tx, _rx) = new_mpsc_::<u8, 1>(2).await;
    tx.try_send(1).expect("第 1 条");
    tx.try_send(2).expect("第 2 条");
    assert_eq!(tx.try_send(3).expect_err("已满"), ChannelError::Stuffed);
}

dual_runtime_test_!(mpsc_full_reports_stuffed_);


/// 测试写端 `next_async` 在队满时**在真实运行时里等待**，读端腾出空间后拿到写票。
/// - 手段：容量 2 写满；把「等写票并发送」与「先让出一次、再读走两条」放在同一个
///   运行时任务里并发执行（`join!`），让发送者先撞上「满」。
/// - 判断：写票最终到手、三条数据一条不少——若 `next_async` 不等待而是立刻失败，
///   这里就会失败。**不手动轮询**，交给运行时的 waker 驱动。
async fn mpsc_send_next_async_waits_for_space_() {
    let (mut tx, mut rx) = new_mpsc_::<u8, 1>(2).await;
    tx.try_send(1).expect("第 1 条");
    tx.try_send(2).expect("第 2 条");

    let send = async {
        let ticket = tx.next_async().await.expect("应当等到写票");
        ticket.send(3).expect("发送");
        tx
    };
    let recv = async {
        // 先让出一次，确保发送者已经试过并且挂在等待上。
        futures_lite::future::yield_now().await;
        assert_eq!(rx.recv().expect("接收"), Some(1));
        assert_eq!(rx.recv().expect("接收"), Some(2));
        rx
    };
    let (_tx, mut rx) = futures_util::join!(send, recv);
    assert_eq!(rx.recv().expect("接收"), Some(3));
}

dual_runtime_test_!(mpsc_send_next_async_waits_for_space_);

/// 测试 MPSC 的 `send_async` 在队满时**在真实运行时里等待**，读端腾空间后完成。
/// - 手段：容量 2 写满；「发送第三条」与「先让出一次、再读走两条」并发执行。
/// - 判断：发送最终成功，读端依次读到 1、2、3。
async fn mpsc_send_async_waits_then_succeeds_() {
    let (mut tx, mut rx) = new_mpsc_::<u8, 2>(2).await;
    tx.try_send(1).expect("第 1 条");
    tx.try_send(2).expect("第 2 条");

    let send = async {
        tx.send_async(3).await.expect("发送应当等到空间");
        tx
    };
    let recv = async {
        futures_lite::future::yield_now().await;
        assert_eq!(rx.recv().expect("接收"), Some(1));
        assert_eq!(rx.recv().expect("接收"), Some(2));
        rx
    };
    let (_tx, mut rx) = futures_util::join!(send, recv);
    assert_eq!(rx.recv().expect("接收"), Some(3));
}


/// 测试多位写者同时用 `send_async` 发送时，消息一条不少、一条不重。
/// - 手段：两个写者在同一个运行时任务里各用 `send_async` 发 3 条。
/// - 判断：读端读到 6 条，集合与发送集合一致。
async fn mpsc_concurrent_send_async_keeps_all_() {
    let (mut tx1, mut rx) = new_mpsc_::<u8, 2>(8).await;
    let mut tx2 = tx1.clone();
    async {
        for i in 0..3u8 {
            tx1.send_async(i).await.expect("写者 1");
            tx2.send_async(100 + i).await.expect("写者 2");
        }
    }.await;
    let mut got = Vec::new();
    while let Some(v) = rx.recv().expect("接收") {
        got.push(v);
    }
    got.sort_unstable();
    assert_eq!(got, vec![0u8, 1, 2, 100, 101, 102]);
}

dual_runtime_test_!(mpsc_concurrent_send_async_keeps_all_);


/// 测试 `send_bulk_async` 先等空间与写入位，再在许可保护下**同步**填充。
/// - 手段：`send_bulk_async(2, f)`，`f` 里同步 push 两条。
/// - 判断：返回实际写入 2；读端按序读到两条。
async fn mpsc_send_bulk_async_fills_synchronously_() {
    let (mut tx, mut rx) = new_mpsc_::<u8, 2>(8).await;
    let written = async {
        tx.send_bulk_async(2, |w| {
            assert!(w.push(7));
            assert!(w.push(8));
        })
        .await
    }.await
    .expect("预留成功");
    assert_eq!(written, 2);
    assert_eq!(rx.recv().expect("接收"), Some(7));
    assert_eq!(rx.recv().expect("接收"), Some(8));
}

dual_runtime_test_!(mpsc_send_bulk_async_fills_synchronously_);


/// 测试 `close_async` **等到写入位**才关闭：此前提交的消息一条都不会丢。
/// - 手段：发 3 条后 `close_async`；再用读端把残留全部读出。
/// - 判断：3 条都在，之后才是 `Closing`。
async fn mpsc_close_async_keeps_committed_messages_() {
    let (mut tx, mut rx) = new_mpsc_::<u8, 2>(8).await;
    for i in 1..=3u8 {
        tx.try_send(i).expect("发送");
    }
    async {
        tx.close_async().into_future().await.expect("关闭成功")
    }.await;
    assert!(rx.is_writer_closed());
    for expect in [1u8, 2, 3] {
        assert_eq!(rx.recv().expect("残留可读"), Some(expect));
    }
    assert_eq!(
        rx.recv().expect_err("关闭且读空后是 EOF"),
        ChannelError::Closing
    );
}

dual_runtime_test_!(mpsc_close_async_keeps_committed_messages_);


/// 测试**最后一个写者消失即关闭写端**：读端不会永久挂起。
/// - 手段：两个克隆写者各发 1 条后把它们都 drop。
/// - 判断：残留可读；读空后返回 `Closing`。
async fn mpsc_drop_all_senders_closes_channel_() {
    let (tx, mut rx) = new_mpsc_::<u8, 2>(8).await;
    {
        let mut a = tx.clone();
        let mut b = tx;
        a.try_send(1).expect("写者 a");
        b.try_send(2).expect("写者 b");
    }
    assert!(rx.is_writer_closed(), "最后一个写者消失后写端应已关闭");
    assert_eq!(rx.recv().expect("接收"), Some(1));
    assert_eq!(rx.recv().expect("接收"), Some(2));
    assert_eq!(rx.recv().expect_err("EOF"), ChannelError::Closing);
}

dual_runtime_test_!(mpsc_drop_all_senders_closes_channel_);


/// 测试**最后一个写者 drop 也走同一套关闭机制**：尽力把 `Closing` 信号放进环里。
/// - 手段：单个写者发 2 条后 drop，观察读端可读格数并读空。
/// - 判断：可读格数 = 3（2 条载荷 + 1 条关闭信号）；读出 2 条载荷后是 `Closing`。
async fn mpsc_drop_sender_puts_closing_signal_in_band_() {
    let (mut tx, mut rx) = new_mpsc_::<u8, 2>(8).await;
    tx.try_send(1).expect("发送");
    tx.try_send(2).expect("发送");
    drop(tx);
    assert_eq!(rx.data_size(), 3, "2 条载荷 + 1 条关闭信号");
    assert_eq!(rx.recv().expect("接收"), Some(1));
    assert_eq!(rx.recv().expect("接收"), Some(2));
    assert_eq!(rx.recv().expect_err("EOF"), ChannelError::Closing);
}

dual_runtime_test_!(mpsc_drop_sender_puts_closing_signal_in_band_);

dual_runtime_test_!(mpsc_send_async_waits_then_succeeds_);

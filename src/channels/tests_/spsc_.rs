//! SPSC 队列（[`SpscChannel`](crate::channels::SpscChannel)）的测试。

use std::{string::String, string::ToString, vec, vec::Vec};

use abs_async_iter::TrAsyncIterator;

use crate::channels::{ChannelError, SpscChannel, SpscReceiver, SpscSender};
use crate::x_deps::abs_cancel::TrMayCancel;

/// 让一个异步用例在 **tokio** 与 **compio** 两种**真实运行时**下各跑一遍。
///
/// 用法：把用例写成 `async fn name_()`，紧随其后写 `dual_runtime_test_!(name_);`。
/// 不自己 `block_on`、不手动轮询——那样测的是「假设的世界」，而不是真实运行时里
/// 被 waker 驱动的行为。
macro_rules! dual_runtime_test_ {
    ($name:ident) => {
        #[allow(non_snake_case, missing_docs)]
        mod $name {
            #[tokio::test]
            async fn tokio_() {
                super::$name().await
            }

            #[compio::test]
            async fn compio_() {
                super::$name().await
            }
        }
    };
}

/// 装配一个默认形态的 SPSC 队列（测试公共前置）。
async fn new_spsc_<T: Send + Sync + 'static>(
    cap: usize,
) -> (SpscSender<T>, SpscReceiver<T>) {
    SpscChannel::with_capacity(cap)
        .expect("容量合法")
        .into_parts::<T, crate::circular_buff::CoreAlloc>()
        .await
        .expect("装配成功")
}

/// 测试 SPSC「构建 → 逐条发送 → 逐条接收」的最小闭环。
/// - 手段：以容量 8 构建，用 `try_send` 写入 3 条，再用 `recv` 依次读出。
/// - 判断：读出序列与写入序列完全一致（FIFO）；读空且写端未关闭时 `recv` 返回
///   `Ok(None)`（暂态），而不是错误。
async fn spsc_fifo_roundtrip_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(8).await;
    for b in [1u8, 2, 3] {
        tx.try_send(b).expect("发送应当成功");
    }
    for expect in [1u8, 2, 3] {
        assert_eq!(rx.recv().expect("接收应当成功"), Some(expect));
    }
    assert_eq!(rx.recv().expect("队空但写端未关闭"), None);
}

dual_runtime_test_!(spsc_fifo_roundtrip_);


/// 测试有界性（背压）与「不挤掉已有数据」。
/// - 手段：以容量 2 构建，连续发送 3 条。
/// - 判断：前两条成功；第三条返回 [`ChannelError::Stuffed`]；随后仍能按序读出两条。
async fn spsc_is_bounded_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(2).await;
    tx.try_send(7).expect("第 1 条");
    tx.try_send(8).expect("第 2 条");
    assert_eq!(tx.try_send(9).expect_err("已满"), ChannelError::Stuffed);
    assert_eq!(rx.recv().expect("接收"), Some(7));
    assert_eq!(rx.recv().expect("接收"), Some(8));
    assert_eq!(rx.recv().expect("队空但写端未关闭"), None);
}

dual_runtime_test_!(spsc_is_bounded_);


/// 测试关闭写端后的 EOF 语义。
/// - 手段：发送 1 条后 `close`，再连续接收两次。
/// - 判断：第一次拿到残留数据；第二次返回 [`ChannelError::Closing`]（终止）。
async fn spsc_close_then_eof_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(8).await;
    tx.try_send(42).expect("发送");
    tx.close();
    assert!(rx.is_writer_closed(), "写端已关闭");
    assert_eq!(rx.recv().expect("残留数据仍可读"), Some(42));
    assert_eq!(
        rx.recv().expect_err("残留读空后是 EOF"),
        ChannelError::Closing
    );
}

dual_runtime_test_!(spsc_close_then_eof_);


/// 测试容量校验在构造期就被拒绝。
/// - 手段：以容量 0 构造。
/// - 判断：`with_capacity(0)` 返回 `Err`。
async fn spsc_rejects_zero_capacity_() {
    assert!(SpscChannel::with_capacity(0).is_err(), "容量 0 必须被拒绝");
}

dual_runtime_test_!(spsc_rejects_zero_capacity_);


/// 测试消息类型不需要 `Copy` / `Clone`（用 `String` 验证所有权转移）。
/// - 手段：发送两条 `String`，再接收两条。
/// - 判断：内容一致，且堆内存随消息从写端转移到读端。
async fn spsc_moves_non_copy_messages_() {
    let (mut tx, mut rx) = new_spsc_::<String>(8).await;
    tx.try_send("hello".to_string()).expect("发送");
    tx.try_send("world".to_string()).expect("发送");
    assert_eq!(rx.recv().expect("接收"), Some("hello".to_string()));
    assert_eq!(rx.recv().expect("接收"), Some("world".to_string()));
}

dual_runtime_test_!(spsc_moves_non_copy_messages_);


/// 测试 bulk 帧**写满**时的完整交付。
/// - 手段：`try_send_bulk(3)` 取得填充器并填满 3 条，drop 填充器提交。
/// - 判断：读端按发送顺序完整读出 3 条。
async fn spsc_bulk_full_() {
    let (mut tx, mut rx) = new_spsc_::<u32>(8).await;
    {
        let mut w = tx.try_send_bulk(3).expect("预留成功");
        assert!(w.push(10));
        assert!(w.push(20));
        assert!(w.push(30));
        assert!(!w.push(40), "超出帧容量应当失败");
    } // drop ⇒ 提交整帧
    for expect in [10u32, 20, 30] {
        assert_eq!(rx.recv().expect("接收"), Some(expect));
    }
    assert_eq!(rx.recv().expect("队空"), None);
}

dual_runtime_test_!(spsc_bulk_full_);


/// 测试 bulk 帧**只写了一部分**时，消费端仍拿到已写前缀，且帧边界不被打乱。
/// - 手段：以容量 8 构建；声称 3 条的帧只填 2 条（第 3 格留给空洞）；随后再发一条
///   单体消息。
/// - 判断：读端依次读到前 2 条与随后的单体消息——中间的空洞被**静默丢弃**，且没有
///   把空洞误当成消息、也没有把下一条单体消息吞进帧里。这是「内部空洞不外泄 +
///   部分写入仍可交付」的核心用例。
async fn spsc_bulk_partial_delivers_written_prefix_() {
    let (mut tx, mut rx) = new_spsc_::<String>(8).await;
    {
        let mut w = tx.try_send_bulk(3).expect("预留成功");
        assert!(w.push("a".to_string()));
        assert!(w.push("b".to_string()));
        assert_eq!(w.written(), 2);
        // 第 3 条故意不填 → Drop 补成空洞。
    }
    tx.try_send("c".to_string()).expect("发送单体");

    assert_eq!(rx.recv().expect("接收"), Some("a".to_string()));
    assert_eq!(rx.recv().expect("接收"), Some("b".to_string()));
    assert_eq!(
        rx.recv().expect("接收"),
        Some("c".to_string()),
        "空洞被丢弃后，下一条单体消息应当照常收到"
    );
    assert_eq!(rx.recv().expect("队空"), None);
}

dual_runtime_test_!(spsc_bulk_partial_delivers_written_prefix_);


/// 测试**整个 bulk 帧被放弃**（一条都没填）时不会阻塞后续消息。
/// - 手段：`try_send_bulk(3)` 一条不填直接 drop，随后发一条单体消息。
/// - 判断：读端直接读到那条单体消息——整帧的空洞被全部丢弃，不会卡住消费端。
async fn spsc_bulk_fully_abandoned_does_not_stall_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(8).await;
    {
        let _w = tx.try_send_bulk(3).expect("预留成功");
    }
    tx.try_send(99).expect("发送单体");
    assert_eq!(rx.recv().expect("接收"), Some(99));
}

dual_runtime_test_!(spsc_bulk_fully_abandoned_does_not_stall_);


/// 测试 bulk 帧的空间需求是 `n + 1`（预告头额外占一格）。
/// - 手段：以容量 4 构建，尝试 `try_send_bulk(4)`（需要 5 格）。
/// - 判断：返回 [`ChannelError::Stuffed`]；而 `try_send_bulk(3)`（恰好 4 格）成功。
async fn spsc_bulk_head_costs_one_extra_slot_() {
    let (mut tx, _rx) = new_spsc_::<u8>(4).await;
    assert_eq!(
        tx.try_send_bulk(4).err().expect("需要 5 格，只有 4 格"),
        ChannelError::Stuffed
    );
    assert!(tx.try_send_bulk(3).is_ok(), "需要 4 格，恰好放得下");
}

dual_runtime_test_!(spsc_bulk_head_costs_one_extra_slot_);


/// 测试连续多个 bulk 帧 + 单体消息交错时，消费端看到的载荷序列完全有序。
/// - 手段：依次发 bulk(2, 填满)、单体、bulk(2, 只填 1)。
/// - 判断：读出的载荷序列为 a,b,c,d，随后读空。
async fn spsc_interleaved_frames_keep_order_() {
    let (mut tx, mut rx) = new_spsc_::<char>(16).await;
    {
        let mut w = tx.try_send_bulk(2).expect("bulk 1");
        w.push('a');
        w.push('b');
    }
    tx.try_send('c').expect("单条");
    {
        let mut w = tx.try_send_bulk(2).expect("bulk 2");
        w.push('d');
    }
    let got: Vec<char> = core::iter::from_fn(|| rx.recv().expect("接收")).collect();
    assert_eq!(got, vec!['a', 'b', 'c', 'd']);
}

dual_runtime_test_!(spsc_interleaved_frames_keep_order_);


/// 测试读端实现 [`TrAsyncIterator`]（写端**故意不**实现，见模块文档）。
/// - 手段：用 `send_async` 写入；用读端 `next_async` 取载荷；关闭后再取。
/// - 判断：读端按序拿到载荷；关闭并读空后返回 [`ChannelError::Closing`]。
async fn spsc_receiver_implements_async_iter_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(8).await;
    async {
        tx.send_async(5).await.expect("发送");
    }.await;
    let got = async {
        rx.next_async().await.expect("应当拿到载荷")
    }.await;
    assert_eq!(got, 5);

    tx.close();
    let closed = async { rx.next_async().await }.await;
    assert!(matches!(closed, Err(ChannelError::Closing)));
}

dual_runtime_test_!(spsc_receiver_implements_async_iter_);


/// 测试 `send_async` 在队满时**在真实运行时里等待**，读端腾出空间后完成。
/// - 手段：容量 2 写满；「发送第三条」与「先让出一次、再读走两条」并发执行（`join!`）。
/// - 判断：发送最终成功，读端依次读到 1、2、3。**不手动轮询**。
async fn spsc_send_async_waits_then_succeeds_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(2).await;
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

dual_runtime_test_!(spsc_send_async_waits_then_succeeds_);


/// 测试 `send_async` 的取消语义：取消令牌就绪即返回 `Cancelled`，且消息**不入队**。
/// - 手段：队满后以「已取消」令牌驱动 `send_async`。
/// - 判断：返回 [`ChannelError::Cancelled`]；读端读到的仍是原有两条。
async fn spsc_send_async_cancelled_does_not_enqueue_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(2).await;
    tx.try_send(1).expect("第 1 条");
    tx.try_send(2).expect("第 2 条");

    let cancel = crate::x_deps::abs_cancel::CancelledToken::new();
    let r = async {
        tx.send_async(3).may_cancel_with(cancel).await
    }.await;
    assert!(matches!(r, Err(ChannelError::Cancelled)), "取消应当返回 Cancelled");

    assert_eq!(rx.recv().expect("接收"), Some(1));
    assert_eq!(rx.recv().expect("接收"), Some(2));
    assert_eq!(rx.recv().expect("第 3 条不应入队"), None);
}

dual_runtime_test_!(spsc_send_async_cancelled_does_not_enqueue_);


/// 测试 `send_bulk_async` 先等够空间、再返回**同步**填充器。
/// - 手段：以容量 4 构建，`send_bulk_async(3)` 需要 4 格，恰好放得下。
/// - 判断：填充器可同步 `push` 3 条；drop 后读端读出 3 条。
async fn spsc_send_bulk_async_returns_sync_writer_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(4).await;
    async {
        let mut w = tx.send_bulk_async(3).await.expect("预留成功");
        assert!(w.push(1));
        assert!(w.push(2));
        assert!(w.push(3));
    }.await;
    for expect in [1u8, 2, 3] {
        assert_eq!(rx.recv().expect("接收"), Some(expect));
    }
}

dual_runtime_test_!(spsc_send_bulk_async_returns_sync_writer_);


/// 测试**写端 drop 即关闭**：读端先读完残留，然后得到 `Closing` 而不是永久挂起。
/// - 手段：发送 2 条后 drop 写端；读端连续读三次。
/// - 判断：前两次拿到残留，第三次返回 [`ChannelError::Closing`]。
async fn spsc_drop_sender_closes_channel_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(8).await;
    tx.try_send(1).expect("发送");
    tx.try_send(2).expect("发送");
    drop(tx);
    assert!(rx.is_writer_closed(), "写端 drop 后应已关闭");
    assert_eq!(rx.recv().expect("接收"), Some(1));
    assert_eq!(rx.recv().expect("接收"), Some(2));
    assert_eq!(rx.recv().expect_err("EOF"), ChannelError::Closing);
}

dual_runtime_test_!(spsc_drop_sender_closes_channel_);


/// 测试 `close_async` 的取消语义：关闭**尚未开始**就被取消 ⇒ 什么都不做。
/// - 手段：以「已取消」令牌驱动 `close_async`。
/// - 判断：返回 [`ChannelError::Cancelled`]；写端**仍然打开**，还能继续发送。
///
/// 与之相对：关闭一旦开始（上游 `close_tx_async` 是先落标志、再排空），取消只中断
/// 「等主动消费端排空」这一步，不会撤销关闭——这正是「关闭前必须给消费端发协议信号、
/// 保证它收得到」这条语义的要求。
async fn spsc_close_async_cancelled_keeps_open_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(8).await;
    let cancel = crate::x_deps::abs_cancel::CancelledToken::new();
    let r = async {
        tx.close_async().may_cancel_with(cancel).await
    }.await;
    assert!(matches!(r, Err(ChannelError::Cancelled)));
    assert!(!rx.is_writer_closed(), "取消后不应关闭");
    tx.try_send(7).expect("仍可发送");
    assert_eq!(rx.recv().expect("接收"), Some(7));
}

dual_runtime_test_!(spsc_close_async_cancelled_keeps_open_);


/// 测试 `close_async` 发送的**带内关闭信号**一定排在已提交载荷之后。
/// - 手段：先发 3 条，再 `close_async`；然后把读端读到结束为止的全部载荷收集起来。
/// - 判断：3 条载荷一条不少、顺序正确，之后才是 [`ChannelError::Closing`]。
///
/// 这是「关闭是内部协议消息的一部分」的直接体现：读端的 EOF 不依赖任何带外标志的
/// 时序，而是**按顺序读到**那条 `Closing`——排在它前面的数据必然已经交付。
async fn spsc_close_async_signal_is_in_band_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(8).await;
    for i in 1..=3u8 {
        tx.try_send(i).expect("发送");
    }
    async {
        tx.close_async().into_future().await.expect("关闭成功")
    }.await;

    let mut got = Vec::new();
    loop {
        match rx.recv() {
            Ok(Some(v)) => got.push(v),
            Ok(None) => panic!("关闭后不应出现暂态空读"),
            Err(ChannelError::Closing) => break,
            Err(e) => panic!("意外错误: {e:?}"),
        }
    }
    assert_eq!(got, vec![1u8, 2, 3], "关闭信号之前的数据必须全部交付");
}

dual_runtime_test_!(spsc_close_async_signal_is_in_band_);


/// 测试**写端 drop 也走同一套关闭机制**：尽力把 `Closing` 信号放进环里。
/// - 手段：发 2 条后 drop 写端，先看读端可读格数，再把数据全部读出。
/// - 判断：可读格数 = 3（2 条载荷 + 1 条关闭信号）——信号确实入了环；随后读到 2 条
///   载荷，再是 `Closing`。这与显式 `close_async` 是同一条路径。
async fn spsc_drop_sender_puts_closing_signal_in_band_() {
    let (mut tx, mut rx) = new_spsc_::<u8>(8).await;
    tx.try_send(1).expect("发送");
    tx.try_send(2).expect("发送");
    drop(tx);
    assert_eq!(rx.data_size(), 3, "2 条载荷 + 1 条关闭信号");
    assert_eq!(rx.recv().expect("接收"), Some(1));
    assert_eq!(rx.recv().expect("接收"), Some(2));
    assert_eq!(rx.recv().expect_err("EOF"), ChannelError::Closing);
}

dual_runtime_test_!(spsc_drop_sender_puts_closing_signal_in_band_);


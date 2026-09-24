//! SPSC 队列（[`SpscChannel`](crate::channels::SpscChannel)）的测试。

use core::mem::{self, MaybeUninit};
use std::{string::String, string::ToString, vec, vec::Vec};

use abs_async_iter::TrAsyncIterator;
use abs_buff::Demand;

use crate::channels::{ChannelError, SpscChannel};

/// 一次性把 `SpscChannel` 装成默认形态（测试内的公共前置）。
fn new_spsc_(
    cap: usize,
) -> (
    crate::channels::SpscSenderDefault<u8>,
    crate::channels::SpscReceiverDefault<u8>,
) {
    futures_lite::future::block_on(async {
        SpscChannel::with_capacity(cap)
            .expect("容量合法")
            .into_parts::<u8, crate::circular_buff::CoreAlloc>()
            .await
            .expect("装配成功")
    })
}

/// 把若干**不可 `Copy`** 的元素搬进写段（测试专用；与 `fill_one_` 同一条路径）。
///
/// 经 `ReclSliceMut::move_items_from_buff` 位拷贝搬入并推进段的已消费偏移——这是
/// `circ_buff` 自己的测试（`fill_segm`）采用的写法。搬空后暂存无需 drop。
fn fill_from_vec_<T, R>(
    segm: &mut crate::circular_buff::ReclSliceMut<'_, T, R>,
    items: Vec<T>,
) -> usize
where
    R: abs_buff::buffer::TrReclaim,
{
    let mut staging: Vec<MaybeUninit<T>> =
        items.into_iter().map(MaybeUninit::new).collect();
    // SAFETY: 元素被逐个位拷贝搬出；搬空后暂存不再持有任何需 drop 的值。
    let moved = segm.move_items_from_buff(&mut staging);
    // 已搬出的槽位由接收端接管所有权；剩下的未初始化槽位无需 drop。
    mem::forget(staging);
    moved
}

/// 从读段 move 出全部元素（测试专用）。
fn take_all_<T, R>(
    segm: &mut crate::circular_buff::ReclSliceRef<'_, T, R>,
) -> Vec<T>
where
    R: abs_buff::buffer::TrReclaim,
{
    let n = segm.least_count();
    let mut dst: Vec<MaybeUninit<T>> = (0..n).map(|_| MaybeUninit::uninit()).collect();
    // SAFETY: 位拷贝搬出；所有权随 `assume_init` 转移给返回值。
    let moved = unsafe { segm.move_items_to_buff(&mut dst) };
    dst.truncate(moved);
    dst.into_iter()
        // SAFETY: `moved` 个槽位已被写入。
        .map(|m| unsafe { m.assume_init() })
        .collect()
}

/// 测试 SPSC 队列「构建 → 逐条发送 → 逐条接收」这条最小闭环。
/// - 手段：以容量 4 构建，用 `send` 写入 3 个单元，再用 `recv` 依次读出。
/// - 判断：读出的序列必须与写入序列**完全一致**（FIFO），证明承载与两端代理都正确。
#[test]
fn spsc_fifo_roundtrip_() {
    let (mut tx, mut rx) = new_spsc_(4);
    for b in [1u8, 2, 3] {
        tx.send(b).expect("发送应当成功");
    }
    assert_eq!(tx.data_size(), 3, "写端观察到的可读数据量");
    assert_eq!(rx.data_size(), 3, "读端观察到的可读数据量");
    for expect in [1u8, 2, 3] {
        assert_eq!(rx.recv().expect("接收应当成功"), Some(expect));
    }
    assert_eq!(rx.recv().expect("队空但写端未关闭"), None);
}

/// 测试有界性（背压）与「不挤掉已有数据」。
/// - 手段：以容量 2 构建，连续发送 3 个单元。
/// - 判断：前两次成功；第三次返回 [`ChannelError::Stuffed`]，随后仍能按序读出。
#[test]
fn spsc_is_bounded_() {
    let (mut tx, mut rx) = new_spsc_(2);
    tx.send(7).expect("第 1 次发送");
    tx.send(8).expect("第 2 次发送");
    assert_eq!(tx.send(9).expect_err("队列已满"), ChannelError::Stuffed);
    assert_eq!(rx.recv().expect("接收"), Some(7));
    assert_eq!(rx.recv().expect("接收"), Some(8));
    assert_eq!(rx.recv().expect("队空但写端未关闭"), None);
}

/// 测试关闭写端后的 EOF 语义。
/// - 手段：发送 1 个单元后 `close`，再连续接收两次。
/// - 判断：第一次拿到残留数据；第二次返回 [`ChannelError::Closing`]（而不是
///   `Ok(None)`）——这是 `TrAsyncIterator` 中「`Err` 终止 / `Ok(None)` 暂态」的分界。
#[test]
fn spsc_close_then_eof_() {
    let (mut tx, mut rx) = new_spsc_(4);
    tx.send(42).expect("发送");
    tx.close();
    assert!(rx.is_writer_closed(), "写端已关闭");
    assert_eq!(rx.recv().expect("残留数据仍可读"), Some(42));
    assert_eq!(
        rx.recv().expect_err("残留读空后是 EOF"),
        ChannelError::Closing
    );
}

/// 测试容量校验在构造期就被拒绝。
/// - 手段：以容量 0 构造。
/// - 判断：`with_capacity(0)` 必须返回 `Err`。
#[test]
fn spsc_rejects_zero_capacity_() {
    assert!(SpscChannel::with_capacity(0).is_err(), "容量 0 必须被拒绝");
}

/// 测试**消息类型不需要 `Copy` / `Clone`**：在借出的写段里原地构造、读段里就地取走。
/// - 手段：以 `String` 为消息类型（非 `TriviallyCopyable`、带堆内存），经
///   `try_next` 取得写段搬入 2 条消息，再经读段全部 move 出来。
/// - 判断：读出的字符串与写入一致；`String` 的堆内存随消息转移（无泄漏、无
///   浅拷贝），证明段路径不要求任何 `Copy` / `Clone` 约束。
#[test]
fn spsc_in_place_constructs_non_copy_messages_() {
    let (mut tx, mut rx) = futures_lite::future::block_on(async {
        SpscChannel::with_capacity(4)
            .expect("容量合法")
            .into_parts::<String, crate::circular_buff::CoreAlloc>()
            .await
            .expect("装配成功")
    });

    {
        let demand = Demand::exactly(2);
        let mut segm = tx.try_next(&demand).expect("暂态可用").expect("有空位");
        let n = fill_from_vec_(
            &mut segm,
            vec!["msg-0".to_string(), "msg-1".to_string()],
        );
        assert_eq!(n, 2, "两条消息都应原地构造成功");
    }

    let demand = Demand::exactly(2);
    let mut segm = rx.try_next(&demand).expect("有数据").expect("非暂态空读");
    let got = take_all_(&mut segm);
    assert_eq!(got, vec!["msg-0".to_string(), "msg-1".to_string()]);
}

/// 测试**两端都实现同一个 [`TrAsyncIterator`]**（不另立 trait）。
/// - 手段：写端用 `next_async` 取得写段并搬入 1 条消息；读端用 `next_async`
///   取得读段并 move 出内容；最后关闭写端再取一次。
/// - 判断：写端产出非空且可写；读端按 FIFO 读到 99；关闭并读空后得到
///   `Err(ChannelError::Closing)`。
#[test]
fn spsc_both_ends_implement_async_iter_() {
    let (mut tx, mut rx) = new_spsc_(4);

    let wrote = futures_lite::future::block_on(async {
        let mut segm = tx.next_async().await.expect("应当产出写段");
        let segm = segm.as_mut().expect("非暂态空产出");
        fill_from_vec_(segm, vec![99u8]) == 1
    });
    assert!(wrote, "写端迭代应当产出一段可写空间");

    let got = futures_lite::future::block_on(async {
        let produced = rx.next_async().await;
        let mut segm = produced.expect("读端迭代应当成功").expect("非暂态空读");
        take_all_(&mut segm)
    });
    assert_eq!(got, vec![99u8]);

    tx.close();
    let closed = futures_lite::future::block_on(async { rx.next_async().await });
    assert!(
        matches!(closed, Err(ChannelError::Closing)),
        "写端关闭且读空后应当是终止性的 EOF"
    );
}

/// 测试写端的**非阻塞**入口在队满时给出暂态 `Ok(None)`（而不是错误）。
/// - 手段：以容量 2 构建，发满 2 条后用 `try_next` 再取一次写段。
/// - 判断：返回 `Ok(None)`——背压是暂态，与终止性的 `Err(Closing)` 区分开
///   （异步路径 `next_async` 在这种情况下会真的 park 等读端腾出空间，因此这里
///   只能断言非阻塞入口，不能用 `block_on(next_async)`）。
#[test]
fn spsc_full_try_next_yields_none_() {
    let (mut tx, _rx) = new_spsc_(2);
    tx.send(1).expect("发送");
    tx.send(2).expect("发送");
    let demand = Demand::exactly(1);
    let produced = tx.try_next(&demand);
    assert!(
        matches!(produced, Ok(None)),
        "队满应当是暂态 Ok(None)（背压），而不是 Err"
    );
}

/// 测试写端 `next_async` 在队满时**真的 park**，读端腾出空间后被唤醒。
/// - 手段：以容量 2 构建并写满；用 `pin!` 固定写端的 `next_async`，先手动轮询
///   一次；再用读端取走 1 条，然后重新轮询同一 future。
/// - 判断：第一次轮询必须是 `Pending`（证明它确实挂在底层可写等待 future 上，
///   而不是空转或立即返回）；第二次必须是 `Ready(Ok(Some(segm)))`——写端被读端
///   的提交唤醒。这条测试把「`next_async` 真的会等待」钉在语义层。
#[test]
fn spsc_send_next_async_parks_until_space_() {
    use core::{
        future::IntoFuture,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    let (mut tx, mut rx) = new_spsc_(2);
    tx.send(1).expect("发送");
    tx.send(2).expect("发送");

    let mut fut = pin!(tx.next_async().into_future());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(fut.as_mut().poll(&mut cx), Poll::Pending),
        "队满时写端应当 park（Pending），而不是立即返回"
    );

    // 读端腾出 1 格：这一步会唤醒 park 中的写端。
    assert_eq!(rx.recv().expect("接收"), Some(1));

    assert!(
        matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(Some(_)))),
        "读端腾出空间后写端应当拿到写段"
    );
}

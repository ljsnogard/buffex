//! MPSC 队列（[`MpscChannel`](crate::channels::MpscChannel)）的测试。

use core::mem::{self, MaybeUninit};
use std::{string::String, string::ToString, vec, vec::Vec};

use abs_async_iter::TrAsyncIterator;
use abs_buff::Demand;

use crate::channels::{ChannelError, MpscChannel, mpsc_::MpscBuildError};

/// 把若干元素搬进写段（测试专用；与 `fill_one_` 同一条路径）。
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

/// 测试 MPSC 队列「共享同一块环形缓冲」的基本读写闭环。
/// - 手段：以「2 个写者、容量 4」构建，克隆写端得到两个独立写者，各发 1 条，
///   再由读端依次取出。
/// - 判断：两条都能发出，读出顺序与发出顺序一致——证明多个写端确实指向同一块承载。
#[test]
fn mpsc_two_senders_share_one_buffer_() {
    let (mut tx1, mut rx) = futures_lite::future::block_on(async {
        MpscChannel::<u8, 2>::with_capacity(4)
            .expect("容量合法")
            .into_parts()
            .await
            .expect("装配成功")
    });
    let mut tx2 = tx1.clone();

    tx1.send(1).expect("写者 1 发送");
    tx2.send(2).expect("写者 2 发送");
    assert_eq!(rx.data_size(), 2, "两个写者共享同一块承载");

    assert_eq!(rx.recv().expect("接收"), Some(1));
    assert_eq!(rx.recv().expect("接收"), Some(2));
    assert_eq!(rx.recv().expect("队空但写端未关闭"), None);
}

/// 测试写者数量上界为 0 时构造期即报错。
/// - 手段：以 `MpscChannel::<u8, 0>::with_capacity(4)` 构造。
/// - 判断：返回 [`MpscBuildError::Channel`] 包裹的 [`ChannelError::Argument`]。
#[test]
fn mpsc_rejects_zero_senders_() {
    let e = MpscChannel::<u8, 0>::with_capacity(4).err();
    assert!(matches!(
        e,
        Some(MpscBuildError::Channel(ChannelError::Argument))
    ));
}

/// 测试**两端都实现同一个 [`TrAsyncIterator`]**。
/// - 手段：写端用 `next_async` 取得写段并搬入 1 条消息；读端用 `next_async`
///   取得读段并 move 出内容；关闭写端后再取一次。
/// - 判断：写端产出非空且可写；读端读到 5；关闭并读空后得到
///   `Err(ChannelError::Closing)`。
#[test]
fn mpsc_both_ends_implement_async_iter_() {
    let (mut tx, mut rx) = futures_lite::future::block_on(async {
        MpscChannel::<u8, 1>::with_capacity(2)
            .expect("容量合法")
            .into_parts()
            .await
            .expect("装配成功")
    });

    let wrote = futures_lite::future::block_on(async {
        let produced = tx.next_async().await.expect("写端迭代应当成功");
        let mut segm = produced.expect("非暂态空产出");
        fill_from_vec_(&mut segm, vec![5u8]) == 1
    });
    assert!(wrote, "写端迭代应当产出一段可写空间");

    let got = futures_lite::future::block_on(async {
        let produced = rx.next_async().await.expect("读端迭代应当成功");
        let mut segm = produced.expect("非暂态空读");
        take_all_(&mut segm)
    });
    assert_eq!(got, vec![5u8]);

    tx.close();
    let closed = futures_lite::future::block_on(async { rx.next_async().await });
    assert!(
        matches!(closed, Err(ChannelError::Closing)),
        "写端关闭且读空后应当是终止性的 EOF"
    );
}

/// 测试**消息类型不需要 `Copy` / `Clone`**（与 SPSC 同样的段路径）。
/// - 手段：以 `String` 为消息类型，经写段搬入 2 条、经读段全部 move 出来。
/// - 判断：内容一致，且堆内存随消息转移。
#[test]
fn mpsc_in_place_constructs_non_copy_messages_() {
    let (mut tx, mut rx) = futures_lite::future::block_on(async {
        MpscChannel::<String, 2>::with_capacity(4)
            .expect("容量合法")
            .into_parts()
            .await
            .expect("装配成功")
    });

    {
        // 写段必须在读取之前 drop——**提交正是发生在段 drop 的时刻**。
        let demand = Demand::exactly(2);
        let mut segm = tx
            .try_next(&demand)
            .expect("暂态可用")
            .expect("有空位");
        assert_eq!(
            fill_from_vec_(&mut segm, vec!["a".to_string(), "b".to_string()]),
            2
        );
    }
    assert_eq!(rx.data_size(), 2, "段 drop 后写入才被提交");

    let demand = Demand::exactly(2);
    let mut segm = rx.try_next(&demand).expect("有数据").expect("非暂态空读");
    assert_eq!(take_all_(&mut segm), vec!["a".to_string(), "b".to_string()]);
}

/// 测试写端的**非阻塞**入口在队满时给出暂态 `Ok(None)`。
/// - 手段：以容量 2 构建，发满 2 条后用 `try_next` 再取一次写段。
/// - 判断：返回 `Ok(None)`——背压是暂态（异步路径在此时会 park 等待）。
#[test]
fn mpsc_full_try_next_yields_none_() {
    let (mut tx, _rx) = futures_lite::future::block_on(async {
        MpscChannel::<u8, 1>::with_capacity(2)
            .expect("容量合法")
            .into_parts()
            .await
            .expect("装配成功")
    });
    tx.send(1).expect("发送");
    tx.send(2).expect("发送");
    let demand = Demand::exactly(1);
    let produced = tx.try_next(&demand);
    assert!(
        matches!(produced, Ok(None)),
        "队满应当是暂态 Ok(None)（背压），而不是 Err"
    );
}

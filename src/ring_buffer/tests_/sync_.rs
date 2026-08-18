//! Synchronous tests: abs_buff segment semantics, wrap-around, error
//! handling, the vectored-IO kernel handoff, the `TrRingBuffer` trait, and
//! the multithreaded SPSC pipe without any runtime.

use std::{
    boxed::Box,
    sync::Arc,
    vec,
    vec::Vec,
};

use abs_buff::Demand;

use crate::ring_buffer::{RxError, TrRingBuffer, TxError};

use super::{fill_segm, make_ring, make_ring_shared, pat_byte, seq_byte, take_segm, RING_CAP};

/// Write `[0..8)` through partial contiguous borrows and read them back,
/// including across the wrap-around.
#[test]
fn segm_borrow_roundtrip_and_wrap() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (_ring, mut tx, mut rx) = make_ring();

    // partial writes: 3 then 5 (wp: 0 -> 3 -> 8)
    let mut segm = tx.try_write_at_most(3).expect("write 3");
    assert_eq!(segm.least_count(), 3);
    fill_segm(&mut segm, &(0..3).map(seq_byte).collect::<Vec<_>>());
    drop(segm);
    assert_eq!(tx.data_size(), 3);

    let mut segm = tx.try_write_at_most(5).expect("write 5");
    assert_eq!(segm.least_count(), 5);
    fill_segm(&mut segm, &(3..8).map(seq_byte).collect::<Vec<_>>());
    drop(segm);
    assert_eq!(tx.data_size(), 8);

    // partial reads: 4 then 4 (rp: 0 -> 4 -> 8)
    let mut segm = rx.try_read_at_most(4).expect("read 4");
    assert_eq!(segm.least_count(), 4);
    let got = take_segm(&mut segm, 4);
    for (i, b) in got.iter().enumerate() {
        assert_eq!(*b, seq_byte(i));
    }
    drop(segm);

    let mut segm = rx.try_read_at_most(4).expect("read 4 more");
    assert_eq!(segm.least_count(), 4);
    let got = take_segm(&mut segm, 4);
    for (i, b) in got.iter().enumerate() {
        assert_eq!(*b, seq_byte(4 + i));
    }
    drop(segm);
    assert_eq!(tx.data_size(), 0);

    // Now force the writer position to wrap: fill the ring again so that
    // `wp` wraps past the end of the buffer. The writable region is
    // contiguous, so the wrap is reached through repeated borrows.
    let mut total = 0usize;
    while total < RING_CAP - 1 {
        let mut segm = tx.try_write_at_most(RING_CAP - 1 - total).expect("fill");
        assert!(segm.least_count() > 0);
        let len = segm.least_count();
        fill_segm(&mut segm, &(0..len).map(|i| seq_byte(100 + total + i)).collect::<Vec<_>>());
        drop(segm);
        total += len;
    }
    assert!(tx.ring().writer_pos() < RING_CAP);
    assert_eq!(tx.data_size(), RING_CAP - 1);
    // full: one slot gap
    assert!(matches!(tx.try_write_at_most(1), Err(TxError::Stuffed(_))));

    // read everything back (this may wrap at the reader side too)
    let mut off = 0usize;
    loop {
        let segm = match rx.try_read_at_most(7) {
            Ok(s) => s,
            Err(RxError::Drained(_)) => break,
            Err(e) => panic!("read failed: {e:?}"),
        };
        let len = segm.least_count();
        let mut segm = segm;
        let got = take_segm(&mut segm, len);
        for (i, b) in got.iter().enumerate() {
            assert_eq!(*b, seq_byte(100 + off + i));
        }
        drop(segm);
        off += len;
    }
    assert_eq!(off, RING_CAP - 1);
}

/// `try_peek` borrows data without consuming it.
#[test]
fn peek_does_not_consume() {
    let (_ring, mut tx, mut rx) = make_ring();
    let mut segm = tx.try_write_at_most(4).expect("write");
    fill_segm(&mut segm, &[10, 11, 12, 13]);
    drop(segm);

    let segm = rx.try_peek().expect("peek");
    // 窥视段的 `iter_slices` 返回剩余可读空间按物理段切出的迭代器（最多两段，
    // 此处数据不跨末端，只有一段）；收集后与期望内容比较；
    let slices: Vec<&[u8]> = segm.iter_slices().collect();
    assert_eq!(slices, vec![&[10u8, 11, 12, 13][..]]);
    drop(segm); // 窥视段 drop 不推进读位置（无回收）

    // still fully readable
    let segm = rx.try_peek().expect("peek again");
    assert_eq!(segm.least_count(), 4);
    drop(segm);

    let mut segm = rx.try_read_at_most(16).expect("read all");
    assert_eq!(segm.least_count(), 4);
    let got = take_segm(&mut segm, 4);
    assert_eq!(got, vec![10, 11, 12, 13]);
    drop(segm);

    // now drained
    assert!(matches!(rx.try_read_at_most(1), Err(RxError::Drained(_))));
}

// ---------------------------------------------------------------------------
// Demand::at_least 下限语义：read_async / write_async / try_read / try_write
// ---------------------------------------------------------------------------
//
// 测试意图：`TrBuffRead::read_async(&Demand)` / `TrBuffWrite::write_async(&Demand)`
// 收到的 Demand 除了上界（max）还有下界（min，例如 `Demand::at_least(n)`）。
// 旧实现只取 max、忽略 min——即使环里只有 2 个就绪元素、而 demand 要求至少 4 个，
// 也会立刻返回一个 2 元素的段，违背"至少 n 个"的语义。修复后：
//   - 异步版：数量不足 min 时必须保持 Pending（不返回不满足要求的段），
//     直到数量达到 min 才就绪；
//   - try 版：数量不足 min 时返回错误（Stuffed / Drained），而不是部分段；
//   - 读侧 EOF（rx 已关闭）时没有更多数据会来，返回现有部分数据。
//
// 内部执行设计：用手写 executor 逐次 poll future——先 poll 一次确认 Pending，
// 再改变环的状态（补充数据 / 腾出空间 / 关闭），再次 poll 确认就绪或返回，
// 从而精确验证"数量不足时不放行等待任务"的行为。

/// `read_async(&Demand::at_least(4))`：环里只有 2 个字节时必须保持 Pending，
/// 补充数据达到 4 个后应就绪并返回不少于 4 的段。
#[test]
fn read_async_honours_at_least() {
    use std::task::{Context, Poll, Waker};

    let (_ring, mut tx, mut rx) = make_ring();

    // 环里先放 2 个字节；
    let mut ws = tx.try_write_at_most(2).expect("write 2");
    fill_segm(&mut ws, &[1, 2]);
    drop(ws);

    // read_async(&Demand::at_least(4))：2 < 4 → 必须 Pending，不能返回 2 字节的段；
    // 说明：trait 的 `read_async(&Demand)` 返回不透明 `impl TrMayCancel`，无法在
    // 测试中手动驱动；这里直接构造其内部的具体 future——trait impl 正是把 Demand
    // 的 [min, max] 转发给 `ReadAsync::new`，故验证的语义完全相同。
    let mut fut = Box::pin(crate::ring_buffer::ReadAsync::new(&mut rx, 4, usize::MAX).into_future());
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    assert!(
        fut.as_mut().poll(&mut cx).is_pending(),
        "2 < 4：数量不足下限时必须保持 Pending"
    );

    // 再补 3 个字节 → 环里 5 >= 4 → 应就绪；
    let mut ws = tx.try_write_at_most(3).expect("write 3 more");
    fill_segm(&mut ws, &[3, 4, 5]);
    drop(ws);

    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(res) => {
            // 先用共享引用判定哪一侧（pick_left / pick_right 都会消费 res）；
            if res.as_ref().pick_right().is_some() {
                panic!("future 失败：{:?}", res.pick_right().unwrap());
            }
            let segm = res.pick_left().expect("SomeOf 必有且仅有一侧");
                assert!(segm.least_count() >= 4, "就绪时段的长度必须满足下限");
                let n = segm.least_count();
                let mut segm = segm;
                let got = take_segm(&mut segm, n);
                drop(segm);
                assert_eq!(got, vec![1, 2, 3, 4, 5], "读出的内容必须按序");

        }
        Poll::Pending => panic!("5 >= 4：数量达标后必须就绪"),
    }
}

/// `write_async(&Demand::at_least(5))`：可写空间只有 3 格时必须保持 Pending，
/// 读者腾出空间达到 5 格后应就绪并返回不少于 5 的段。
#[test]
fn write_async_honours_at_least() {
    use std::task::{Context, Poll, Waker};

    let (_ring, mut tx, mut rx) = make_ring(); // 容量 16

    // 先写满 12 个字节：data = 12，free = 16 - 1 - 12 = 3；
    let mut off = 0usize;
    while off < 12 {
        let mut ws = tx.try_write_at_most(12 - off).expect("write");
        let len = ws.least_count();
        fill_segm(&mut ws, &(off..off + len).map(seq_byte).collect::<Vec<_>>());
        drop(ws);
        off += len;
    }
    assert_eq!(tx.free_size(), 3);

    // write_async(&Demand::at_least(5))：free = 3 < 5 → 必须 Pending；
    // （同读侧：trait impl 把 Demand 的 [min, max] 转发给 `WriteAsync::new`）
    let mut fut = Box::pin(crate::ring_buffer::WriteAsync::new(&mut tx, 5, usize::MAX).into_future());
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    assert!(
        fut.as_mut().poll(&mut cx).is_pending(),
        "free = 3 < 5：可写空间不足下限时必须保持 Pending"
    );

    // 读者消费 3 个 → data = 9，free = 6 >= 5 → 应就绪；
    let mut rs = rx.try_read_at_most(3).expect("read 3");
    let _ = take_segm(&mut rs, 3);
    drop(rs);

    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(res) => {
            // 先用共享引用判定哪一侧（pick_left / pick_right 都会消费 res）；
            if res.as_ref().pick_right().is_some() {
                panic!("future 失败：{:?}", res.pick_right().unwrap());
            }
            let segm = res.pick_left().expect("SomeOf 必有且仅有一侧");
                assert!(segm.least_count() >= 5, "就绪时段的长度必须满足下限");
                drop(segm);

        }
        Poll::Pending => panic!("free = 6 >= 5：空间达标后必须就绪"),
    }
}

/// `try_read(&Demand::at_least(4))`：环里只有 2 个字节时不得返回 2 字节的段，
/// 而应返回 Drained 错误（数量不足下限）。
#[test]
fn try_read_honours_at_least() {
    use abs_buff::{Demand, TrBuffTryRead};

    let (_ring, mut tx, mut rx) = make_ring();
    let mut ws = tx.try_write_at_most(2).expect("write 2");
    fill_segm(&mut ws, &[1, 2]);
    drop(ws);

    // 2 < 4 → 必须报 Drained，而不是给一个 2 字节的段；
    let some = TrBuffTryRead::try_read(&mut rx, &Demand::at_least(4));
    assert!(
        matches!(some.pick_right(), Option::Some(RxError::Drained(_))),
        "数量不足下限时必须返回 Drained"
    );
}

/// `try_write(&Demand::at_least(4))`：可写空间只有 3 格时不得返回 3 字节的段，
/// 而应返回 Stuffed 错误。
#[test]
fn try_write_honours_at_least() {
    use abs_buff::{Demand, TrBuffTryWrite};

    let (_ring, mut tx, _rx) = make_ring(); // 容量 16
    // 写满 12 个字节：free = 3；
    let mut off = 0usize;
    while off < 12 {
        let mut ws = tx.try_write_at_most(12 - off).expect("write");
        let len = ws.least_count();
        fill_segm(&mut ws, &(0..len).map(seq_byte).collect::<Vec<_>>());
        drop(ws);
        off += len;
    }
    assert_eq!(tx.free_size(), 3);

    let some = TrBuffTryWrite::try_write(&mut tx, &Demand::at_least(4));
    assert!(
        matches!(some.pick_right(), Option::Some(TxError::Stuffed(_))),
        "可写空间不足下限时必须返回 Stuffed"
    );
}

/// 读侧 EOF 特例：rx 已关闭且数据不足下限时，没有更多数据会来，
/// `read_async` 应返回现有部分数据，而不是永远等待。
#[test]
fn read_async_at_least_returns_partial_on_eof() {
    use std::task::{Context, Poll, Waker};

    let (ring, mut tx, mut rx) = make_ring();
    let mut ws = tx.try_write_at_most(2).expect("write 2");
    fill_segm(&mut ws, &[7, 8]);
    drop(ws);
    drop(tx);
    ring.close_rx(); // 模拟 EOF：不会再有多余数据到达

    let mut fut = Box::pin(crate::ring_buffer::ReadAsync::new(&mut rx, 4, usize::MAX).into_future());
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(res) => {
            // 先用共享引用判定哪一侧（pick_left / pick_right 都会消费 res）；
            if res.as_ref().pick_right().is_some() {
                panic!("future 失败：{:?}", res.pick_right().unwrap());
            }
            let segm = res.pick_left().expect("SomeOf 必有且仅有一侧");
                // EOF：返回现有 2 个字节（不足下限）作为最后一段；
                assert_eq!(segm.least_count(), 2);
                let n = segm.least_count();
                let mut segm = segm;
                let got = take_segm(&mut segm, n);
                drop(segm);
                assert_eq!(got, vec![7, 8]);

        }
        Poll::Pending => panic!("rx 已关闭：不得永远等待"),
    }
}

/// `Demand::less_than`（无下限）行为保持不变：环里有多少就返回多少。
#[test]
fn read_async_less_than_still_partial() {
    use std::task::{Context, Poll, Waker};

    let (_ring, mut tx, mut rx) = make_ring();
    let mut ws = tx.try_write_at_most(2).expect("write 2");
    fill_segm(&mut ws, &[1, 2]);
    drop(ws);

    let mut fut = Box::pin(crate::ring_buffer::ReadAsync::new(&mut rx, 0, 8).into_future());
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(res) => {
            // 先用共享引用判定哪一侧（pick_left / pick_right 都会消费 res）；
            if res.as_ref().pick_right().is_some() {
                panic!("future 失败：{:?}", res.pick_right().unwrap());
            }
            let segm = res.pick_left().expect("SomeOf 必有且仅有一侧");
                assert_eq!(segm.least_count(), 2, "无下限时返回现有全部数据");
                drop(segm);

        }
        Poll::Pending => panic!("2 > 0：有数据就必须就绪"),
    }
}

// ---------------------------------------------------------------------------
// 跨末端的两段式（scatter/gather）段：一次拿到"逻辑上连续、物理上分段"的空位
// ---------------------------------------------------------------------------
//
// 测试意图：普通的 abs_buff `SegmMut` / `SegmRef` 只能表达**一段物理连续**的
// 缓冲区。但 RingBuffer 的可用空间在环绕缓冲区末端时会被物理拆成两段（例如
// 末端 2 格 + 开端 2 格），逻辑上它们才是一段连续空间。本用例验证：当空闲
// 空间被物理拆成两段时，生产者**一次性**要求"至少全部空位"（不要求物理连续），
// RingBuffer 必须能用一个逻辑段满足——否则在"必须一次写入整块"的用法（例如
// 管道按整段搬运）下就会卡死。
//
// 内部执行设计：
//   1. 容量 10：生产者一次写入 8 个字节（wp = 8）。
//   2. 消费者消费 3 个（rp = 3）：data = 5。
//      （题述为"消费 2 个"，但单间隙方案始终预留 1 格，消费 2 个只能腾出
//       2+1 格；改为消费 3 个后恰好得到 2+2，与题述"末端 2 个 + 开端 2 个"
//       的图景一致。）
//   3. 单间隙方案下可写空间 = 容量 - 1 - data = 10 - 1 - 5 = 4，物理上分为
//      两段：[8, 10)（末端 2 格）+ [0, 2)（开端 2 格，刚被消费掉）。
//   4. 生产者调用 `try_write(4)` 一次性要求 4 个空位：
//      - 旧实现（单连续 slice）只能给出 min(4, 容量 - wp) = 2 格，
//        "一次给 4 个"的语义无法满足，下面的断言会失败（即要修复的 bug）；
//      - 修复后应返回覆盖两段的逻辑段，least_count() == 4。
//   5. 跨两段写入 4 个元素后，读者按序读回全部 9 个字节（读侧同样会跨末端
//      环绕，因此读段也必须是两段式的）。
#[test]
fn write_one_shot_satisfies_wrapped_free_space() {
    // 容量 10 的 ring，以唯一持有者身份拆分；
    let ring = Arc::new(
        crate::ring_buffer::RingBuffer::<Box<[u8]>>::try_new(vec![0u8; 10].into_boxed_slice())
            .unwrap(),
    );
    let (mut tx, mut rx) = crate::ring_buffer::RingBuffer::try_split_shared(
        ring,
        std::sync::Arc::strong_count, std::sync::Arc::weak_count,
    )
    .expect("唯一持有者拆分必须成功");

    // —— 1. 生产者一次写入 8 个字节（wp -> 8）——
    let mut off = 0usize;
    while off < 8 {
        let mut segm = tx.try_write_at_most(8 - off).expect("write");
        let len = segm.least_count();
        fill_segm(&mut segm, &(off..off + len).map(seq_byte).collect::<Vec<_>>());
        drop(segm);
        off += len;
    }
    assert_eq!(tx.data_size(), 8);

    // —— 2. 消费者消费 3 个字节（rp -> 3）——
    let mut segm = rx.try_read_at_most(3).expect("read 3");
    let _ = take_segm(&mut segm, 3);
    drop(segm);
    assert_eq!(tx.data_size(), 5);

    // —— 3. 确认空闲空间被物理拆成两段：末端 2 格 + 开端 2 格 ——
    assert_eq!(tx.ring().writer_pos(), 8, "wp 应在 8");
    assert_eq!(tx.ring().reader_pos(), 3, "rp 应在 3");
    assert_eq!(tx.ring().free_size(), 4, "可写空间共 4 格");

    // —— 4. 核心断言：一次性要求全部 4 个空位（逻辑上连续）——
    let mut segm = tx.try_write_at_most(4).expect("一次要求 4 个空位");
    assert_eq!(
        segm.least_count(),
        4,
        "两段物理空间应被视作逻辑上的一段，一次给出全部 4 个空位"
    );

    // —— 5. 跨两段写入 4 个元素并提交；读者按序读回全部 9 个字节 ——
    fill_segm(&mut segm, &[100u8, 101, 102, 103]);
    drop(segm);
    assert_eq!(tx.data_size(), 9);

    let mut expected: Vec<u8> = (3..8).map(seq_byte).collect();
    expected.extend_from_slice(&[100, 101, 102, 103]);
    let mut got = Vec::new();
    while let Ok(segm) = rx.try_read_at_most(5) {
        let len = segm.least_count();
        let mut segm = segm;
        got.extend_from_slice(&take_segm(&mut segm, len));
        drop(segm);
    }
    assert_eq!(got, expected, "读者应按序读回全部数据");
}

/// Error semantics: `Stuffed` when full, `Drained` when empty, `Closing`
/// after close.
#[test]
fn error_semantics() {
    let (_ring, mut tx, mut rx) = make_ring();

    // empty
    assert!(matches!(rx.try_read_at_most(1), Err(RxError::Drained(_))));

    // fill to capacity - 1
    let mut segm = tx.try_write_at_most(RING_CAP).expect("fill");
    let n = segm.least_count();
    assert_eq!(n, RING_CAP - 1, "one slot is always unused");
    fill_segm(&mut segm, &vec![0u8; n]);
    drop(segm);
    assert!(matches!(tx.try_write_at_most(1), Err(TxError::Stuffed(_))));

    // drain
    let mut segm = rx.try_read_at_most(RING_CAP).expect("read all");
    let n = segm.least_count();
    assert_eq!(n, RING_CAP - 1);
    take_segm(&mut segm, n);
    drop(segm);
    assert!(matches!(rx.try_read_at_most(1), Err(RxError::Drained(_))));

    // closing
    rx.close();
    assert!(matches!(rx.try_read_at_most(1), Err(RxError::Closing)));
    assert!(matches!(rx.try_peek(), Err(RxError::Closing)));

    tx.close();
    // The `Closing` error is only reported when the ring is full as well
    // (matching the documented semantics: "the output end has closed and the
    // buffer is already full").
    let mut segm = tx.try_write_at_most(1).expect("write while closing still has space");
    fill_segm(&mut segm, &[0u8]);
    drop(segm);
}

/// The `TrRingBuffer` trait: the ring is a direct user pipe (write through
/// the tx half, read through the rx half).
#[test]
fn tr_ring_buffer_trait() {
    use abs_buff::{TrBuffTryPeek, TrBuffTryRead, TrBuffTryWrite};

    let mut ring = crate::ring_buffer::RingBuffer::<Box<[u8]>>::try_new(
        vec![0u8; RING_CAP].into_boxed_slice(),
    )
    .unwrap();

    assert_eq!(ring.capacity(), RING_CAP);
    assert_eq!(ring.data_size(), 0);

    let Some((mut tx, mut rx)) = ring.try_split_io() else {
        panic!("try_split_io returned None");
    };

    // write through the abs_buff trait interface
    let demand = Demand::less_than(4);
    let some = TrBuffTryWrite::try_write(&mut tx, &demand);
    let Some(mut segm) = some.pick_left() else {
        panic!("TrBuffTryWrite::try_write failed")
    };
    fill_segm(&mut segm, &(0..4).map(seq_byte).collect::<Vec<_>>());
    drop(segm);
    assert_eq!(tx.data_size(), 4);

    // read through the abs_buff trait interface
    let demand = Demand::less_than(16);
    let some = TrBuffTryRead::try_read(&mut rx, &demand);
    let Some(segm) = some.pick_left() else {
        panic!("TrBuffTryRead::try_read failed")
    };
    let n = segm.least_count();
    let mut segm = segm;
    let got: Vec<u8> = take_segm(&mut segm, n);
    drop(segm);
    assert_eq!(got, vec![0, 1, 2, 3]);

    // write 4 more and peek through the abs_buff trait interface
    let demand = Demand::less_than(4);
    let some = TrBuffTryWrite::try_write(&mut tx, &demand);
    let Some(mut segm) = some.pick_left() else {
        panic!("write 4 more failed")
    };
    fill_segm(&mut segm, &[4, 5, 6, 7]);
    drop(segm);
    let some = TrBuffTryPeek::try_peek(&mut rx);
    let Some(segm) = some.pick_left() else {
        panic!("TrBuffTryPeek::try_peek failed")
    };
    // 窥视段的 `iter_slices` 按物理段切出（最多两段，此处只有一段）；
    let slices: Vec<&[u8]> = segm.iter_slices().collect();
    assert_eq!(slices[0][0], 4);
    drop(segm);

    drop(tx);
    drop(rx);
    assert_eq!(ring.capacity(), RING_CAP);
}

/// The vectored-IO kernel handoff: contiguous and wrapped iovec pairs.
#[test]
fn iovec_take_put() {
    let ring = make_ring_shared();
    let mut tx = RingTxShim(&ring);

    // write 6 bytes
    let mut segm = tx.try_write_at_most(6).unwrap();
    let n = segm.least_count();
    fill_segm(&mut segm, &(0..n).map(|i| (10 + i) as u8).collect::<Vec<_>>());
    drop(segm);

    // take the send iovecs (contiguous: one non-empty slice)
    let (a, b) = ring.take_send_iovecs().expect("send iovecs");
    assert_eq!(a, &[10, 11, 12, 13, 14, 15]);
    assert!(b.is_empty());
    ring.put_back_send(6);
    assert_eq!(ring.data_size(), 0);

    // fill the ring to force a wrap, then take the wrapped send iovecs
    let mut off = 0usize;
    while off < RING_CAP - 1 {
        let mut segm = tx.try_write_at_most(RING_CAP - off).unwrap();
        let len = segm.least_count();
        fill_segm(&mut segm, &(0..len).map(|i| (20 + off + i) as u8).collect::<Vec<_>>());
        drop(segm);
        off += len;
    }
    let (a, b) = ring.take_send_iovecs().expect("wrapped send iovecs");
    assert!(!a.is_empty() && !b.is_empty(), "the region must wrap");
    // the two slices concatenate to the whole readable region
    let mut all = Vec::new();
    all.extend_from_slice(a);
    all.extend_from_slice(b);
    assert_eq!(all.len(), RING_CAP - 1);
    for (i, x) in all.iter().enumerate() {
        assert_eq!(*x, 20 + i as u8);
    }
    ring.put_back_send(all.len());

    // take the recv iovecs (writable region), fill them, put back
    let (a, b) = ring.take_recv_iovecs().expect("recv iovecs");
    let n = a.len() + b.len();
    assert_eq!(n, RING_CAP - 1);
    for (i, slot) in a.iter_mut().enumerate() {
        *slot = pat_byte(i);
    }
    for (i, slot) in b.iter_mut().enumerate() {
        *slot = pat_byte(a.len() + i);
    }
    ring.put_back_recv(n);
    assert_eq!(ring.data_size(), n);
    let (c, _) = ring.take_send_iovecs().unwrap();
    assert_eq!(&c[..4], &[0, 7, 14, 21]);
    ring.put_back_send(0);
}

/// The runtime reservation blocks the opposite user end (kernel mode).
#[test]
fn kernel_reservation_blocks_user() {
    let ring = make_ring_shared();
    let mut tx = RingTxShim(&ring);
    let mut rx = RingRxShim(&ring);

    let mut segm = tx.try_write_at_most(4).unwrap();
    fill_segm(&mut segm, &[1, 2, 3, 4]);
    drop(segm);

    let (_a, _b) = ring.take_send_iovecs().unwrap();
    // the user reader is blocked while the kernel owns the region
    assert!(matches!(rx.try_read_at_most(1), Err(RxError::Drained(_))));
    ring.put_back_send(4);
    // the kernel wrote the data out: the ring is drained again
    assert!(matches!(rx.try_read_at_most(1), Err(RxError::Drained(_))));

    // the runtime reserves the writable region for a kernel read; the user
    // writer is blocked meanwhile
    let (a, _b) = ring.take_recv_iovecs().unwrap();
    a[0] = 42;
    a[1] = 43;
    a[2] = 44;
    assert!(matches!(tx.try_write_at_most(1), Err(TxError::Stuffed(_))));
    ring.put_back_recv(3);

    // now the user can read the received data
    let mut segm = rx.try_read_at_most(3).unwrap();
    let got = take_segm(&mut segm, 3);
    assert_eq!(got, vec![42, 43, 44]);
    drop(segm);
}

/// A write-only ring: the tx half alone (used by the kernel-mode drivers).
#[test]
fn split_borrowed_halves() {
    let mut ring = crate::ring_buffer::RingBuffer::<Box<[u8]>>::try_new(
        vec![0u8; RING_CAP].into_boxed_slice(),
    )
    .unwrap();
    let (mut tx, mut rx) = ring.split();
    let mut segm = tx.try_write_at_most(2).unwrap();
    fill_segm(&mut segm, &[1, 2]);
    drop(segm);
    let mut segm = rx.try_read_at_most(2).unwrap();
    let got = take_segm(&mut segm, 2);
    assert_eq!(got, vec![1, 2]);
    drop(segm);
}

/// The ring is `Send + Sync` when the storage and element are.
#[test]
fn ring_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<crate::ring_buffer::RingBuffer<Box<[u8]>>>>();
}

/// Multithreaded SPSC pipe: one writer thread, one reader thread, no runtime.
#[test]
fn spsc_multithread() {
    let _ = env_logger::builder().is_test(true).try_init();
    const TOTAL: usize = 500;

    let (_ring, mut tx, mut rx) = make_ring();

    let writer = std::thread::spawn(move || {
        let mut off = 0usize;
        while off < TOTAL {
            let res = tx.try_write_at_most(5);
            let mut progressed = false;
            if let Ok(mut segm) = res {
                let len = segm.least_count();
                fill_segm(&mut segm, &(0..len).map(|i| seq_byte(off + i)).collect::<Vec<_>>());
                drop(segm);
                off += len;
                progressed = true;
            }
            if !progressed {
                std::thread::yield_now();
            }
        }
        tx.close();
    });

    let reader = std::thread::spawn(move || {
        let mut off = 0usize;
        loop {
            if off >= TOTAL {
                break;
            }
            match rx.try_read_at_most(9) {
                Ok(segm) => {
                    let len = segm.least_count();
                    let mut segm = segm;
                    let got = take_segm(&mut segm, len);
                    for (i, b) in got.iter().enumerate() {
                        assert_eq!(*b, seq_byte(off + i), "reader mismatch at {off}+{i}");
                    }
                    off += len;
                    drop(segm);
                }
                Err(_) => std::thread::yield_now(),
            }
        }
        rx.close();
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

/// Thin shims so the kernel-mode tests can write/read through the shared
/// ring without holding halves.
struct RingTxShim<'a>(&'a super::SharedRing);
impl<'a> RingTxShim<'a> {
    fn try_write_at_most(&mut self, n: usize) -> Result<crate::ring_buffer::ReclSliceMut<'_, u8>, TxError<usize>> {
        self.0.try_write_at(n).map(|(s, t)| self.0.write_segm(s, t))
    }
}
struct RingRxShim<'a>(&'a super::SharedRing);
impl<'a> RingRxShim<'a> {
    fn try_read_at_most(&mut self, n: usize) -> Result<crate::ring_buffer::ReclSliceRef<'_, u8>, RxError<usize>> {
        self.0.try_read_at(n).map(|(s, t)| self.0.read_segm(s, t))
    }
}

// ---------------------------------------------------------------------------
// try_split_shared 的 SPSC 拆分保护
// ---------------------------------------------------------------------------
//
// 测试意图：`try_split_shared` 会把同一个共享句柄（Arc）clone 进写/读两个半区，
// 从而产生"一对"生产者与消费者。若调用前 Arc 已被 clone（引用计数 > 1），
// 其它 clone 就可能再被拿去拆分出第二对（甚至更多对），把 SPSC 退化成 MPMC：
// 两个写者会竞争推进 wp、两个读者会竞争推进 rp，还可能拿到重叠的 &mut 区域，
// 使 ring 的 lock-free 状态机失效。因此拆分必须在"调用方持有唯一引用"
// （引用计数 == 1）时才被允许。
//
// 内部执行设计：每条用例都通过 `Arc::strong_count` 观察引用计数，分别验证
// 三种情形——(1) 唯一持有者拆分成功且半区可用；(2) 已存在其它 clone 时拆分
// 被拒绝并把句柄原样退回；(3) 拆分成功一次后，从半区 clone 出的句柄（驱动
// 任务的典型用法）无法再拆出第二对；旧半区全部释放后允许重新拆分。

/// 唯一持有者（引用计数 == 1）拆分成功，产出的半区能完成一次写→读往返。
#[test]
fn split_shared_succeeds_for_sole_owner() {
    // 新建 Arc 时计数为 1，满足"唯一持有者"前提；
    let ring = Arc::new(
        crate::ring_buffer::RingBuffer::<Box<[u8]>>::try_new(vec![0u8; RING_CAP].into_boxed_slice())
            .unwrap(),
    );
    let (mut tx, mut rx) = crate::ring_buffer::RingBuffer::try_split_shared(
        ring,
        std::sync::Arc::strong_count, std::sync::Arc::weak_count,
    )
    .expect("唯一持有者拆分必须成功");

    // 拆出的半区必须真的可用：写两个字节，再原样读回；
    let mut segm = tx.try_write_at_most(2).expect("write 2");
    fill_segm(&mut segm, &[1u8, 2]);
    drop(segm);
    let mut segm = rx.try_read_at_most(2).expect("read 2");
    let got = take_segm(&mut segm, 2);
    assert_eq!(got, vec![1, 2]);
    drop(segm);
}

/// 调用前已有其它 clone（引用计数 > 1）时，拆分被拒绝，并把句柄原样退回，
/// 调用方仍可继续使用它。
#[test]
fn split_shared_rejects_non_sole_owner() {
    let ring = Arc::new(
        crate::ring_buffer::RingBuffer::<Box<[u8]>>::try_new(vec![0u8; RING_CAP].into_boxed_slice())
            .unwrap(),
    );
    // 模拟"还有别处握着 Arc"：clone 一个副本，计数变为 2；
    let clone = ring.clone();

    // 引用计数 == 2 > 1 → 拆分必须失败，且 Err 退回的正是被移入的那个 Arc；
    let err = match crate::ring_buffer::RingBuffer::try_split_shared(ring, std::sync::Arc::strong_count, std::sync::Arc::weak_count) {
        Result::Err(e) => e,
        Result::Ok(_) => panic!("引用计数 > 1 时拆分必须被拒绝"),
    };
    assert!(Arc::ptr_eq(&err, &clone), "Err 必须原样退回句柄");

    // 退回的句柄依然可用（拆分失败不应污染状态），例如能正常查询容量；
    assert_eq!(err.capacity(), RING_CAP);
}

/// 拆分成功一次后，即使从半区 clone 出句柄（驱动任务的典型用法），也无法再
/// 拆出第二对生产者/消费者；只有旧半区全部释放后，ring 才允许被重新拆分。
#[test]
fn split_shared_rejects_second_pair_until_halves_dropped() {
    let ring = Arc::new(
        crate::ring_buffer::RingBuffer::<Box<[u8]>>::try_new(vec![0u8; RING_CAP].into_boxed_slice())
            .unwrap(),
    );
    let (tx, rx) = crate::ring_buffer::RingBuffer::try_split_shared(ring, std::sync::Arc::strong_count, std::sync::Arc::weak_count)
        .expect("唯一持有者拆分必须成功");

    // 从写半区 clone 出驱动侧句柄：此时计数 >= 2，任何新拆分都被拒绝，
    // 否则驱动句柄就能拆出第二对生产者/消费者，破坏 SPSC；
    let driver = tx.shared().clone();
    let err = match crate::ring_buffer::RingBuffer::try_split_shared(driver, std::sync::Arc::strong_count, std::sync::Arc::weak_count) {
        Result::Err(e) => e,
        Result::Ok(_) => panic!("半区存活期间不允许第二对拆分"),
    };
    // 退回的句柄与 driver 是同一个分配，计数至少为 2（tx/rx 各持一份）；
    assert!(Arc::strong_count(&err) >= 2);

    // 旧半区全部释放后，只剩 err 这一个引用（计数回到 1），此时允许重新
    // 拆分——但同一时刻仍然只有一对生产者/消费者，SPSC 依旧成立；
    drop(tx);
    drop(rx);
    let (mut tx2, mut rx2) = crate::ring_buffer::RingBuffer::try_split_shared(
        err,
        std::sync::Arc::strong_count, std::sync::Arc::weak_count,
    )
    .expect("旧半区全部释放后允许重新拆分");
    // 重新拆分出的半区同样可用；
    let mut segm = tx2.try_write_at_most(1).expect("write 1");
    fill_segm(&mut segm, &[7u8]);
    drop(segm);
    let mut segm = rx2.try_read_at_most(1).expect("read 1");
    let got = take_segm(&mut segm, 1);
    assert_eq!(got, vec![7]);
    drop(segm);
}

// ---------------------------------------------------------------------------
// 泛型段测试：move_items_* 的 trait 默认实现（ReclSliceRef / ReclSliceMut）
// ---------------------------------------------------------------------------
//
// 测试意图：abs_buff 把 `move_items_*` 提升为 `TrBuffSegmRef` / `TrBuffSegmMut`
// 的 trait 默认方法，任何实现者都必须满足这些默认实现的语义。这里用 abs_buff
// 导出的泛型测试函数（`abs_buff::buffer::segm_tests`，需启用 `segm-tests`
// feature）验证 RingBuffer 专属的两段式段 `ReclSliceRef` / `ReclSliceMut`：
// 数据按序搬移、消费量正确推进、无重复无丢失。
//
// 内部执行设计：每个方向用两个独立 ring（源环装数据、目标环留空）分别借出
// 读段与写段，调用泛型函数完成搬移；随后在目标环上读回内容、在源环上确认
// 已全部消费，验证数据真正按序到达目标存储。

/// 通过 abs_buff 的泛型段测试函数验证 `ReclSliceRef` / `ReclSliceMut` 的
/// `move_items_*` trait 默认实现（四个方向全部覆盖）。
#[test]
fn recl_segm_move_items_trait_defaults() {
    use abs_buff::buffer::segm_tests as t;
    use core::mem::MaybeUninit;

    // —— move_items_to_segm（读段 → 写段，跨两个 ring）——
    {
        let (src_ring, mut src_tx, mut src_rx) = make_ring();
        let (dst_ring, mut dst_tx, mut dst_rx) = make_ring();
        let expect: Vec<u8> = (0..15).collect();

        // 源环写入 16 字节；
        let mut ws = src_tx.try_write_at_most(15).expect("源环写入");
        fill_segm(&mut ws, &expect);
        drop(ws);
        // 分别借出读段（源环）与写段（目标环）；
        let mut src_segm = src_rx.try_read_at_most(15).expect("源环读段");
        let mut dst_segm = dst_tx.try_write_at_most(15).expect("目标环写段");
        let moved = t::test_move_items_to_segm(&mut src_segm, &mut dst_segm, &expect);
        assert_eq!(moved, 15, "泛型函数应返回搬移数量");
        drop(src_segm);
        drop(dst_segm);

        assert_eq!(src_ring.data_size(), 0, "源环必须被全部消费");
        assert_eq!(dst_ring.data_size(), 15, "目标环必须收到全部数据");
        // 目标环读回内容按序校验；
        let mut got = Vec::new();
        while let Ok(segm) = dst_rx.try_read_at_most(8) {
            let len = segm.least_count();
            let mut segm = segm;
            got.extend_from_slice(&take_segm(&mut segm, len));
            drop(segm);
        }
        assert_eq!(got, expect, "目标环内容必须按序到达");
    }

    // —— move_items_from_segm（镜像：写段一侧发起，跨两个 ring）——
    {
        let (src_ring, mut src_tx, mut src_rx) = make_ring();
        let (dst_ring, mut dst_tx, mut dst_rx) = make_ring();
        let expect: Vec<u8> = (0..15).collect();

        let mut ws = src_tx.try_write_at_most(15).expect("源环写入");
        fill_segm(&mut ws, &expect);
        drop(ws);
        let mut src_segm = src_rx.try_read_at_most(15).expect("源环读段");
        let mut dst_segm = dst_tx.try_write_at_most(15).expect("目标环写段");
        let moved = t::test_move_items_from_segm(&mut src_segm, &mut dst_segm, &expect);
        assert_eq!(moved, 15);
        drop(src_segm);
        drop(dst_segm);

        assert_eq!(src_ring.data_size(), 0, "源环必须被全部消费");
        assert_eq!(dst_ring.data_size(), 15, "目标环必须收到全部数据");
        let mut got = Vec::new();
        while let Ok(segm) = dst_rx.try_read_at_most(8) {
            let len = segm.least_count();
            let mut segm = segm;
            got.extend_from_slice(&take_segm(&mut segm, len));
            drop(segm);
        }
        assert_eq!(got, expect, "目标环内容必须按序到达");
    }

    // —— move_items_to_buff（读段 → 本地缓冲，内容由泛型函数校验）——
    {
        let (_src_ring, mut src_tx, mut src_rx) = make_ring();
        let expect: Vec<u8> = (0..15).collect();
        let mut ws = src_tx.try_write_at_most(15).expect("源环写入");
        fill_segm(&mut ws, &expect);
        drop(ws);
        let mut src_segm = src_rx.try_read_at_most(15).expect("源环读段");
        let mut dst_buf = [MaybeUninit::<u8>::uninit(); 15];
        // SAFETY: u8 无 drop，位拷贝安全；
        let moved = unsafe { t::test_move_items_to_buff(&mut src_segm, &mut dst_buf, &expect) };
        assert_eq!(moved, 15);
        drop(src_segm);
    }

    // —— move_items_from_buff（本地缓冲 → 写段，目标环读回校验）——
    {
        let (_dst_ring, mut dst_tx, mut dst_rx) = make_ring();
        let expect: Vec<u8> = (100..115).collect();
        let mut src_buf = [MaybeUninit::<u8>::uninit(); 15];
        let mut dst_segm = dst_tx.try_write_at_most(15).expect("目标环写段");
        // SAFETY: u8 无 drop，位拷贝安全；
        let moved = unsafe { t::test_move_items_from_buff(&mut dst_segm, &mut src_buf, &expect) };
        assert_eq!(moved, 15);
        drop(dst_segm);
        let mut got = Vec::new();
        while let Ok(segm) = dst_rx.try_read_at_most(8) {
            let len = segm.least_count();
            let mut segm = segm;
            got.extend_from_slice(&take_segm(&mut segm, len));
            drop(segm);
        }
        assert_eq!(got, expect, "目标环内容必须按序到达");
    }
}

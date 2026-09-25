//! channels 专用：把一个写段按「一条消息一格」并发地分给若干写者。
//!
//! # 它是什么
//!
//! MPSC 的写路径要能**同时**让多个写者往同一段内存里各写自己那条消息。本类型把从
//! [`SegmMut::take_slice_mut`] 借来的一段内存，按「**一格一条消息**」借给若干写者：
//! 每次 [`ConcurrentSegmMut::grant`] 借出一格，写者写完后那片随 `Drop` 自动上报。
//!
//! **只支持单条消息**：不做 `send_bulk` 那种「一次要 `n + 1` 格」的申请，于是片固定
//! 为一格——既不需要堆内存，也不需要定长数组。
//!
//! # 一个 `AtomicUsize` 说清两件事
//!
//! 片的编号在**本轮**内是 `0..usize::BITS`，片 `Drop` 时把位图里自己那一位置 1：
//!
//! ```text
//! done.fetch_or(1 << bit);          // 片的上报
//! ```
//!
//! 就这么一个原子字，能精确算出两件我们关心的事：
//!
//! * **完成了多少片**——`done.count_ones()`；
//! * **提交前缀有多长**——从第 0 位起连续 1 的个数，即 `(!done).trailing_zeros()`。
//!   后者才是能交给写段的**推进量**：完成是乱序的，第 5 片先完成也不能越过还没
//!   上报的第 2 片（否则读者会与写者并发访问同一格）。
//!
//! 一轮最多 `usize::BITS` 片——一个字的位数就是信息上限。整轮都上报之后，
//! [`ConcurrentSegmMut::commit`] 会把位图清零并把这一轮计入「已轮换」，于是可以
//! 接着借下一轮：段再长也不需要任何额外存储。
//!
//! # 提交链
//!
//! [`ConcurrentSegmMut::lend`] 借到的是一个 [`SegmReclaim`]；`commit` 把已上报前缀
//! 按**差额**报给它，父段的消费偏移因此前进；父段 `drop` 时再按该偏移回收
//! （对环形缓冲而言就是推进写位置）。
//!
//! # 调用方必须守的两个前提
//!
//! 1. **同一时刻只有一个开放的并发写段**；上一段的全部片都解析完（位图回到 0）
//!    之后才能开下一段，否则旧片的迟到上报会写进新段的位号里；
//! 2. 位图由调用方持有并跨段复用——第 1 条不满足时位图就会残留。

use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicUsize, Ordering},
};

use abs_buff::buffer::{SegmMut, SegmReclaim, TrReclaim};

/// 一轮最多能借出多少片：位图的位数。
const ROUND_BITS_: usize = usize::BITS as usize;

// ---------------------------------------------------------------------------
// 片
// ---------------------------------------------------------------------------

/// 借给**单个写者**的一格内存；`Drop` 即上报。
///
/// 只有两条路：要么 [`Piece::send`] 写下这条消息，要么**不发送直接 `drop`**——
/// 后者就是放弃，`Drop` 会把这格补成空洞。两条路都走同一个 `Drop`，
/// 因此不会漏报也不会重复上报，不需要 `finish()` 或 `abandon()` 这类空方法。
pub(crate) struct Piece<'a, T> {
    /// 本片在本轮里的位号。
    bit_: usize,
    /// 这一格。
    cell_: &'a mut MaybeUninit<T>,
    /// 完成位图（由调用方持有）。
    done_: &'a AtomicUsize,
}

impl<T> Piece<'_, T> {
    /// 写下这条消息；随后本片随 `self` 一起 `drop` 并上报。
    pub(crate) fn send(self, item: T) {
        self.cell_.write(item);
    }
}

impl<T> Drop for Piece<'_, T> {
    fn drop(&mut self) {
        self.done_.fetch_or(1usize << self.bit_, Ordering::Release);
    }
}

unsafe impl<'a, T> Send for Piece<'a, T>
where
    T: Send,
{}

unsafe impl<'a, T> Sync for Piece<'a, T>
where
    T: Sync,
{}

// ---------------------------------------------------------------------------
// 并发写段
// ---------------------------------------------------------------------------

/// 一段被借出来、按「一格一条消息」并发分发的写段。
pub(crate) struct ConcurrentSegmMut<'a, T> {
    /// 还没借出去的剩余内存。
    buf_: &'a mut [MaybeUninit<T>],
    /// 完成位图。
    done_: AtomicUsize,
    /// 本轮已借出的片数。
    round_: usize,
    /// 提交器：把消费量报回父段。
    reclaim_: SegmReclaim<'a>,
}

impl<'a, T> ConcurrentSegmMut<'a, T> {
    /// 从 `segm` 借出 `len` 格，转成并发写段。
    ///
    /// - `len` 受 [`SegmMut::take_slice_mut`] 的约束（必须小于段的剩余格数）;
    /// - `init_default` 是兜底写，有些 Piece 可能无法完成任务就 drop，因此调用者可以
    ///   预先写入一个默认值，用于区分 Piece 是否真正被写入
    ///
    /// 返回 `None` 表示借不出来。
    pub(crate) fn lend<R, F>(
        segm: &'a mut SegmMut<'_, T, R>,
        len: usize,
        init_default: F,
    ) -> Option<Self>
    where
        R: TrReclaim,
        F: FnOnce(&mut [MaybeUninit<T>]),
    {
        segm.take_slice_mut(len, |buf, reclaim| {
            init_default(buf);
            ConcurrentSegmMut::new_(buf, reclaim)
        })
    }

    /// 一次性借出大量 Piece
    ///
    /// # SAFETY:
    ///
    /// - The caller should retrieve the concrete `Piece` with a valid index
    ///   according to the returned index value.
    pub(crate) unsafe fn pieces<'f>(
        &'f mut self,
        pieces: &mut [MaybeUninit<Piece<'f, T>>],
    ) -> usize {
        let size = self.buf_.len() - self.round_;
        if size == 0 {
            return size;
        }
        let buf = &mut self.buf_[self.round_..self.round_ + size];
        for (u, cell) in buf.iter_mut().enumerate() {
            let i = self.round_ + u;
            pieces[u].write(Piece {
                bit_: i,
                cell_: cell,
                done_: &self.done_,
            });
        }
        size
    }

    fn new_(
        buf: &'a mut [MaybeUninit<T>],
        reclaim: SegmReclaim<'a>,
    ) -> Self {
        ConcurrentSegmMut {
            buf_: buf,
            done_: AtomicUsize::new(0usize),
            round_: 0,
            reclaim_: reclaim,
        }
    }

    /// 从第 0 位起连续 1 的个数 = 提交前缀（格数）。
    fn prefix_(&self) -> usize {
        let done = self.done_.load(Ordering::Acquire);
        (!done).trailing_zeros() as usize
    }
}

impl<T> Drop for ConcurrentSegmMut<'_, T> {
    fn drop(&mut self) {
        // 兜底：并发阶段结束时把已上报前缀交还，免得调用方忘了 `commit`。
        todo!()
    }
}

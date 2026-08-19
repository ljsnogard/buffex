//! RingBuffer 专用的段类型：写段 / 读段 / 窥视段。
//!
//! 普通的 abs_buff `SegmRef` / `SegmMut` 只能表达**一段物理连续**的缓冲区。
//! 但 RingBuffer 的可用 / 可读区域在环绕缓冲区末端时会被物理拆成两段
//! （例如末端 2 格 + 开端 2 格）。因此这里为 RingBuffer 定制了专用的段类型，
//! 内部用一个 enum 表达"只有一段连续空间"或"拥有两段连续空间"两种可能性，
//! 把两段物理空间视作**逻辑上的一段**——这样生产者 / 消费者可以一次性拿到
//! 跨末端的全部空间，而不是被"单连续 slice"的表示卡死。
//!
//! 这些类型同样实现 `TrBuffSegmRef` / `TrBuffSegmMut`，因此 abs_buff 的管道
//! 机制（PipeJoin）可以直接使用；`as_segm_ref` / `as_segm_mut` 每次交出
//! 当前物理段的一个子段，父段的 offset 在子段 drop 时累计，段整体 drop 时
//! 按已消费量提交给 ring（逐段回收粒度）。

use core::{mem::MaybeUninit, ops::Try, pin::Pin};

use abs_buff::{
    Demand, buffer::{SegmMut, SegmReclaim, SegmRef, TrBuffSegmMut, TrBuffSegmRef, TrBuffSegmView, TrReclaim}
};
// use abs_iter::TrAsSlice;

use super::state_::RingCore;

// ---------------------------------------------------------------------------
// 物理空间的两段式表示
// ---------------------------------------------------------------------------

/// 写段持有的物理空间：一段连续，或两段连续（跨越缓冲区末端）。
pub(super) enum SegmSlicesMut<'a, T> {
    One(&'a mut [MaybeUninit<T>]),
    Two(&'a mut [MaybeUninit<T>], &'a mut [MaybeUninit<T>]),
}

impl<'a, T> SegmSlicesMut<'a, T> {
    #[inline]
    fn len(&self) -> usize {
        match self {
            SegmSlicesMut::One(a) => a.len(),
            SegmSlicesMut::Two(a, b) => a.len() + b.len(),
        }
    }

    /// 按 `offset` 返回剩余空间的两段（不足两段时以空段补齐）。
    fn remaining_mut(&mut self, offset: usize) -> [&mut [MaybeUninit<T>]; 2] {
        match self {
            SegmSlicesMut::One(a) => [&mut a[offset..], &mut []],
            SegmSlicesMut::Two(a, b) => {
                let la = a.len();
                if offset < la {
                    [&mut a[offset..], b]
                } else {
                    [&mut b[offset - la..], &mut []]
                }
            }
        }
    }

    /// `offset` 所在物理段的剩余部分（子段的基础）。
    fn current_mut(&mut self, offset: usize) -> &mut [MaybeUninit<T>] {
        match self {
            SegmSlicesMut::One(a) => &mut a[offset..],
            SegmSlicesMut::Two(a, b) => {
                let la = a.len();
                if offset < la {
                    &mut a[offset..]
                } else {
                    &mut b[offset - la..]
                }
            }
        }
    }

    /// 只读视图版本的 [`SegmSlicesMut::remaining_mut`]。
    fn remaining_ref(&self, offset: usize) -> [&[MaybeUninit<T>]; 2] {
        match self {
            SegmSlicesMut::One(a) => [&a[offset..], &[]],
            SegmSlicesMut::Two(a, b) => {
                let la = a.len();
                if offset < la {
                    [&a[offset..], b]
                } else {
                    [&b[offset - la..], &[]]
                }
            }
        }
    }
}

/// 读段 / 窥视段持有的物理空间：一段连续，或两段连续。
pub(super) enum SegmSlicesRef<'a, T> {
    One(&'a mut [T]),
    Two(&'a mut [T], &'a mut [T]),
}

impl<'a, T> SegmSlicesRef<'a, T> {
    #[inline]
    fn len(&self) -> usize {
        match self {
            SegmSlicesRef::One(a) => a.len(),
            SegmSlicesRef::Two(a, b) => a.len() + b.len(),
        }
    }

    // fn remaining_mut(&mut self, offset: usize) -> [&mut [T]; 2] {
    //     match self {
    //         SegmSlicesRef::One(a) => [&mut a[offset..], &mut []],
    //         SegmSlicesRef::Two(a, b) => {
    //             let la = a.len();
    //             if offset < la {
    //                 [&mut a[offset..], b]
    //             } else {
    //                 [&mut b[offset - la..], &mut []]
    //             }
    //         }
    //     }
    // }

    fn current_mut(&mut self, offset: usize) -> &mut [T] {
        match self {
            SegmSlicesRef::One(a) => &mut a[offset..],
            SegmSlicesRef::Two(a, b) => {
                let la = a.len();
                if offset < la {
                    &mut a[offset..]
                } else {
                    &mut b[offset - la..]
                }
            }
        }
    }

    fn remaining_ref(&self, offset: usize) -> [&[T]; 2] {
        match self {
            SegmSlicesRef::One(a) => [&a[offset..], &[]],
            SegmSlicesRef::Two(a, b) => {
                let la = a.len();
                if offset < la {
                    [&a[offset..], b]
                } else {
                    [&b[offset - la..], &[]]
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 回收器
// ---------------------------------------------------------------------------

/// 提交器：写段 drop 时按已消费量推进写位置。
pub struct WriterReclaim<'a> {
    core: &'a RingCore,
    cap: usize,
}

impl<'a> WriterReclaim<'a> {
    pub(super) const fn new(core: &'a RingCore, cap: usize) -> Self {
        WriterReclaim { core, cap }
    }
}

impl TrReclaim for WriterReclaim<'_> {
    fn reclaim(&mut self, amount: usize) -> usize {
        self.core.advance_write(self.cap, amount);
        0
    }
}

/// 提交器：读段 drop 时按已消费量推进读位置。
pub struct ReaderReclaim<'a> {
    core: &'a RingCore,
    cap: usize,
}

impl<'a> ReaderReclaim<'a> {
    pub(super) const fn new(core: &'a RingCore, cap: usize) -> Self {
        ReaderReclaim { core, cap }
    }
}

impl TrReclaim for ReaderReclaim<'_> {
    fn reclaim(&mut self, amount: usize) -> usize {
        self.core.advance_read(self.cap, amount);
        0
    }
}

/// 读段 drop 时的两种行为：读段提交（推进读位置），窥视段不提交。
pub(super) enum ReadReclaim<'a> {
    /// 读段：drop 时把已消费量提交给 ring（推进读位置）。
    Consume(ReaderReclaim<'a>),
    /// 窥视段：drop 时不提交。
    Peek,
}

pub type ChildReclaim<'a> = SegmReclaim<'a>;

// ---------------------------------------------------------------------------
// 写段
// ---------------------------------------------------------------------------

/// RingBuffer 专用写段：两段物理空间视作逻辑上的一段（见模块文档）。
pub struct ReclSliceMut<'a, T> {
    pieces: SegmSlicesMut<'a, T>,
    /// 已消费（已提交给 ring）的逻辑单元数，跨两段累计。
    offset: usize,
    reclaim: Option<WriterReclaim<'a>>,
}

impl<'a, T> ReclSliceMut<'a, T> {
    pub(super) fn new(pieces: SegmSlicesMut<'a, T>, reclaim: WriterReclaim<'a>) -> Self {
        ReclSliceMut {
            pieces,
            offset: 0,
            reclaim: Option::Some(reclaim),
        }
    }

    /// 逻辑上剩余（未消费）的单元数：两段物理空间之和减去已消费量。
    #[inline]
    pub fn least_count(&self) -> usize {
        self.pieces.len() - self.offset
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.least_count() == 0
    }

    /// 本段逻辑上的总容量（两段物理空间之和）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.pieces.len()
    }

    /// 剩余可写空间按物理段切出（最多两段），供调用方直接写入。
    /// 空段会被过滤，调用方看到的每一段都非空。
    pub fn iter_slices_mut(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<T>]> {
        self.pieces
            .remaining_mut(self.offset)
            .into_iter()
            .filter(|s| !s.is_empty())
    }

    /// 当前物理段的剩余部分作为一个 abs_buff 子段；子段 drop 时通过
    /// [`ChildReclaim`] 把其已消费量累计到父段的 `offset`。
    pub fn as_segm_mut<'f>(&'f mut self) -> SegmMut<'f, T, ChildReclaim<'f>> {
        // 直接借用 `self.pieces` 与 `self.offset`（不同字段，借用检查器
        // 可判定互斥），与 abs_buff 内部 `SegmRef::as_segm_ref` 的做法一致。
        let slice = self.pieces.current_mut(self.offset);
        let reclaim = ChildReclaim::new(Pin::new(&mut self.offset));
        SegmMut::new(slice, reclaim)
    }

    pub fn take_segm_mut<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> Option<SegmMut<'f, T, ChildReclaim<'f>>> {
        let c = self.least_count();
        if c == 0 {
            return Option::None;
        }
        let available = Demand::less_than(c);
        let agreement = demand.compromise(&available)?;
        let max_len = agreement.max()?;
        // 子段只能覆盖当前物理段；跨段部分由下一次 take 处理。
        let cur = self.pieces.current_mut(self.offset);
        let take = core::cmp::min(*max_len, cur.len());
        let slice = &mut cur[..take];
        let reclaim = ChildReclaim::new(Pin::new(&mut self.offset));
        Option::Some(SegmMut::new(slice, reclaim))
    }

    /// See `TrBuffSegmMut::move_items_from_buff`.
    ///
    /// ## Safety
    ///
    /// - See `TrBuffSegmMut::move_items_from_buff`
    #[inline]
    pub unsafe fn move_items_from_buff(&mut self, src: &mut [MaybeUninit<T>]) -> usize {
        unsafe { TrBuffSegmMut::move_items_from_buff(self, src) }
    }
}

impl<'a, T> Drop for ReclSliceMut<'a, T> {
    fn drop(&mut self) {
        let Option::Some(mut r) = self.reclaim.take() else {
            return;
        };
        r.reclaim(self.offset);
    }
}

impl<'a, T> TrBuffSegmView for ReclSliceMut<'a, T> {
    type Item = MaybeUninit<T>;

    #[inline]
    fn is_empty(&self) -> bool {
        ReclSliceMut::is_empty(self)
    }

    #[inline]
    fn least_count(&self) -> usize {
        ReclSliceMut::least_count(self)
    }

    /// 剩余可写空间按物理段切出（最多两段，逻辑上是一段）；空段被过滤。
    fn iter_slices(&self) -> impl IntoIterator<Item = &[Self::Item]> {
        self.pieces
            .remaining_ref(self.offset)
            .into_iter()
            .filter(|s| !s.is_empty())
    }
}

impl<'a, T> TrBuffSegmMut<'a, T> for ReclSliceMut<'a, T> {
    type Reclaimer<'f> = ChildReclaim<'f> where Self: 'f;

    #[inline]
    fn as_segm_mut<'f>(&'f mut self) -> SegmMut<'f, T, Self::Reclaimer<'f>> {
        ReclSliceMut::as_segm_mut(self)
    }

    #[inline]
    fn take_segm_mut<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> impl Try<Output: TrBuffSegmMut<'f, T>> {
        ReclSliceMut::take_segm_mut(self, demand)
    }
}

// ---------------------------------------------------------------------------
// 读段 / 窥视段
// ---------------------------------------------------------------------------

/// RingBuffer 专用读段（窥视段是同一类型、只是 drop 时不提交）。
pub struct ReclSliceRef<'a, T> {
    pieces: SegmSlicesRef<'a, T>,
    offset: usize,
    reclaim: Option<ReadReclaim<'a>>,
}

/// RingBuffer 专用窥视段：窥视不消费，drop 时不推进读位置。
pub type ReclPeekRef<'a, T> = ReclSliceRef<'a, T>;

impl<'a, T> ReclSliceRef<'a, T> {
    pub(super) fn new(pieces: SegmSlicesRef<'a, T>, reclaim: ReadReclaim<'a>) -> Self {
        ReclSliceRef {
            pieces,
            offset: 0,
            reclaim: Option::Some(reclaim),
        }
    }

    /// 逻辑上剩余（未消费）的单元数。
    #[inline]
    pub fn least_count(&self) -> usize {
        self.pieces.len() - self.offset
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.least_count() == 0
    }

    /// 本段逻辑上的总容量（两段物理空间之和）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.pieces.len()
    }

    /// 剩余可读空间按物理段切出（最多两段，逻辑上是一段）；空段被过滤。
    pub fn iter_slices(&self) -> impl Iterator<Item = &[T]> {
        self.pieces
            .remaining_ref(self.offset)
            .into_iter()
            .filter(|s| !s.is_empty())
    }

    pub fn take_segm_ref<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> Option<SegmRef<'f, T, ChildReclaim<'f>>> {
        let c = self.least_count();
        if c == 0 {
            return Option::None;
        }
        let available = Demand::less_than(c);
        let agreement = demand.compromise(&available)?;
        let max_len = agreement.max()?;
        let cur = self.pieces.current_mut(self.offset);
        let take = core::cmp::min(*max_len, cur.len());
        let slice = &mut cur[..take];
        let reclaim = ChildReclaim::new(Pin::new(&mut self.offset));
        Option::Some(SegmRef::new(slice, reclaim))
    }

    /// 当前物理段的剩余部分作为一个 abs_buff 子段（同写段的设计）。
    pub fn as_segm_ref<'f>(&'f mut self) -> SegmRef<'f, T, ChildReclaim<'f>> {
        let slice = self.pieces.current_mut(self.offset);
        let reclaim = ChildReclaim::new(Pin::new(&mut self.offset));
        SegmRef::new(slice, reclaim)
    }

    /// See [abs_buff::buffer::TrBuffSegmRef::move_items_to_buff]
    ///
    /// ## Safety
    ///
    /// - See [abs_buff::buffer::TrBuffSegmRef::move_items_to_buff]
    pub unsafe fn move_items_to_buff(&mut self, dst: &mut [MaybeUninit<T>]) -> usize {
        unsafe { TrBuffSegmRef::move_items_to_buff(self, dst) }
    }
}

impl<'a, T> Drop for ReclSliceRef<'a, T> {
    fn drop(&mut self) {
        let Option::Some(r) = self.reclaim.take() else {
            return;
        };
        match r {
            ReadReclaim::Consume(mut r) => {
                r.reclaim(self.offset);
            }
            ReadReclaim::Peek => {}
        }
    }
}

impl<'a, T> TrBuffSegmView for ReclSliceRef<'a, T> {
    type Item = T;

    #[inline]
    fn is_empty(&self) -> bool {
        ReclSliceRef::is_empty(self)
    }

    #[inline]
    fn least_count(&self) -> usize {
        ReclSliceRef::least_count(self)
    }

    /// 剩余可读空间按物理段切出（最多两段，逻辑上是一段）；空段被过滤。
    fn iter_slices(&self) -> impl IntoIterator<Item = &[Self::Item]> {
        self.pieces
            .remaining_ref(self.offset)
            .into_iter()
            .filter(|s| !s.is_empty())
    }
}

impl<'a, T> TrBuffSegmRef<'a, T> for ReclSliceRef<'a, T> {
    type Reclaimer<'f> = ChildReclaim<'f> where Self: 'f;

    #[inline]
    fn as_segm_ref<'f>(&'f mut self) -> SegmRef<'f, T, Self::Reclaimer<'f>> {
        ReclSliceRef::as_segm_ref(self)
    }

    #[inline]
    fn take_segm_ref<'f>(
        &'f mut self,
        demand: &Demand<usize>,
    ) -> impl Try<Output: TrBuffSegmRef<'f, T>> {
        ReclSliceRef::take_segm_ref(self, demand)
    }
}

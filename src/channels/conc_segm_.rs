use core::{
    cmp::min,
    future::{self, Future, IntoFuture},
    mem::MaybeUninit,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

use abs_buff::{
    buffer::{SegmMut, SegmReclaim, TrReclaim},
    x_deps::abs_cancel,
};
use abs_cancel::{NonCancellableToken, TrCancellationToken, TrMayCancel};

/// 一批最多能借出多少片：位图的位数。
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
    /// 本片在本批里的位号。
    bit_: u16,
    segm_: NonNull<ConcurrentSegmMut<'a, T>>,
}

impl<'a, T> Piece<'a, T> {
    pub const fn new(bit: u16, segm: &ConcurrentSegmMut<'a, T>) -> Self {
        Piece {
            bit_: bit,
            segm_: NonNull::from_ref(segm),
        }
    }

    /// 写下这条消息；随后本片随 `self` 一起 `drop` 并上报。
    pub fn send(mut self, item: T) {
        self.cell_().write(item);
    }

    const fn bit_usize(&self) -> usize {
        self.bit_ as usize
    }

    const fn done_(&self) -> &AtomicUsize {
        let segm = unsafe { self.segm_.as_ref() };
        &segm.done_
    }

    const fn buf_(&self) -> &[MaybeUninit<T>] {
        let segm = unsafe { self.segm_.as_ref() };
        segm.buf_
    }

    const fn cell_(&mut self) -> &mut MaybeUninit<T> {
        let segm = unsafe { self.segm_.as_mut() };
        &mut segm.buf_[self.bit_ as usize]
    }
}

impl<'a, T> Drop for Piece<'a, T> {
    fn drop(&mut self) {
        self.done_().fetch_or(1usize << self.bit_, Ordering::Release);
        let segm = unsafe { self.segm_.as_mut() };
        if self.bit_usize() != segm.pieces_ - 1 {
            return;
        };
        let Option::Some(waker) = segm.waker_.take() else {
            return;
        };
        waker.wake()
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

// ----------------------------------------------------------------------------
// 并发写段
// ----------------------------------------------------------------------------

pub(crate) struct Pieces<'a, 'f, T>(&'f mut ConcurrentSegmMut<'a, T>);

impl<'a, 'f, T> core::iter::Iterator for &mut Pieces<'a, 'f, T> {
    type Item = Piece<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        todo!()
    }
}

// ----------------------------------------------------------------------------
// 并发写段
// ----------------------------------------------------------------------------

/// 一段被借出来、按「一格一条消息」并发分发的写段。
pub(crate) struct ConcurrentSegmMut<'a, T> {
    /// 借来的整段内存，**不切**；`round_` 是它里面已借出的前缀长度。
    buf_: &'a mut [MaybeUninit<T>],
    /// 本批的完成位图：bit `i` 对应「本批内第 `i` 片」。
    done_: AtomicUsize,
    /// 实际已构造的 Piece 数量
    pieces_: usize,
    /// 提交器：把消费量报回父段。
    reclaim_: SegmReclaim<'a>,

    waker_: Option<Waker>,
}

impl<'a, T> ConcurrentSegmMut<'a, T> {
    /// 从 `segm` 借出 `len` 格，转成并发写段。
    ///
    /// - `len` 受 [`SegmMut::take_slice_mut`] 的约束（必须小于段的剩余格数），
    ///   因此借不出来的情形返回 `None`；
    /// - `init_default` 是兜底写：有些片可能不会被 `send` 就 `drop`，调用者可以
    ///   预先写入一个默认值，用于区分片是否真正被写入。
    ///
    /// 一段最多只能借出 `usize::BITS` 片（位图宽度）；要更多，请在本类型 `drop`
    /// 之后再从同一父段 `take_slice_mut` 一段。
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

    pub async fn pieces_async<'f>(
        &'f mut self,
    ) -> (Pieces<'a, 'f, T>, CommitAsync<'a, 'f, T>) {
        let this = self as *mut Self;
        let waker = future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
        self.waker_ = Option::Some(waker);
        (Pieces(self), CommitAsync(unsafe { &mut *this }))
    }

    fn new_(
        buf: &'a mut [MaybeUninit<T>],
        reclaim: SegmReclaim<'a>,
    ) -> Self {
        ConcurrentSegmMut {
            buf_: buf,
            done_: AtomicUsize::new(0usize),
            pieces_: 0usize,
            reclaim_: reclaim,
            waker_: Option::None,
        }
    }

    /// 从第 0 位起连续 1 的个数 = 本批已归还的连续前缀（格数）。
    ///
    /// 位图只覆盖本批的前 `round_` 位，因此结果截到 `round_`：即便有迟到的片
    /// 写进了高位，提交也不会越过真正借出去的格（越过就会把未初始化内存暴露给
    /// 读者）。
    fn prefix_(&self) -> usize {
        let done = self.done_.load(Ordering::Acquire);
        min((!done).trailing_zeros() as usize, self.pieces_)
    }
}

impl<T> Drop for ConcurrentSegmMut<'_, T> {
    fn drop(&mut self) {
        todo!()
    }
}

pub(crate) struct CommitAsync<'a, 'f, T>(&'f mut ConcurrentSegmMut<'a, T>);

impl<'a, 'f, T> IntoFuture for CommitAsync<'a, 'f, T> {
    type IntoFuture = CommitFuture<'a, 'f, T, NonCancellableToken>;
    type Output = ();

    fn into_future(self) -> Self::IntoFuture {
        CommitFuture {
            params_: self.0,
            cancel_: NonCancellableToken::new()
        }
    }
}

impl<'a, 'f, T> TrMayCancel<'f> for CommitAsync<'a, 'f, T> {
    type MayCancelFuture<'g, C> = CommitFuture<'a, 'g, T, C>
    where
        'g: 'f,
        Self: 'g,
        C: 'g + TrCancellationToken;

    type MayCancelOutput = ();

    fn may_cancel_with<C>(
        self,
        cancel: C,
    ) -> Self::MayCancelFuture<'f, C>
    where
        C: 'f + TrCancellationToken
    {
        todo!()
    }
}

pub(crate) struct CommitFuture<'a, 'f, T, K>
where
    K: TrCancellationToken,
{
    params_: &'f mut ConcurrentSegmMut<'a, T>,
    cancel_: K,
}

impl<'a, 'f, T, K> Future for CommitFuture<'a, 'f, T, K>
where
    K: TrCancellationToken,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this_mut = unsafe { self.get_unchecked_mut() };
        let segm_mut = &mut this_mut.params_;
        let all_committed = false; // todo: 根据 done 判断是否全部已完成
        if all_committed {
            return Poll::Ready(());
        }
        if this_mut.cancel_.is_cancelled() {
            return Poll::Ready(());
        }
        // 每次 poll 都用当前 waker 覆盖，避免任务迁移后 waker 失效
        segm_mut.waker_ = Option::Some(cx.waker().clone());
        Poll::Pending
    }
}

//! 段读写的小工具与 bulk 填充器。
//!
//! 这些函数只在本模块内部使用；它们的共同前提是**段必须来自本队列的核心**，
//! 因此段的单元类型就是 [`SlotMsg`](super::slot_::SlotMsg)。

use core::mem::MaybeUninit;

use abs_buff::buffer::TrReclaim;
use mm_ptr::x_deps::abs_mm::mem_alloc::{CoreAlloc, TrMalloc};

use abs_buff::{
    Demand,
    x_deps::abs_cancel::{TrCancellationToken, TrMayCancel},
};

use super::{
    ChannelError, slot_::SlotMsg,
    spsc_::{BufOf, Msg, WrReclaim},
};
use crate::circular_buff::{BufConsumer, Producer, ReclSliceMut, ReclSliceRef};

/// 上载半部（`Producer`）的完整类型：**两个队列共用**。
pub(super) type ProducerOf<T, A> = Producer<BufConsumer<Msg<T>>, BufOf<T, A>, Msg<T>, A>;

/// 把一条协议消息推进写段；段已满时把消息**原样交还**。
///
/// 走 `move_items_from_buff`（与 `circ_buff` 自己的测试同一条路径）：它一边位拷贝
/// 搬入、一边推进段的已消费偏移，段 drop 时才会按偏移提交正确的格数。
///
/// 交还（而不是就地 drop）是为了让**异步重试不丢消息**：`send_async` 可能在
/// 「预留失败 / 被取消」后重来，消息必须还能拿回来。
pub(super) fn push_msg_<T, R>(
    segm: &mut ReclSliceMut<'_, SlotMsg<T>, R>,
    msg: SlotMsg<T>,
) -> Result<(), SlotMsg<T>>
where
    R: TrReclaim,
{
    if segm.least_count() == 0 {
        return Err(msg);
    }
    let mut staging = [MaybeUninit::new(msg)];
    let moved = segm.move_items_from_buff(&mut staging);
    if moved == 1 {
        // 已搬走：暂存槽位现在是未初始化的。`MaybeUninit` 本身没有析构 glue，
        // 因此让数组自然离开作用域即可（不会二次 drop）。
        Ok(())
    } else {
        // 未被搬走：从暂存里取回消息并交还。
        // SAFETY: `moved != 1` 说明元素未被搬出，槽位仍处于已初始化状态。
        let recovered = unsafe { staging[0].assume_init_read() };
        Err(recovered)
    }
}

/// 把一条**载荷**推进写段；段已满时把载荷放回 `slot` 并返回 `false`。
///
/// 这是异步重试路径的关键工具：调用者把消息留在 `Option` 里，只有真正写进环里才
/// 移走它。
pub(super) fn push_payload_<T, R>(
    segm: &mut ReclSliceMut<'_, SlotMsg<T>, R>,
    slot: &mut Option<T>,
) -> bool
where
    R: TrReclaim,
{
    let Some(item) = slot.take() else {
        return false;
    };
    match push_msg_(segm, SlotMsg::Payload(item)) {
        Ok(()) => true,
        Err(SlotMsg::Payload(item)) => {
            *slot = Option::Some(item);
            false
        }
        // 类型上不可能：本函数只推载荷，失败时交还的必然也是载荷。
        Err(_) => false,
    }
}

/// 从读段取出一条协议消息；段已空返回 `None`。
pub(super) fn pop_msg_<T, R>(
    segm: &mut ReclSliceRef<'_, SlotMsg<T>, R>,
) -> Option<SlotMsg<T>>
where
    R: TrReclaim,
{
    if segm.least_count() == 0 {
        return None;
    }
    let mut dst = [MaybeUninit::<SlotMsg<T>>::uninit()];
    // SAFETY: 位拷贝搬出；所有权随 `assume_init_read` 转移给返回值，
    // 因此 `dst` 不再持有任何需 drop 的值。
    let moved = unsafe { segm.move_items_to_buff(&mut dst) };
    if moved != 1 {
        return None;
    }
    // SAFETY: `moved == 1` 表示第 0 格已被写入并移出。
    Some(unsafe { dst[0].assume_init_read() })
}

/// 写入一帧的预告头（放在连续段第一格）；失败说明段已满。
pub(super) fn push_bulk_head_<T, R>(
    segm: &mut ReclSliceMut<'_, SlotMsg<T>, R>,
    n: usize,
) -> bool
where
    R: TrReclaim,
{
    push_msg_(segm, SlotMsg::BulkHead { len: n }).is_ok()
}

/// bulk 填充器：生产者**自己往已预留的帧里填**成员。
///
/// 由 `try_send_bulk` / `send_bulk_async` **返回**（不是闭包参数）：那时写许可已经在
/// 手，因此填充本身完全同步。
///
/// # 它只持有一个写段
///
/// 段按值持有，**不需要任何堆分配**，也不需要保存写许可——因为填充一定发生在持有
/// 写许可的那个方法**内部**（见 `MpscSender::try_send_bulk` 的说明），段不可能比
/// 写许可活得更久。`drop` 即提交整帧，此时写位置在写许可保护下推进。
///
/// # 空洞的补齐
///
/// 只填了 `k < n` 条时，剩下的 `n - k` 格由 [`Drop`] 补写成空洞。于是**提交区间永远
/// 全部已初始化**，读端不会读到未初始化内存；即使生产者中途 panic（栈展开会跑
/// `Drop`），本帧也会被补成「已写前缀 + 空洞尾巴」。
///
/// [`Drop`]: BulkWriter::drop
pub struct BulkWriter<'a, T, A = CoreAlloc>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。已预留的连续写段。
    segm_: Option<ReclSliceMut<'a, SlotMsg<T>, WrReclaim<'a, T, A>>>,
    /// 本帧的成员格数（不含预告头）。
    len_: usize,
    /// 已填成员数。
    written_: usize,
}

impl<'a, T, A> BulkWriter<'a, T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    pub(super) fn new_(
        segm: ReclSliceMut<'a, SlotMsg<T>, WrReclaim<'a, T, A>>,
        len: usize,
    ) -> Self {
        BulkWriter { segm_: Option::Some(segm), len_: len, written_: 0 }
    }

    /// 本帧最多可填的成员数（申请时给定的 `n`）。
    #[inline]
    pub fn len(&self) -> usize {
        self.len_
    }

    /// 本帧是否不允许任何成员（`n == 0`）。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len_ == 0
    }

    /// 已填成员数。
    #[inline]
    pub fn written(&self) -> usize {
        self.written_
    }

    /// 填一条成员；帧已满返回 `false`。
    pub fn push(&mut self, item: T) -> bool {
        if self.written_ >= self.len_ {
            return false;
        }
        let Option::Some(segm) = self.segm_.as_mut() else {
            return false;
        };
        if push_msg_(segm, SlotMsg::Payload(item)).is_ok() {
            self.written_ = self.written_.saturating_add(1);
            true
        } else {
            false
        }
    }

    /// 把剩余成员格补成空洞（幂等）。
    fn finish_(&mut self) {
        let Option::Some(segm) = self.segm_.as_mut() else {
            return;
        };
        while self.written_ < self.len_ {
            if push_msg_(segm, SlotMsg::Void).is_err() {
                break;
            }
            self.written_ = self.written_.saturating_add(1);
        }
    }
}

impl<T, A> Drop for BulkWriter<'_, T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    fn drop(&mut self) {
        // 生产者提前收尾（正常结束或 panic 展开）时，补齐本帧剩余的空洞格，
        // 保证「提交区间全部已初始化」这一不变量。随后 `segm_` 被丢弃并提交整帧；
        // 写许可由调用它的方法持有，因此提交一定在放锁之前发生。
        self.finish_();
    }
}

// ---------------------------------------------------------------------------
// 关闭：两个队列**共用同一套机制**
// ---------------------------------------------------------------------------

/// **尽力**把 `Closing` 协议信号放进环里（非阻塞）。
///
/// 返回值：`Ok(true)` 已提交；`Ok(false)` 环满（可稍后重试）；`Err(_)` 已无意义
/// （写端已关 / 读端已关）。
///
/// 两端共用同一个实现——SPSC 靠 `&mut Producer` 的独占借用拿到写权，MPSC 在写许可
/// 之下拿到 `&mut Producer`，拿到之后要做的事完全一样。
pub(super) fn try_push_closing_<T, A>(
    prod: &mut ProducerOf<T, A>,
) -> Result<bool, ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + mm_ptr::x_deps::abs_mm::mem_alloc::TrMalloc + Clone,
{
    let demand = Demand::exactly(1);
    let obtained = prod.try_write(&demand);
    if obtained.as_ref().pick_left().is_none() {
        let err = obtained
            .pick_right()
            .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
        let e = ChannelError::from(err);
        return match e {
            ChannelError::Stuffed => Ok(false),
            other => Err(other),
        };
    }
    let Some(mut segm) = obtained.pick_left() else {
        return Ok(false);
    };
    Ok(push_msg_(&mut segm, SlotMsg::Closing).is_ok())
}

/// 把 `Closing` 信号放进环里，环满时**等空间**再试。
///
/// 返回是否成功放入了信号。等到「已无意义」（写端/读端已关）时返回 `false`——
/// 此时调用者落标志即可，那也是带外 EOF 的兜底路径。
pub(super) async fn push_closing_wait_<T, A, C>(
    prod: &mut ProducerOf<T, A>,
    cancel: C,
) -> bool
where
    T: Send + Sync + 'static,
    A: Send + Sync + mm_ptr::x_deps::abs_mm::mem_alloc::TrMalloc + Clone,
    C: TrCancellationToken,
{
    let demand = Demand::exactly(1);
    loop {
        match try_push_closing_(prod) {
            Ok(true) => return true,
            Ok(false) => {
                let _ = prod
                    .write_async(&demand)
                    .may_cancel_with(cancel.child_token())
                    .await;
            }
            Err(_) => return false,
        }
    }
}

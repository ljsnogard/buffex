//! 有界 MPSC 队列：多个写者经 [`CooperativeRwLock`] 排队写入。
//!
//! # 承载与竞争
//!
//! * **承载**：消息存放在 [`circular_buff`](crate::circular_buff) 的环形缓冲里，
//!   单元类型是内部的槽位协议 [`SlotMsg<T>`](super::slot_)——公开 API 只呈现用户
//!   载荷 `T`；
//! * **竞争**：写者之间用上游 `atomic_sync` 的
//!   [`CooperativeRwLock`](atomic_sync::rwlock::cooperative::CooperativeRwLock)
//!   排队。它是**异步运行时无关**的协作式锁：排队者以 waker 挂起。
//!
//! # 锁**保护 `Producer`**，且覆盖「取位 → 写入 → 提交」全程
//!
//! 共享状态是 `Shared<MpscShared>`，而 `MpscShared` 里的锁**保护的就是 `Producer`
//! 本身**（不是「锁 + 数据并列」）。因为 `WriterGuard` 实现了
//! `DerefMut<Target = Producer>`，取得写许可即取得 `&mut Producer`——写路径上因此
//! **没有一处 `unsafe`**：不需要 `UnsafeCell`，也不需要从 `&self` 伪造 `&mut`。
//!
//! 会话（`CooperativeAcqSession`）在每个写入口**就地取得**，是局部变量；写许可不
//! 越出方法作用域。把「持锁的填充器」作为返回值交出去会要求会话比方法活得久，而锁
//! 就在本发送端自己的共享分配里——那是自引用，诚实写法表达不出来，只能伪造寿命
//! （即绕过借用检查）。会话分离的设计本来就是要让「持有许可」不越出作用域，因此
//! MPSC 的 bulk 填充发生在方法内部（见 [`MpscSender::try_send_bulk`]）。
//!
//! 本实现**故意**让写许可覆盖整个写操作，而不是「取到位置就放锁」：
//!
//! * 底层 `Producer::try_write` 只「借出坐标、不推进写位置」，写位置要等段 drop
//!   才由 `advance_write` 推进。若在提交前放锁，两位写者会**拿到同一个写位置**
//!   而互相覆盖（实测两段首地址相同），先提交者还会把后提交者尚未写入的格并入
//!   可读范围，使读端读到未初始化数据；
//! * 要让「取到位置就放锁」成立，核心必须同时提供**占位游标 + 提交游标**，并让
//!   消费端只认已提交前缀。那是独立的一步改造，暂时**放弃**该优化。
//!
//! 于是写者之间是**串行**的：后到者要等前一位写完。代价换来的是一条清晰不变量：
//! **提交顺序 == 取位顺序**，因此单游标 `wp` 就足够，消费端语义不受生产端竞争影响。
//!
//! # 发送侧入口与空洞语义
//!
//! | 入口 | 占用格数 | 场景 |
//! | --- | --- | --- |
//! | [`MpscSender::try_send`] / [`MpscSender::send_async`] | 1 | 单体消息 |
//! | [`MpscSender::try_send_bulk`] / [`MpscSender::send_bulk_async`] | `n + 1` | 一次连续放 `n` 条（预告头额外占一格） |
//!
//! bulk 入口接收一个**同步闭包**（而不是返回持锁的填充器），原因见上。空洞的补齐、
//! 预告头、以及「只写了前缀也能被消费端拿到」的语义与 SPSC 完全一致，见
//! [`crate::channels::spsc_`] 的模块文档与 [`BulkWriter`]。

use abs_async_iter::TrAsyncIterator;
use abs_buff::{Demand, gen_may_cancel_future, x_deps::abs_cancel};
use abs_cancel::{TrCancellationToken, TrMayCancel};
use abs_mm::mem_alloc::{CoreAlloc, TrMalloc};
use atomic_sync::rwlock::cooperative::CooperativeRwLockOwned;
use mm_ptr::{Shared, x_deps::abs_mm};

use crate::circular_buff::{
    BufConsumer, BufProducer, Consumer,
    builder::{BuilderError, CircularBuffBuilder, ConsumerSetBuilder},
};
use super::{
    ChannelError,
    segm_::{
        BulkWriter, pop_msg_, push_bulk_head_, push_closing_wait_, push_payload_,
        try_push_closing_, ProducerOf,
    },
    spsc_::{BufOf, CoreOf, Msg},
};

/// 迭代路径统一使用的需求区间。
const ANY_: Demand<usize> = Demand::at_least(1);

/// 写端共享状态的锁：**它保护的就是 `Producer` 本身**。
type LockOf<T, A> = CooperativeRwLockOwned<ProducerOf<T, A>>;

/// 预留「尽可能大」的一段的需求；理由同 `spsc_::BULK_DEMAND_`
/// （段要放进返回值，它借用的 `Demand` 必须活到那时）。
static BULK_DEMAND_: Demand<usize> = Demand::at_least(1);

// ---------------------------------------------------------------------------
// 共享状态
// ---------------------------------------------------------------------------

/// 多写者共享的那一份状态：**锁保护 `Producer`**。
///
/// # 为什么锁要保护数据，而不是与数据并列
///
/// `WriterGuard` 实现了 `DerefMut<Target = T>`，所以「取得写许可」就直接等价于
/// 「取得 `&mut Producer`」。这样写路径上**没有一处 `unsafe`**：
///
/// * 不需要 `UnsafeCell<Producer>`，也不需要从 `&self` 伪造 `&mut`——`&mut` 是锁
///   合法借出的；
/// * 会话（`CooperativeAcqSession`）在每个方法里就地取得，是**局部变量**；写许可
///   不逃出方法作用域，因此没有任何结构体需要保存会话，也就不会有「伪造寿命」的
///   问题。
///
/// `core_ref_` 单独留一份核心句柄，用于**免锁**观察容量 / 空闲 / 数据量。
struct MpscShared<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。保护 `Producer` 的协作式读写锁（只用写侧）。
    lock_: LockOf<T, A>,
    /// `_` 后缀：非 pub 字段。核心句柄：免锁观察尺寸，并在最后一位写者消失时关闭。
    core_ref_: Shared<CoreOf<T, A>, A>,
}

impl<T, A> MpscShared<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 取得写许可并在其保护下执行 `f`；写许可**在本方法内**生灭。
    ///
    /// 会话就地在栈上取得——这正是 `atomic_sync` 分离 session 与 guard 的用法：
    /// 「获取许可的能力」不必也不应比「持有许可」活得更久。
    fn with_prod_<R, F>(&self, f: F) -> Result<R, ChannelError>
    where
        F: FnOnce(&mut ProducerOf<T, A>) -> Result<R, ChannelError>,
    {
        let mut sess = self.lock_.acquire_session();
        let mut guard = sess.try_write().map_err(|_| ChannelError::Stuffed)?;
        // `WriterGuard: DerefMut<Target = Producer>` ⇒ 合法的 `&mut Producer`。
        f(&mut guard)
    }
}

impl<T, A> Drop for MpscShared<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    fn drop(&mut self) {
        // 最后一位写者消失 ⇒ 关闭写端，读端才会读到 EOF。
        //
        // SAFETY: 本结构正在销毁，`Shared` 的引用计数已归零，不存在任何其他持有者，
        // 也就不可能有并发访问——此刻取得 `&mut Producer` 是独占的。
        // 与显式关闭**同一套机制**：先尽力放 `Closing` 协议信号，再落标志兜底。
        //
        // SAFETY: 本结构正在销毁，`Shared` 引用计数已归零，不存在任何其他持有者，
        // 也就不可能有并发访问——此刻取得 `&mut Producer` 是独占的。
        let prod = unsafe { &mut *self.lock_.as_mut_ptr() };
        let _ = try_push_closing_(prod);
        prod.close();
    }
}

// ---------------------------------------------------------------------------
// 队列构造
// ---------------------------------------------------------------------------

/// MPSC 队列的构造入口。
///
/// `S` 是**写者数量上界**：与 `circ_buff` 在构建期决定拓扑的取向一致。
///
/// # Examples
///
/// ```no_run
/// use buffex::channels::MpscChannel;
///
/// # async fn demo() -> Result<(), buffex::channels::ChannelError> {
/// // 4 个写者，容量 1024 格。
/// let (tx, rx) = MpscChannel::<u8, 4>::with_capacity(1024)
///     .expect("容量非法")
///     .into_parts()
///     .await
///     .expect("装配失败");
/// # let _ = (tx, rx);
/// # Ok(())
/// # }
/// ```
pub struct MpscChannel<T, const S: usize, A = CoreAlloc>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。
    capacity_: usize,
    /// `_` 后缀：非 pub 字段。
    _unuse_t_: core::marker::PhantomData<fn() -> (T, A)>,
}

impl<T, const S: usize, A> MpscChannel<T, S, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + Default + 'static,
{
    /// 以指定容量（格数）创建。
    ///
    /// # Errors
    ///
    /// 容量非法返回 [`MpscBuildError`]；写者数量上界 `S` 为 0 时返回
    /// [`ChannelError::Argument`]。
    pub fn with_capacity(capacity: usize) -> Result<Self, MpscBuildError> {
        if S == 0 {
            return Err(MpscBuildError::Channel(ChannelError::Argument));
        }
        let _probe = CircularBuffBuilder::<BufOf<T, A>, Msg<T>, A>::with_capacity(
            capacity,
        )
        .map_err(|e| MpscBuildError::Builder(super::map_builder_err_(e)))?;
        Ok(MpscChannel {
            capacity_: capacity,
            _unuse_t_: core::marker::PhantomData,
        })
    }

    /// 容量（格数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity_
    }

    /// 写者数量上界（类型参数 `S`）。
    #[inline]
    pub fn sender_limit(&self) -> usize {
        S
    }

    /// 装配底层核心并拆成写端与读端。
    pub async fn into_parts(
        self,
    ) -> Result<(MpscSender<T, A>, MpscReceiver<T, A>), MpscBuildError> {
        let builder =
            CircularBuffBuilder::<BufOf<T, A>, Msg<T>, A>::with_capacity(
                self.capacity_,
            )
            .map_err(|e| MpscBuildError::Builder(super::map_builder_err_(e)))?;
        let ready: ConsumerSetBuilder<BufConsumer<Msg<T>>, BufOf<T, A>, Msg<T>, A> =
            builder.consumer_passive();
        let mut ready = ready.producer_passive();
        let (tx, rx) = ready
            .build_async()
            .await
            .map_err(|e| MpscBuildError::Builder(super::map_builder_err_(e)))?;
        let core_ref_ = tx.core_ref_of_();
        // 锁**保护** `Producer`：取得写许可即取得 `&mut Producer`。
        let shared = Shared::new(
            MpscShared {
                lock_: CooperativeRwLockOwned::new(
                    tx,
                    core::sync::atomic::AtomicUsize::new(0),
                ),
                core_ref_,
            },
            A::default(),
        );
        Ok((
            MpscSender { shared_: shared },
            MpscReceiver { inner_: rx, pending_: 0 },
        ))
    }
}

/// [`MpscChannel::with_capacity`] / [`MpscChannel::into_parts`] 的错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MpscBuildError {
    /// 底层构建器拒绝该容量。
    Builder(BuilderError<()>),
    /// 通道层自己的参数校验失败。
    Channel(ChannelError),
}

impl core::fmt::Display for MpscBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MpscBuildError::Builder(e) => write!(f, "MpscBuildError::Builder({e:?})"),
            MpscBuildError::Channel(e) => e.fmt(f),
        }
    }
}

impl core::error::Error for MpscBuildError {}

// ---------------------------------------------------------------------------
// 发送端
// ---------------------------------------------------------------------------

/// 发送端半部：可克隆，每个克隆体是一个独立写者。
pub struct MpscSender<T = u8, A = CoreAlloc>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。共享状态：锁保护着 `Producer`。
    shared_: Shared<MpscShared<T, A>, A>,
}

impl<T, A> Clone for MpscSender<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    fn clone(&self) -> Self {
        MpscSender { shared_: self.shared_.clone() }
    }
}

impl<T, A> MpscSender<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 队列容量（格数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.shared_.core_ref_.capacity()
    }

    /// 当前可写格数（免锁读取核心状态）。
    #[inline]
    pub fn free_size(&self) -> usize {
        self.shared_.core_ref_.free_size()
    }

    /// 当前可读格数（含预告头与空洞，仅供观察）。
    #[inline]
    pub fn data_size(&self) -> usize {
        self.shared_.core_ref_.data_size()
    }

    /// 关闭本写者（**同步、非阻塞**版本）。
    ///
    /// 它需要写入位：若写入位正被别的写者占着，本调用**不会生效**并返回
    /// [`ChannelError::Stuffed`]——请改用 [`MpscSender::close_async`]。
    ///
    /// # Errors
    ///
    /// 写入位被占时返回 [`ChannelError::Stuffed`]。
    pub fn close(&mut self) -> Result<(), ChannelError> {
        self.shared_.with_prod_(|prod| {
            // 尽力放信号；环满时靠标志位的带外 EOF 兜底。
            let _ = try_push_closing_(prod);
            prod.close();
            Ok(())
        })
    }

    /// 异步关闭本写者：**先等到写入位**，再把写端标记为关闭。
    ///
    /// 「先等到写入位」正是关键：若有写者正在写（已占位、尚未提交），同步关闭会让
    /// 读端在它提交之前就看到 EOF，那条消息就永远送不到了。本方法保证**此前已提交的
    /// 消息都在环里**之后才关闭。
    ///
    /// # 取消语义
    ///
    /// * **尚未开始关闭**就被取消（含「还没拿到写入位」）⇒ 返回
    ///   [`ChannelError::Cancelled`]，什么都不做；
    /// * 关闭一旦开始（标志已落下），取消只中断「等主动消费端排空」这一步，
    ///   **不会撤销关闭**。
    pub fn close_async<'f>(&'f mut self) -> MpscCloseAsync<'f, 'f, T, A> {
        MpscCloseAsync::new(self)
    }

    /// **尝试**发送一条单体消息：占 1 格。
    ///
    /// 「尝试」指写入位只抢一次，抢不到就返回 [`ChannelError::Stuffed`] 交给调用者
    /// 决定；需要等锁就用 [`MpscSender::send_async`]。
    ///
    /// # Errors
    ///
    /// 无空位或写入位被别的写者占着返回 [`ChannelError::Stuffed`]；写端已关闭返回
    /// [`ChannelError::Closing`]。
    pub fn try_send(&mut self, item: T) -> Result<(), ChannelError> {
        let mut slot = Option::Some(item);
        self.shared_.with_prod_(|prod| {
            let demand = Demand::exactly(1);
            let obtained = prod.try_write(&demand);
            if obtained.as_ref().pick_left().is_none() {
                let err = obtained
                    .pick_right()
                    .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
                return Err(ChannelError::from(err));
            }
            let Some(mut segm) = obtained.pick_left() else {
                return Err(ChannelError::Stuffed);
            };
            if !push_payload_(&mut segm, &mut slot) {
                return Err(ChannelError::Stuffed);
            }
            Ok(())
        })
    }

    /// 异步发送**一条单体消息**：队满时等空间，写入位被占时等锁。
    ///
    /// # 取消语义
    ///
    /// 取消令牌就绪即返回 [`ChannelError::Cancelled`]；**消息不会**被写入。
    pub fn send_async<'f>(
        &'f mut self,
        item: T,
    ) -> MpscSendAsync<'f, 'f, T, A> {
        MpscSendAsync::new(self, item)
    }

    /// 取得一张写票。
    fn ticket_(&self) -> MpscWriteTicket<T, A> {
        MpscWriteTicket { shared_: self.shared_.clone() }
    }

    /// **尝试**发送一个 bulk 帧：`f` 在**写许可保护之下同步填充**。
    ///
    /// # 为什么填充在方法内部
    ///
    /// 写许可（`WriterGuard`）借用一个 `CooperativeAcqSession`，而会话又借用锁；把
    /// 「持锁的填充器」作为返回值交出去，就要求会话比本方法活得更久——而锁就在本
    /// 发送端自己的共享分配里，那是**自引用**，诚实写法表达不出来（只能伪造寿命，
    /// 也就是绕过借用检查）。会话分离的设计本来就是要让「持有许可」不越出作用域。
    ///
    /// 因此 `f` 也**不能**中途 `await`：持锁跨 `await` 会让争同一把锁的其他写者
    /// 永久等待。
    ///
    /// 返回实际填入的成员数；未填的成员格补成空洞，整帧仍被提交。
    ///
    /// # Errors
    ///
    /// 需要 `n + 1` 格连续空间；不足或写入位被占返回 [`ChannelError::Stuffed`]。
    pub fn try_send_bulk<F>(&mut self, n: usize, f: F) -> Result<usize, ChannelError>
    where
        F: FnOnce(&mut BulkWriter<'_, T, A>),
    {
        self.shared_.with_prod_(|prod| {
            let obtained = prod.try_write(&BULK_DEMAND_);
            if obtained.as_ref().pick_left().is_none() {
                let err = obtained
                    .pick_right()
                    .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
                return Err(ChannelError::from(err));
            }
            let Some(mut segm) = obtained.pick_left() else {
                return Err(ChannelError::Stuffed);
            };
            if !push_bulk_head_(&mut segm, n) {
                return Err(ChannelError::Stuffed);
            }
            let mut writer = BulkWriter::new_(segm, n);
            f(&mut writer);
            // 读计数 → `writer` drop（提交整帧，仍在写许可之内）→ 闭包返回 →
            // `with_prod_` 放锁。顺序不可颠倒。
            Ok(writer.written())
        })
    }

    /// 异步发送一个 bulk 帧：先等空间与写入位，再在许可保护下同步填充。
    ///
    /// # 取消语义
    ///
    /// 取消令牌就绪即返回 [`ChannelError::Cancelled`]；`f` 未被调用。
    pub fn send_bulk_async<'f, F>(
        &'f mut self,
        n: usize,
        f: F,
    ) -> MpscSendBulkAsync<'f, 'f, T, A, F>
    where
        F: FnOnce(&mut BulkWriter<'_, T, A>) + 'static,
    {
        MpscSendBulkAsync::new(self, n, f)
    }
}

impl<T, A> TrAsyncIterator for MpscSender<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
{
    /// 产出物：一次写入的许可（[`MpscWriteTicket`]）。
    type Item = MpscWriteTicket<T, A>;
    type Err = ChannelError;

    type NextAsync<'f>
        = MpscSendNextAsync<'f, 'f, T, A>
    where
        Self: 'f;

    #[inline]
    fn next_async(&mut self) -> Self::NextAsync<'_> {
        MpscSendNextAsync::new(self)
    }
}

// ---------------------------------------------------------------------------
// 写票
// ---------------------------------------------------------------------------

/// 一次写入的许可，由 [`MpscSender::next_async`] 产出。
///
/// 票上的方法各自完成一次**完整的**写操作（在写锁之内取位 → 写入 → 提交）。若此时
/// 写入位被别的写者占着，返回 [`ChannelError::Stuffed`]，调用者可重试。
pub struct MpscWriteTicket<T = u8, A = CoreAlloc>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。
    shared_: Shared<MpscShared<T, A>, A>,
}

impl<T, A> MpscWriteTicket<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 用这张票发送一条单体消息。
    ///
    /// # Errors
    ///
    /// 同 [`MpscSender::send`]。
    pub fn send(self, item: T) -> Result<(), ChannelError> {
        let mut slot = Option::Some(item);
        self.shared_.with_prod_(|prod| {
            let demand = Demand::exactly(1);
            let obtained = prod.try_write(&demand);
            if obtained.as_ref().pick_left().is_none() {
                let err = obtained
                    .pick_right()
                    .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
                return Err(ChannelError::from(err));
            }
            let Some(mut segm) = obtained.pick_left() else {
                return Err(ChannelError::Stuffed);
            };
            if !push_payload_(&mut segm, &mut slot) {
                return Err(ChannelError::Stuffed);
            }
            Ok(())
        })
    }

    /// 放弃本次写入。
    pub fn skip(self) {}
}

// ---------------------------------------------------------------------------
// 接收端
// ---------------------------------------------------------------------------

/// 接收端半部：不可克隆（单消费者）。
pub struct MpscReceiver<T = u8, A = CoreAlloc>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。
    inner_: Consumer<BufProducer<Msg<T>>, BufOf<T, A>, Msg<T>, A>,
    /// 当前 bulk 帧还剩多少成员格未消费。
    pending_: usize,
}

impl<T, A> MpscReceiver<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 队列容量（格数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner_.capacity()
    }

    /// 当前可读格数（含预告头与空洞）。
    #[inline]
    pub fn data_size(&self) -> usize {
        self.inner_.data_size()
    }

    /// 写端是否已全部关闭。
    #[inline]
    pub fn is_writer_closed(&self) -> bool {
        self.inner_.is_producer_closed()
    }

    /// 取出一条用户载荷；语义同 [`SpscReceiver::recv`](super::SpscReceiver::recv)。
    ///
    /// # Errors
    ///
    /// 读端已关闭、或写端已关闭且已读空时返回 [`ChannelError::Closing`]。
    pub fn recv(&mut self) -> Result<Option<T>, ChannelError> {
        loop {
            let msg = match self.pop_one_()? {
                Option::Some(m) => m,
                Option::None => return Ok(Option::None),
            };
            match msg {
                Msg::Payload(t) => {
                    if self.pending_ > 0 {
                        self.pending_ -= 1;
                    }
                    return Ok(Option::Some(t));
                }
                Msg::Void => {
                    if self.pending_ > 0 {
                        self.pending_ -= 1;
                    }
                }
                Msg::BulkHead { len } => {
                    self.pending_ = len;
                }
                // 关闭信号：带内 EOF——排在它前面的载荷都已经交付过了。
                Msg::Closing => return Err(ChannelError::Closing),
            }
        }
    }

    fn pop_one_(&mut self) -> Result<Option<Msg<T>>, ChannelError> {
        let demand = Demand::exactly(1);
        let obtained = self.inner_.try_read(&demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ConsumerError::<usize>::Closing);
            return match err {
                crate::circular_buff::ConsumerError::Drained(_) => Ok(Option::None),
                other => Err(ChannelError::from(other)),
            };
        }
        let Some(mut segm) = obtained.pick_left() else {
            return Ok(Option::None);
        };
        Ok(pop_msg_(&mut segm))
    }
}

impl<T, A> TrAsyncIterator for MpscReceiver<T, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
{
    /// 产出物：一条用户载荷。
    type Item = T;
    type Err = ChannelError;

    type NextAsync<'f>
        = MpscReceiveNextAsync<'f, 'f, T, A>
    where
        Self: 'f;

    #[inline]
    fn next_async(&mut self) -> Self::NextAsync<'_> {
        MpscReceiveNextAsync::new(self)
    }
}

// ---------------------------------------------------------------------------
// 生成的可取消 future
// ---------------------------------------------------------------------------

/// 写端 `next_async`：拿到一张写票。
///
/// 没有空位时先取写入位、再 `await` 底层可写等待 future——即**等空间的写者同样占着
/// 写入位**，这与「锁覆盖全程」是同一个决定。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(MpscSendNext, pub)]
async fn mpsc_send_next_async_<'f, T, A, C>(
    sender: &'f mut MpscSender<T, A>,
    cancel: C,
) -> Result<MpscWriteTicket<T, A>, ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    let shared = &*sender.shared_;
    let demand = ANY_;
    loop {
        if cancel.is_cancelled() {
            return Err(ChannelError::Cancelled);
        }
        if shared.core_ref_.free_size() > 0 {
            return Ok(sender.ticket_());
        }
        // 等空间：取得写许可后在许可保护下等待（等空间的写者也占着写入位）。
        let mut sess = shared.lock_.acquire_session();
        let Result::Ok(mut guard) = sess
            .write_async()
            .may_cancel_with(cancel.child_token())
            .await
        else {
            return Err(ChannelError::Cancelled);
        };
        let _ = guard
            .write_async(&demand)
            .may_cancel_with(cancel.child_token())
            .await;
    }
}

/// 读端 `next_async`：取得一条用户载荷。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(MpscReceiveNext, pub)]
async fn mpsc_receive_next_async_<'f, T, A, C>(
    receiver: &'f mut MpscReceiver<T, A>,
    cancel: C,
) -> Result<T, ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    let demand = ANY_;
    loop {
        if cancel.is_cancelled() {
            return Err(ChannelError::Cancelled);
        }
        match receiver.recv() {
            Ok(Option::Some(t)) => return Ok(t),
            Ok(Option::None) => {}
            Err(e) => return Err(e),
        }
        let _ = receiver
            .inner_
            .read_async(&demand)
            .may_cancel_with(cancel.child_token())
            .await;
    }
}

/// 写端 `send_async`：取写许可（`await`，一次），在其保护下等空间并发送。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(MpscSend, pub)]
async fn mpsc_send_async_<'f, T, A, C>(
    sender: &'f mut MpscSender<T, A>,
    item: T,
    cancel: C,
) -> Result<(), ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    // 会话就地取得；写许可由 `guard` 持有，且**不越出本函数**。
    let mut sess = sender.shared_.lock_.acquire_session();
    let Result::Ok(mut guard) = sess
        .write_async()
        .may_cancel_with(cancel.child_token())
        .await
    else {
        return Err(ChannelError::Cancelled);
    };
    let mut slot = Option::Some(item);
    loop {
        if cancel.is_cancelled() {
            return Err(ChannelError::Cancelled);
        }
        let demand = Demand::exactly(1);
        let obtained = guard.try_write(&demand);
        if obtained.as_ref().pick_left().is_some() {
            let Some(mut segm) = obtained.pick_left() else {
                return Err(ChannelError::Stuffed);
            };
            if !push_payload_(&mut segm, &mut slot) {
                return Err(ChannelError::Stuffed);
            }
            return Ok(());
        }
        let err = obtained
            .pick_right()
            .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
        match ChannelError::from(err) {
            // 队满：持锁等空间；醒来后空间归本写者所有（许可没放）。
            ChannelError::Stuffed => {
                let _ = guard
                    .write_async(&demand)
                    .may_cancel_with(cancel.child_token())
                    .await;
            }
            other => return Err(other),
        }
    }
}

/// 写端 `send_bulk_async`：等写入位后在许可保护下等空间、预留整帧并同步填充。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(MpscSendBulk, pub)]
async fn mpsc_send_bulk_async_<'f, T, A, F, C>(
    sender: &'f mut MpscSender<T, A>,
    n: usize,
    f: F,
    cancel: C,
) -> Result<usize, ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    F: FnOnce(&mut BulkWriter<'_, T, A>) + 'static,
    C: TrCancellationToken,
{
    let mut sess = sender.shared_.lock_.acquire_session();
    let Result::Ok(mut guard) = sess
        .write_async()
        .may_cancel_with(cancel.child_token())
        .await
    else {
        return Err(ChannelError::Cancelled);
    };
    // 闭包只在**预留成功之后**才被取走，因此等空间的过程不会消耗它。
    let mut slot = Option::Some(f);
    loop {
        if cancel.is_cancelled() {
            return Err(ChannelError::Cancelled);
        }
        let obtained = guard.try_write(&BULK_DEMAND_);
        if obtained.as_ref().pick_left().is_some() {
            let Option::Some(f) = slot.take() else {
                return Ok(0);
            };
            let Some(mut segm) = obtained.pick_left() else {
                return Err(ChannelError::Stuffed);
            };
            if !push_bulk_head_(&mut segm, n) {
                return Err(ChannelError::Stuffed);
            }
            // `writer` 在本轮作用域内 drop（提交整帧），随后 `guard` 才 drop。
            let mut writer = BulkWriter::new_(segm, n);
            f(&mut writer);
            return Ok(writer.written());
        }
        let err = obtained
            .pick_right()
            .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
        match ChannelError::from(err) {
            ChannelError::Stuffed => {
                let _ = guard
                    .write_async(&BULK_DEMAND_)
                    .may_cancel_with(cancel.child_token())
                    .await;
            }
            other => return Err(other),
        }
    }
}

/// 写端 `close_async`：等到写入位之后关闭写端。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(MpscClose, pub)]
async fn mpsc_close_async_<'f, T, A, C>(
    sender: &'f mut MpscSender<T, A>,
    cancel: C,
) -> Result<(), ChannelError>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    let mut sess = sender.shared_.lock_.acquire_session();
    // 等写入位 ⇒ 等所有在写的写者提交完，此前提交的消息都在环里。
    let Result::Ok(mut guard) = sess
        .write_async()
        .may_cancel_with(cancel.child_token())
        .await
    else {
        return Err(ChannelError::Cancelled);
    };
    if cancel.is_cancelled() {
        // 关闭还没开始：什么都不做，如实报告取消。
        return Err(ChannelError::Cancelled);
    }
    // 1) 持写许可把**协议信号**放进环里（环满时等空间）——与 SPSC 共用同一套实现；
    //    持许可还保证「信号之后不会再有写者插入」。
    let _ = push_closing_wait_(&mut guard, cancel.child_token()).await;
    // 2) 落写端标志并交给 `Producer` 自己的异步关闭（排空 / 发信号给主动消费端）。
    guard
        .close_async()
        .may_cancel_with(cancel.child_token())
        .await;
    Ok(())
}

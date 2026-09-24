//! 有界 MPSC 队列：多个写者经 [`CooperativeRwLock`] 竞争一个**写入位**。
//!
//! # 承载与竞争
//!
//! * **承载**：消息存放在 [`circular_buff`](crate::circular_buff) 的环形缓冲里。
//!   与 SPSC 一样，**没有任何 `T: Copy` / `T: Clone` 约束**：写者在借出的段里
//!   原地构造消息；
//! * **竞争**：写者之间用上游 `atomic_sync` 的
//!   [`CooperativeRwLock`](atomic_sync::rwlock::cooperative::CooperativeRwLock)
//!   排队。它是**异步运行时无关**的协作式读写锁：排队者以 waker 挂起，不阻塞
//!   线程，也不依赖任何执行器。
//!
//! **本模块没有自己的缓冲、没有自己的锁。**
//!
//! # 写者侧的排队模型
//!
//! 写者端也实现 [`TrAsyncIterator`](abs_async_iter::TrAsyncIterator)（与读端**同一个
//! trait**，不另立门户）：每次 `next_async` 竞争写入权，拿到后返回一段可写空间。
//! **后续排队者不必等前面的写者写完**——锁只覆盖「取得写入位」这一小段临界区，
//! 不含数据搬运：
//!
//! ```text
//! 写者 A:  [取写入位] ──────写入自己的数据──────→ [提交]
//! 写者 B:           [取写入位] ────写入────→ [提交]
//! 写者 C:                    [取写入位] ...
//! ```
//!
//! # 放弃写入
//!
//! 「先占位、后写入」意味着写者可能在**取得写入位之后**放弃（future 被取消、
//! 任务 panic、调用者显式放弃）。本实现的立场是：**要求消息体自身表达「空」**
//! ——写者要么把消息构造完整，要么把该槽位留成 `T = Option<U>` 的 `None`，由
//! 消费端按业务语义跳过。这样就不需要在 `circ_buff` 的核心上再加一层「提交游标
//! + 每笔占位状态」的空洞协议。
//!
//! 若一次没有构造任何单元就 drop 写段，等价于「什么也没发生」：段提交量为 0，
//! 写位置不前进。
//!
//! # 与 SPSC 的两类入口保持一致
//!
//! | 入口 | 一次处理 | 消息本体 | 适用 |
//! | --- | --- | --- | --- |
//! | [`MpscSender::send`] / [`MpscReceiver::recv`] | 1 个 | 直接 move | 消息现成 |
//! | [`MpscSender::try_next`] / [`MpscReceiver::try_next`] | 多个 | 借出段、就地构造 / 读取 | 批量、不可 `Clone` |

use core::{borrow::BorrowMut, cell::UnsafeCell, mem::MaybeUninit};

use abs_async_iter::TrAsyncIterator;
use abs_buff::{Demand, gen_may_cancel_future, x_deps::abs_cancel};
use abs_cancel::{TrCancellationToken, TrMayCancel};
use atomic_sync::{
    rwlock::cooperative::CooperativeRwLockOwned,
};
use mm_ptr::{
    Owned, Shared,
    x_deps::abs_mm::mem_alloc::{CoreAlloc, TrMalloc},
};

use super::{
    ChannelError,
    spsc_::{
        extend_read_segm_, extend_write_segm_, fill_one_, map_builder_err_,
        take_one_,
    },
};
use crate::circular_buff::{
    BufConsumer, BufProducer, Consumer, Producer,
    abs_comp_::TrProducer,
    builder::{BuilderError, CircularBuffBuilder, ConsumerSetBuilder},
    core_::CircCore,
    reclaim_::{ReaderReclaim, WriterReclaim},
};

/// 迭代路径统一使用的需求区间：至少 1 个单元、不设上限。
const ANY_: Demand<usize> = Demand::at_least(1);

// ---------------------------------------------------------------------------
// 具体类型别名
// ---------------------------------------------------------------------------

/// 队列核心：被动生产端 × 被动消费端（与 SPSC 同构，只是由多写者共享）。
pub type MpscCore<B, T> = CircCore<BufProducer<T>, BufConsumer<T>, B, T>;

/// 写提交器。
pub type MpscWriterReclaim<'a, B, T> = WriterReclaim<'a, MpscCore<B, T>>;

/// 读提交器。
pub type MpscReaderReclaim<'a, B, T> = ReaderReclaim<'a, MpscCore<B, T>>;

/// 写段：一次发送可用的可写空间（drop 时提交已构造的单元数）。
pub type MpscWriteSegm<'a, B, T> =
    crate::circular_buff::ReclSliceMut<'a, T, MpscWriterReclaim<'a, B, T>>;

/// 读段：一次接收可用的可读数据（drop 时提交已消费量）。
pub type MpscReadSegm<'a, B, T> =
    crate::circular_buff::ReclSliceRef<'a, T, MpscReaderReclaim<'a, B, T>>;

/// 读段（接收端半部的通用形态，`P` 为生产端端类型）。
pub type MpscReadSegmOf<'a, P, B, T> = crate::circular_buff::ReclSliceRef<
    'a,
    T,
    ReaderReclaim<'a, CircCore<P, BufConsumer<T>, B, T>>,
>;

/// 队列的默认数据承载。
pub type MpscDefaultBuf<T, A = CoreAlloc> = Owned<[MaybeUninit<T>], A>;

/// 写端半部的默认形态。
pub type MpscSenderDefault<T, A = CoreAlloc> = MpscSender<MpscDefaultBuf<T, A>, T, A>;

/// 读端半部的默认形态。
pub type MpscReceiverDefault<T, A = CoreAlloc> =
    MpscReceiver<BufProducer<T>, MpscDefaultBuf<T, A>, T, A>;

// ---------------------------------------------------------------------------
// 共享核心：承载 + 写入位
// ---------------------------------------------------------------------------

/// 多写者共享的那一份状态。
///
/// 写者之间只通过 `write_queue_` 协调：谁拿到写锁，谁就拥有「写入位」的取得权。
struct MpscShared<B, T, A>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 上传（`Producer`）半部**只用于取位**。
    ///
    /// 为什么是 [`UnsafeCell`]：`Producer::try_write` 需要 `&mut self`，而
    /// `mm_ptr::Shared` 只提供 `Deref`（没有 `DerefMut`）；多写者又必须共享同一个
    /// `Producer`（它的 `core_ref_` 就是那块唯一的环形缓冲）。
    ///
    /// # Safety
    ///
    /// 对 `producer_` 的每一次访问都在 `write_queue_` 的**写锁**保护之下（见
    /// [`MpscShared::produce_`]），因此任一时刻只有一个写者拿到 `&mut`。
    producer_: UnsafeCell<Producer<BufConsumer<T>, B, T, A>>,
    /// 保活：`Producer` 半部与所有段都只借用核心，核心本身由这份 `Shared` 托住。
    ///
    /// 段被 `next_async` 交给调用者时会延长生命周期（见
    /// `mpsc_send_next_async_`），因此必须保证「段活着时核心一定活着」。
    core_ref_: Shared<CircCore<BufProducer<T>, BufConsumer<T>, B, T>, A>,
    /// 写入位：一次只允许一位写者占住。
    write_queue_: CooperativeRwLockOwned<()>,
    /// `_` 后缀：非 pub 字段。
    _unuse_t_: core::marker::PhantomData<fn() -> T>,
}

impl<B, T, A> MpscShared<B, T, A>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 在**已持有写入位**的前提下取得上载半部的独占引用。
    ///
    /// # Safety
    ///
    /// 调用者必须持有 `write_queue_` 的写许可；在该许可存活期间不会有第二个调用
    /// 者进入本方法，因此返回的 `&mut` 是独占的。
    #[allow(clippy::mut_from_ref)]
    unsafe fn produce_(&self) -> &mut Producer<BufConsumer<T>, B, T, A> {
        // SAFETY: 由调用者的写许可保证独占（见本函数与 `producer_` 的文档）。
        unsafe { &mut *self.producer_.get() }
    }
}

// ---------------------------------------------------------------------------
// 写端
// ---------------------------------------------------------------------------

/// 发送端半部：可克隆，每个克隆体是一个独立写者。
///
/// 实现 [`TrAsyncIterator`]，产出物是 [`MpscWriteSegm`]——调用者在段里**原地
/// 构造**消息（可含不可 `Clone` 的类型），段 drop 时提交。
pub struct MpscSender<B, T = u8, A = CoreAlloc>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。
    shared_: Shared<MpscShared<B, T, A>, A>,
}

impl<B, T, A> Clone for MpscSender<B, T, A>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    fn clone(&self) -> Self {
        MpscSender { shared_: self.shared_.clone() }
    }
}

/// 接收端半部：不可克隆（单消费者）。
pub struct MpscReceiver<P, B, T = u8, A = CoreAlloc>
where
    P: Send + Sync + TrProducer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// `_` 后缀：非 pub 字段。
    inner_: Consumer<P, B, T, A>,
}

/// MPSC 队列的构造入口。
///
/// `S` 是**写者数量上界**：与 `circ_buff` 在构建期决定拓扑的取向一致——队列在
/// 构造期就知道有多少个写者共享这块缓冲，实现因此无需动态扩容。
///
/// # Examples
///
/// ```no_run
/// use buffex::channels::MpscChannel;
///
/// # async fn demo() -> Result<(), buffex::channels::ChannelError> {
/// // 4 个写者，容量 1024 个单元。
/// let (tx, rx) = MpscChannel::<u8, 4>::with_capacity(1024)
///     .expect("容量非法")
///     .into_parts()
///     .await
///     .expect("装配失败");
/// // `tx` 可克隆给多个写者，`rx` 只有一个。
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
    _unuse_t_: core::marker::PhantomData<fn() -> (T, A)>,
}

impl<T, const S: usize, A> MpscChannel<T, S, A>
where
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + Default + 'static,
{
    /// 以指定容量（单元数）创建。
    ///
    /// # Errors
    ///
    /// 容量非法（与 `circ_buff` 的构建器同一套校验）返回 [`MpscBuildError`]；
    /// 写者数量上界 `S` 为 0 时返回 [`ChannelError::Argument`]。
    pub fn with_capacity(capacity: usize) -> Result<Self, MpscBuildError> {
        if S == 0 {
            return Err(MpscBuildError::Channel(ChannelError::Argument));
        }
        let _probe =
            CircularBuffBuilder::<MpscDefaultBuf<T, A>, T, A>::with_capacity(
                capacity,
            )
            .map_err(|e| MpscBuildError::Builder(map_builder_err_(e)))?;
        Ok(MpscChannel {
            capacity_: capacity,
            _unuse_t_: core::marker::PhantomData,
        })
    }

    /// 容量（单元数）。
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
    ///
    /// 写端是 [`Clone`]：每个克隆体是一个独立写者，它们在同一条
    /// [`CooperativeRwLockOwned`] 上排队竞争写入位。
    pub async fn into_parts(
        self,
    ) -> Result<(MpscSenderDefault<T, A>, MpscReceiverDefault<T, A>), MpscBuildError>
    {
        let builder =
            CircularBuffBuilder::<MpscDefaultBuf<T, A>, T, A>::with_capacity(
                self.capacity_,
            )
            .map_err(|e| MpscBuildError::Builder(map_builder_err_(e)))?;
        let ready: ConsumerSetBuilder<BufConsumer<T>, MpscDefaultBuf<T, A>, T, A> =
            builder.consumer_passive();
        let mut ready = ready.producer_passive();
        let (tx, rx) = ready
            .build_async()
            .await
            .map_err(|e| MpscBuildError::Builder(map_builder_err_(e)))?;
        let core_ref = tx.core_ref_of_();
        let shared = Shared::new(
            MpscShared {
                producer_: UnsafeCell::new(tx),
                core_ref_: core_ref,
                write_queue_: CooperativeRwLockOwned::new_owned(()),
                _unuse_t_: core::marker::PhantomData,
            },
            A::default(),
        );
        Ok((MpscSender { shared_: shared }, MpscReceiver { inner_: rx }))
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
            // `BuilderError` 未实现 `Display`（外部类型，加不了 impl），因此用
            // `Debug` 输出其分类。
            MpscBuildError::Builder(e) => write!(f, "MpscBuildError::Builder({e:?})"),
            MpscBuildError::Channel(e) => e.fmt(f),
        }
    }
}

impl core::error::Error for MpscBuildError {}

impl<B, T, A> MpscSender<B, T, A>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 队列容量（单元数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.shared_.core_ref_.capacity()
    }

    /// 当前可写空间。
    #[inline]
    pub fn free_size(&self) -> usize {
        self.shared_.core_ref_.free_size()
    }

    /// 同步关闭本写者。
    ///
    /// 语义同 `std::sync::mpsc` 的 `Sender::drop`：最后一个写者关闭后队列进入
    /// EOF，读端把残留数据读完后得到 `Ok(None)`。
    pub fn close(&mut self) {
        let mut sess = self.shared_.write_queue_.acquire_session();
        if sess.try_write().is_ok() {
            // SAFETY: 已持有写入位的写许可。
            unsafe { self.shared_.produce_() }.close();
        }
    }

    /// 非阻塞取得一个**写段**（竞争写入位成功后借出）。
    ///
    /// 锁只覆盖「取得写入位」这一段：拿到段之后写者即可自行构造消息，锁在
    /// `next_async` 返回时已释放。
    ///
    /// `Ok(Option::None)` 表示**暂态**拿不到写入位（被别的写者占着）或没有空位
    /// （背压）——与读端 `try_next` 的 `Ok(None)`（暂态无数据）对称。
    ///
    /// # Errors
    ///
    /// 写端已关闭返回 [`ChannelError::Closing`]。
    pub fn try_next<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Result<Option<MpscWriteSegm<'f, B, T>>, ChannelError> {
        let mut sess = self.shared_.write_queue_.acquire_session();
        // 写入位被占：暂态，告知调用者稍后重试（异步路径会 await 同一把锁）。
        let Result::Ok(_guard) = sess.try_write() else {
            return Ok(Option::None);
        };
        // SAFETY: 已持有写入位的写许可。
        let obtained = unsafe { self.shared_.produce_() }.try_write(demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
            return match ChannelError::from(err) {
                ChannelError::Stuffed => Ok(Option::None),
                other => Err(other),
            };
        }
        Ok(obtained.pick_left())
    }

    /// 就地发送**一个**现成的值（消息本体已就绪时的快捷入口；不暴露段）。
    ///
    /// # Errors
    ///
    /// 同 [`MpscSender::try_next`]。
    pub fn send(&mut self, item: T) -> Result<(), ChannelError> {
        let mut sess = self.shared_.write_queue_.acquire_session();
        let _guard = sess
            .try_write()
            .map_err(|_| ChannelError::Stuffed)?;
        let demand = Demand::exactly(1);
        // SAFETY: 已持有写入位的写许可。
        let obtained = unsafe { self.shared_.produce_() }.try_write(&demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
            return Err(ChannelError::from(err));
        }
        let Some(mut segm) = obtained.pick_left() else {
            return Err(ChannelError::Stuffed);
        };
        if !fill_one_(&mut segm, item) {
            return Err(ChannelError::Stuffed);
        }
        Ok(())
    }
}

impl<P, B, T, A> MpscReceiver<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 借用底层 [`Consumer`] 半部。
    #[inline]
    pub fn inner_mut(&mut self) -> &mut Consumer<P, B, T, A> {
        &mut self.inner_
    }

    /// 队列容量（单元数）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner_.capacity()
    }

    /// 当前可读数据量。
    #[inline]
    pub fn data_size(&self) -> usize {
        self.inner_.data_size()
    }

    /// 写端是否已全部关闭。
    #[inline]
    pub fn is_writer_closed(&self) -> bool {
        self.inner_.is_producer_closed()
    }

    /// 关闭读端。
    #[inline]
    pub fn close(&mut self) {
        let mut fut = self.inner_.close_async();
        let _ = (&mut fut,);
    }

    /// 非阻塞取得一个**读段**。
    ///
    /// `Ok(None)` 表示**暂态**无数据；`Err(Closing)` 表示 EOF 或读端已关闭。
    ///
    /// # Errors
    ///
    /// 见上。
    pub fn try_next<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Result<Option<MpscReadSegmOf<'f, P, B, T>>, ChannelError> {
        let obtained = self.inner_.try_read(demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ConsumerError::<usize>::Closing);
            return match err {
                crate::circular_buff::ConsumerError::Drained(_) => Ok(None),
                other => Err(ChannelError::from(other)),
            };
        }
        Ok(obtained.pick_left())
    }

    /// 取出**一个**值（`T` 不需要 `Copy`：值是从段里 move 出来的）。
    ///
    /// # Errors
    ///
    /// 读端已关闭，或写端已关闭且缓冲已读空（EOF）时返回
    /// [`ChannelError::Closing`]。
    pub fn recv(&mut self) -> Result<Option<T>, ChannelError> {
        let demand = Demand::exactly(1);
        let obtained = self.inner_.try_read(&demand);
        if obtained.as_ref().pick_left().is_none() {
            let err = obtained
                .pick_right()
                .unwrap_or(crate::circular_buff::ConsumerError::<usize>::Closing);
            return match err {
                crate::circular_buff::ConsumerError::Drained(_) => Ok(None),
                other => Err(ChannelError::from(other)),
            };
        }
        let Some(mut segm) = obtained.pick_left() else {
            return Ok(None);
        };
        Ok(take_one_(&mut segm))
    }
}

// ---------------------------------------------------------------------------
// 两端都实现 abs_async_iter
// ---------------------------------------------------------------------------

impl<B, T, A> TrAsyncIterator for MpscSender<B, T, A>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]> + 'static,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
{
    /// 产出物：一段可写空间。
    type Item = MpscWriteSegm<'static, B, T>;
    type Err = ChannelError;

    type NextAsync<'f>
        = MpscSendNextAsync<'f, 'f, B, T, A>
    where
        Self: 'f;

    #[inline]
    fn next_async(&mut self) -> Self::NextAsync<'_> {
        MpscSendNextAsync::new(self)
    }
}

impl<P, B, T, A> TrAsyncIterator for MpscReceiver<P, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T> + 'static,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]> + 'static,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
{
    type Item = MpscReadSegmOf<'static, P, B, T>;
    type Err = ChannelError;

    type NextAsync<'f>
        = MpscReceiveNextAsync<'f, 'f, P, B, T, A>
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

/// 写端 `next_async`：竞争写入位，拿到后借出一段可写空间。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(MpscSendNext, pub)]
async fn mpsc_send_next_async_<'f, B, T, A, C>(
    sender: &'f mut MpscSender<B, T, A>,
    cancel: C,
) -> Result<Option<MpscWriteSegm<'static, B, T>>, ChannelError>
where
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]> + 'static,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    // 唯一需要等待的地方：写入位被别的写者占着。协作式锁把排队者挂起（waker），
    // 取消令牌就绪时其等待 future 会自行回收队列槽位。
    let shared = &*sender.shared_;
    let mut sess = shared.write_queue_.acquire_session();
    let _guard = sess
        .write_async()
        .may_cancel_with(cancel)
        .await
        .map_err(|_| ChannelError::Cancelled)?;

    let demand = ANY_;
    // SAFETY: 已持有写入位的写许可。
    let obtained = unsafe { shared.produce_() }.try_write(&demand);
    if obtained.as_ref().pick_left().is_none() {
        let err = obtained
            .pick_right()
            .unwrap_or(crate::circular_buff::ProducerError::<usize>::Closing);
        return match ChannelError::from(err) {
            // 队满（背压）是**暂态**：本轮没有产出，调用者可稍后重试，而不是
            // 把整个迭代判死。
            ChannelError::Stuffed => Ok(Option::None),
            other => Err(other),
        };
    }
    let Some(segm) = obtained.pick_left() else {
        return Ok(Option::None);
    };
    // SAFETY: 保活责任由 `MpscShared::core_ref_` 承担（见 `extend_write_segm_`）。
    Ok(Option::Some(unsafe { extend_write_segm_(segm) }))
}

/// 读端 `next_async`：取得一段可读数据。
#[allow(clippy::needless_lifetimes)]
#[gen_may_cancel_future(MpscReceiveNext, pub)]
async fn mpsc_receive_next_async_<'f, P, B, T, A, C>(
    receiver: &'f mut MpscReceiver<P, B, T, A>,
    cancel: C,
) -> Result<Option<MpscReadSegmOf<'static, P, B, T>>, ChannelError>
where
    P: Send + Sync + TrProducer<Data = T> + 'static,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]> + 'static,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone + 'static,
    C: TrCancellationToken,
{
    let demand = ANY_;
    if cancel.is_cancelled() {
        return Ok(Option::None);
    }
    let obtained = receiver.inner_.try_read(&demand);
    if obtained.as_ref().pick_left().is_none() {
        let err = obtained
            .pick_right()
            .unwrap_or(crate::circular_buff::ConsumerError::<usize>::Closing);
        return match err {
            crate::circular_buff::ConsumerError::Drained(_) => Ok(Option::None),
            other => Err(ChannelError::from(other)),
        };
    }
    let Some(segm) = obtained.pick_left() else {
        return Ok(Option::None);
    };
    // SAFETY: 保活责任由 `MpscShared::core_ref_` 承担（见 `extend_write_segm_`）。
    Ok(Option::Some(unsafe { extend_read_segm_(segm) }))
}

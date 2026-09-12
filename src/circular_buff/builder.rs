//! 构建器：在**构建期**决定两端的模式（被动 / 主动）与用途（数据从哪来、到
//! 哪去），并把端类型（`circ_buff_` 的 `BuffProducer` /
//! `BuffConsumer` / `DeviceProducer` / `DeviceConsumer`）装配进核心。
//!
//! # 设计意图
//!
//! * **构造期定模式**：模式一旦构建完成即固定，之后由 hook 联动驱动；
//! * **类型状态强制**：`CircularBuffBuilder`（仅容量）→ 任一「单端已定」的中态
//!   （[`ProducerSetBuilder`] 生产端已定 / [`ConsumerSetBuilder`] 消费端已定）→
//!   [`ReadyBuilder`]（两端已定）→ `build_async`。type-state 链强制「两端模式
//!   必须在构建期决定」，漏设一端无法编译；
//! * **两端顺序自由**：生产端与消费端**任意顺序**设置——可以从生产端开始
//!   （[`CircularBuffBuilder::pipe_from_input`] / [`CircularBuffBuilder::producer_passive`]）
//!   再从消费端收尾，也可以从消费端开始
//!   （[`CircularBuffBuilder::pipe_into_output`] / [`CircularBuffBuilder::consumer_passive`]）
//!   再从生产端收尾；还可以用 [`CircularBuffBuilder::pipe_between`] 一步同时设置
//!   两端，或两端都不设置、直接 [`CircularBuffBuilder::build_async`] 得到双端被动
//!   （经典手动管道）；
//! * **设备 move 进核心**：`pipe_from_input` / `pipe_into_output` 把设备以
//!   具体类型装入端类型，核心无需类型擦除；
//! * **异步构建**：`build` 已改为异步（[`ReadyBuilder::build_async`]）——主动端
//!   在真正产出构建成果前完成异步初始化（`init_async`，例如登记待唤醒结构）。
//!   `build_async` 在堆上分配核心与缓冲（`Shared<CircCore>`，分配器 `A` 默认
//!   [`CoreAlloc`]），产出 [`SpscPair`]——没有「缓冲聚合体」这一层，使用者只
//!   持有 [`Producer`] / [`Consumer`] 半部。
//!
//! # 用法一览（生产端/消费端可任意换序）
//!
//! **主动端不产出半部**——`build_async` 的返回类型由两端模式决定：双端被动 →
//! 一对半部（[`SpscPair`]）；主动生产 × 被动消费 → 仅消费端半部；被动生产 ×
//! 主动消费 → 仅生产端半部；主动 × 主动 → [`Pipeline`]（流水线 future：
//! 交给运行时 spawn 后持续由设备驱动流动，断开经
//! [`Pipeline::disconnect_handle`]）。
//!
//! ```ignore
//! // 双端被动（默认）：经典手动管道，两端都可访问——不 pipe 任何设备
//! let (mut tx, mut rx) = CircularBuffBuilder::with_capacity(4096)?
//!     .build_async().await?;
//!
//! // 显式双端被动（先设生产端，再设消费端）
//! let (mut tx, mut rx) = CircularBuffBuilder::with_capacity(4096)?
//!     .producer_passive()
//!     .consumer_passive()
//!     .build_async().await?;
//!
//! // 显式双端被动（先设消费端，再设生产端——顺序与上例对调）
//! let (mut tx, mut rx) = CircularBuffBuilder::with_capacity(4096)?
//!     .consumer_passive()
//!     .producer_passive()
//!     .build_async().await?;
//!
//! // 主动生产 × 被动消费：从 TrInput 自动灌入，用户自行读取
//! // （主动生产端不产出半部——只拿到消费端）
//! let mut ready = CircularBuffBuilder::with_capacity(4096)?
//!     .pipe_from_input(input)      // 生产端先行
//!     .consumer_passive();
//! let mut rx = ready.build_async().await?;
//!
//! // 被动生产 × 主动消费：用户自行写入，写后自动搬运到 TrOutput
//! // （主动消费端不产出半部——只拿到生产端）
//! let mut ready = CircularBuffBuilder::with_capacity(4096)?
//!     .pipe_into_output(output)    // 消费端先行
//!     .producer_passive();
//! let mut tx = ready.build_async().await?;
//!
//! // 主动 × 主动：TrInput → 缓冲 → TrOutput 流水线——`build_async` 返回
//! // [`Pipeline`]（Future）：交给运行时 spawn 后持续由设备驱动流动，断开经
//! // `pipeline.disconnect_handle().request()`
//! let mut ready = CircularBuffBuilder::with_capacity(4096)?
//!     .pipe_into_output(output)    // 消费端先行
//!     .pipe_from_input(input);     // 生产端后设
//! let pipeline = ready.build_async().await?;
//!
//! // 一步同时设置两端（等价于上例）
//! let mut ready = CircularBuffBuilder::with_capacity(4096)?
//!     .pipe_between(input, output);
//! let pipeline = ready.build_async().await?;
//! ```
//!
//! 缓冲容量在 `build_async` 时校验（`core_::MIN_CAPACITY` ..= `core_::MAX_CAPACITY`）。

use core::{
    borrow::BorrowMut,
    marker::PhantomData,
    mem::MaybeUninit,
};

use abs_buff::{
    gen_may_cancel_future,
    io::{TrInput, TrOutput},
    x_deps::abs_cancel,
};
use abs_cancel::{TrCancellationToken, TrMayCancel};
use mm_ptr::{
    Owned, Shared,
    x_deps::abs_mm::mem_alloc::{CoreAlloc, TrMalloc},
};

use super::{
    abs_comp_::{TrConsumer, TrProducer},
    circ_buff_::{BufConsumer, BufProducer, DevConsumer, DevProducer},
    core_::{self, CircCore},
    spsc_::{Consumer, CoreRef, Pipeline, Producer, SpscPair},
};

/// 构建错误：容量超出合法区间（`core_::MIN_CAPACITY` ..= `core_::MAX_CAPACITY`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuilderError<T> {
    /// 容量过小。
    SizeTooSmall(T),
    /// 容量过大。
    SizeTooBig(T),
    Cancelled,
    ProducerInit,
    ConsumerInit,
}

struct BuildEssential<P, C, B, T, A> {
    producer_: Option<P>,
    consumer_: Option<C>,
    buffer_: Option<B>,
    alloc_: Option<A>,
    _use_t_: PhantomData<fn() -> T>,
}

impl<P, C, B, T, A> BuildEssential<P, C, B, T, A> {
    pub const fn new(
        producer: P,
        consumer: C,
        buffer: B,
        alloc: A
    ) -> Self {
        BuildEssential {
            producer_: Option::Some(producer),
            consumer_: Option::Some(consumer),
            buffer_: Option::Some(buffer),
            alloc_: Option::Some(alloc),
            _use_t_: PhantomData,
        }
    }
}

/// 构建链起点：只定了容量（与分配器），两端模式均未决定。
///
/// 从这里可以：
///
/// * 先定**生产端**（[`Self::producer_passive`] / [`Self::pipe_from_input`]），
///   再进入 [`ProducerSetBuilder`] 定消费端；
/// * 先定**消费端**（[`Self::consumer_passive`] / [`Self::pipe_into_output`]），
///   再进入 [`ConsumerSetBuilder`] 定生产端；
/// * 一步同时定两端（[`Self::pipe_between`]）；
/// * 两端都不定，直接 [`Self::build`]——默认双端被动（经典手动管道）。
pub struct CircularBuffBuilder<B, T = u8, A = CoreAlloc>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    T: 'static,
    A: TrMalloc + Clone,
{
    buffer_: B,
    alloc_: A,
    _use_t_: PhantomData<fn() -> T>,
}

impl<B, T, A> CircularBuffBuilder<B, T, A>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    A: TrMalloc + Clone,
{
    pub fn try_with_buffer(
        buffer: B,
        alloc: A,
    ) -> Result<Self, BuilderError<usize>> {
        let size = buffer.borrow().len();
        match Self::try_capacity(size) {
            Result::Err(e) => Result::Err(e),
            Result::Ok(_) => Result::Ok(Self {
                buffer_: buffer,
                alloc_: alloc,
                _use_t_: PhantomData,
            })
        }
    }

    pub fn try_capacity(capacity: usize) -> Result<usize, BuilderError<usize>> {
        if capacity > core_::MAX_CAPACITY {
            return Result::Err(BuilderError::SizeTooBig(capacity));
        };
        if capacity < core_::MIN_CAPACITY {
            return Result::Err(BuilderError::SizeTooSmall(capacity))
        };
        Result::Ok(capacity)
    }
}

impl<T, A> CircularBuffBuilder<Owned<[MaybeUninit<T>], A>, T, A>
where
    A: TrMalloc + Clone + Default,
{
    /// 以默认分配器（[`CoreAlloc`]）构造，并对容量以 try_capacity 来检查，如果
    /// 容量不被支持，将返回错误。
    pub fn with_capacity(capacity: usize) -> Result<Self, BuilderError<usize>> {
        Self::with_allocator(capacity, A::default())
    }
}

impl<T, A> CircularBuffBuilder<Owned<[MaybeUninit<T>], A>, T, A>
where
    A: TrMalloc + Clone,
{
    /// 以自定义分配器构造。
    pub fn with_allocator(
        capacity: usize,
        alloc: A,
    ) -> Result<Self, BuilderError<usize>> {
        let cap = Self::try_capacity(capacity)?;
        let buff = Owned::new_uninit_slice(cap, alloc.clone());
        Result::Ok(CircularBuffBuilder {
            buffer_: buff,
            alloc_: alloc,
            _use_t_: PhantomData,
        })
    }

    /// 生产端为被动模式：调用者驱动写入。进入 [`ProducerSetBuilder`]，
    /// 下一步（`consumer_passive` / `pipe_into_output`）定消费端。
    #[allow(clippy::type_complexity)]
    pub fn producer_passive(
        self,
    ) -> ProducerSetBuilder<
            BufProducer<T>,
            Owned<[MaybeUninit<T>], A>,
            T, A,
        >
    {
        ProducerSetBuilder {
            producer: BufProducer::new(),
            buffer_: self.buffer_,
            alloc: self.alloc_,
            _use_t_: PhantomData,
        }
    }

    /// 生产端为主动模式：从输入设备 `input` 自动灌入缓冲（设备 move 进核心）。
    /// 进入 [`ProducerSetBuilder`]，下一步（`consumer_passive` /
    /// `pipe_into_output`）定消费端。
    #[allow(clippy::type_complexity)]
    pub fn pipe_from_input<I>(
        self,
        input: I,
    ) -> ProducerSetBuilder<
            DevProducer<I, T>,
            Owned<[MaybeUninit<T>], A>,
            T, A,
        >
    where
        I: TrInput<T>,
    {
        ProducerSetBuilder {
            producer: DevProducer::new(input),
            buffer_: self.buffer_,
            alloc: self.alloc_,
            _use_t_: PhantomData,
        }
    }

    /// 消费端为被动模式：调用者驱动读取。进入 [`ConsumerSetBuilder`]，
    /// 下一步（`producer_passive` / `pipe_from_input`）定生产端。
    #[allow(clippy::type_complexity)]
    pub fn consumer_passive(
        self,
    ) -> ConsumerSetBuilder<
            BufConsumer<T>,
            Owned<[MaybeUninit<T>], A>,
            T, A,
        >
    {
        ConsumerSetBuilder {
            consumer: BufConsumer::new(),
            buffer_: self.buffer_,
            alloc: self.alloc_,
            _use_t_: PhantomData,
        }
    }

    /// 消费端为主动模式：缓冲数据自动搬运到输出设备 `output`（设备 move 进
    /// 核心）。进入 [`ConsumerSetBuilder`]，下一步（`producer_passive` /
    /// `pipe_from_input`）定生产端。
    #[allow(clippy::type_complexity)]
    pub fn pipe_into_output<O>(
        self,
        output: O,
    ) -> ConsumerSetBuilder<
            DevConsumer<O, T>,
            Owned<[MaybeUninit<T>], A>,
            T, A,
        >
    where
        O: TrOutput<T>,
    {
        ConsumerSetBuilder {
            consumer: DevConsumer::new(output),
            buffer_: self.buffer_,
            alloc: self.alloc_,
            _use_t_: PhantomData,
        }
    }

    /// 一步同时决定两端：生产端为主动（`input` 自动灌入）、消费端为主动
    /// （自动搬运到 `output`）——`TrInput → 缓冲 → TrOutput` 同步流水线。
    ///
    /// 等价于 `pipe_from_input(input).pipe_into_output(output)` 或
    /// `pipe_into_output(output).pipe_from_input(input)`。直接进入
    /// [`ReadyBuilder`]，下一步即 `build_async`。
    #[allow(clippy::type_complexity)]
    pub fn pipe_between<I, O>(
        self,
        input: I,
        output: O,
    ) -> ReadyBuilder<
            DevProducer<I, T>,
            DevConsumer<O, T>,
            Owned<[MaybeUninit<T>], A>,
            T, A
        >
    where
        I: TrInput<T>,
        O: TrOutput<T>,
    {
        ReadyBuilder {
            essential_: Option::Some(BuildEssential::new(
                DevProducer::new(input),
                DevConsumer::new(output),
                self.buffer_,
                self.alloc_,
            ))
        }
    }

    /// 两端都不设置时的默认：双端被动（经典手动管道），直接产出半部对。
    ///
    /// 等价于 `producer_passive().consumer_passive().build_async()` 或
    /// `consumer_passive().producer_passive().build_async()`。
    #[allow(clippy::type_complexity)]
    pub async fn build_async(
        self,
    ) -> Result<
        SpscPair<Owned<[MaybeUninit<T>], A>, T, A>,
        BuilderError<()>,
    >
    where
        T: Send + Sync + 'static,
        A: Send + Sync + TrMalloc + Clone,
    {
        ReadyBuilder {
            essential_: Option::Some(BuildEssential::new(
                BufProducer::new(),
                BufConsumer::new(),
                self.buffer_,
                self.alloc_,
            ))
        }
        .build_async()
        .await
    }
}

/// 构建链中段：**生产端**模式已定，消费端模式尚未决定。
///
/// 由 [`CircularBuffBuilder::producer_passive`] / [`CircularBuffBuilder::pipe_from_input`]
/// 进入；下一步（`consumer_passive` / `pipe_into_output`）定消费端。
pub struct ProducerSetBuilder<P, B, T = u8, A = CoreAlloc>
where
    P: TrProducer<Data = T>,
    B: BorrowMut<[MaybeUninit<T>]>,
    T: 'static,
    A: TrMalloc + Clone,
{
    producer: P,
    buffer_: B,
    alloc: A,
    _use_t_: PhantomData<fn() -> T>,
}

impl<P, B, T, A> ProducerSetBuilder<P, B, T, A>
where
    P: TrProducer<Data = T>,
    B: BorrowMut<[MaybeUninit<T>]>,
    A: TrMalloc + Clone,
{
    /// 消费端为被动模式：调用者驱动读取。
    pub fn consumer_passive(self) -> ReadyBuilder<P, BufConsumer<T>, B, T, A> {
        ReadyBuilder {
            essential_: Option::Some(BuildEssential::new(
                self.producer,
                BufConsumer::new(),
                self.buffer_,
                self.alloc,
            ))
        }
    }

    /// 消费端为主动模式：缓冲数据自动搬运到输出设备 `output`。
    pub fn pipe_into_output<O>(
        self,
        output: O,
    ) -> ReadyBuilder<P, DevConsumer<O, T>, B, T, A>
    where
        O: TrOutput<T>,
    {
        ReadyBuilder {
            essential_: Option::Some(BuildEssential::new(
                self.producer,
                DevConsumer::new(output),
                self.buffer_,
                self.alloc,
            ))
        }
    }
}

/// 构建链中段：**消费端**模式已定，生产端模式尚未决定。
///
/// 与 [`ProducerSetBuilder`] 对称——由
/// [`CircularBuffBuilder::consumer_passive`] / [`CircularBuffBuilder::pipe_into_output`]
/// 进入（「先设消费端、再设生产端」的顺序）；下一步（`producer_passive` /
/// `pipe_from_input`）定生产端。
pub struct ConsumerSetBuilder<C, B, T = u8, A = CoreAlloc>
where
    C: TrConsumer<Data = T>,
    B: BorrowMut<[MaybeUninit<T>]>,
    T: 'static,
    A: TrMalloc + Clone,
{
    consumer: C,
    buffer_: B,
    alloc: A,
    _use_t_: PhantomData<fn() -> T>,
}

impl<C, B, T, A> ConsumerSetBuilder<C, B, T, A>
where
    C: TrConsumer<Data = T>,
    B: BorrowMut<[MaybeUninit<T>]>,
    A: TrMalloc + Clone,
{
    /// 生产端为被动模式：调用者驱动写入。
    pub fn producer_passive(self) -> ReadyBuilder<BufProducer<T>, C, B, T, A> {
        ReadyBuilder {
            essential_: Option::Some(BuildEssential::new(
                BufProducer::new(),
                self.consumer,
                self.buffer_,
                self.alloc,
            ))
        }
    }

    /// 生产端为主动模式：从输入设备 `input` 自动灌入缓冲（设备 move 进核心）。
    pub fn pipe_from_input<I>(
        self,
        input: I,
    ) -> ReadyBuilder<DevProducer<I, T>, C, B, T, A>
    where
        I: TrInput<T>,
    {
        ReadyBuilder {
            essential_: Option::Some(BuildEssential::new(
                DevProducer::new(input),
                self.consumer,
                self.buffer_,
                self.alloc,
            )),
        }
    }
}

/// 构建链终点：两端模式均已决定，可以 `build_async`。
pub struct ReadyBuilder<P, C, B, T = u8, A = CoreAlloc>
where
    P: TrProducer<Data = T>,
    C: TrConsumer<Data = T>,
    B: BorrowMut<[MaybeUninit<T>]>,
    A: TrMalloc + Clone,
{
    essential_: Option<BuildEssential<P, C, B, T, A>>,
}

impl<P, C, B, T, A> ReadyBuilder<P, C, B, T, A>
where
    P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    /// 装配核心并产出**按两端模式决定的构建产物**。
    ///
    /// 校验容量 → 分配缓冲并装入两端（`CircCore::new`）→ **await 两端各自的
    /// `init_async`**（主动端在构建期完成异步初始化，例如登记待唤醒结构；被动端
    /// 为 no-op）→ 移到堆上（[`Shared`]）→ 构建期初始驱动（主动端先泵一轮，
    /// 让数据开始流动）→ 按模式装配产物（**主动端不产出半部**，见
    /// [`BuildOutcome`]）。
    ///
    /// 返回类型由两端模式决定：被动 × 被动 → [`SpscPair`]（一对半部，调用者
    /// 自行读写）；主动生产 × 被动消费 → 仅消费端半部（读取即自动驱动输入泵
    /// 补位）；被动生产 × 主动消费 → 仅生产端半部（写入即自动驱动输出泵排空）；
    /// 主动 × 主动 → [`Pipeline`]（流水线 future：交给运行时 spawn 后持续
    /// 由两端设备驱动流动，直到一端出错 / 关闭或调用者请求断开）。
    pub fn build_async<'f>(&'f mut self) -> EssentialBuildAsync<'f, P, C, B, T, A>
    where
        (): BuildOutcome<P, C, B, T, A>,
    {
        let es = self.essential_.as_mut().expect("");
        EssentialBuildAsync(es)
    }
}

#[gen_may_cancel_future(EssentialBuild)]
async fn essential_build_async_<'f, P, C, B, T, A, K>(
    essential: &'f mut BuildEssential<P, C, B, T, A>,
    cancel: &'f mut K,
) -> Result<
        <() as BuildOutcome<P, C, B, T, A>>::Output,
        BuilderError<()>>
where
    P: Send + Sync + TrProducer<Data = T>,
    C: Send + Sync + TrConsumer<Data = T>,
    B: Send + Sync + BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
    (): BuildOutcome<P, C, B, T, A>,
    K: TrCancellationToken + Clone,
{
    let producer = essential.producer_.take().expect("");
    let consumer = essential.consumer_.take().expect("");
    let buffer = essential.buffer_.take().expect("");
    let alloc = essential.alloc_.take().expect("");
    let core = CircCore::new(
        producer,
        consumer,
        buffer,
    );
    // core 的初始化过程存在自引用结构
    unsafe {
        let init_producer = core
            .producer_ptr()
            .as_mut()
            .init_async(&core)
            .may_cancel_with(cancel)
            .await;
        if init_producer.is_err() {
            return Result::Err(BuilderError::ProducerInit);
        }
        let init_consumer = core
            .consumer_ptr()
            .as_mut()
            .init_async(&core)
            .may_cancel_with(cancel)
            .await;
        if init_consumer.is_err() {
            return Result::Err(BuilderError::ConsumerInit);
        }
    }
    let shared = Shared::new(core, alloc.clone());
    // 构建期初始搬运已在上面的 `init_async` 中完成（`DevProducer` 填满缓冲并
    // armed 等空位；`DevConsumer` armed 等数据）。
    Ok(<() as BuildOutcome<P, C, B, T, A>>::assemble(
        shared, alloc,
    ))
}

// ---------------------------------------------------------------------------
// 构建产物装配：按两端模式决定 `build` 的返回类型
// ---------------------------------------------------------------------------

/// 按两端模式装配 `build_async` 的产物（**内部机制**，调用者不应实现或直接
/// 使用）。
///
/// 设计初衷：**主动端不产出半部**——主动端由设备驱动，调用者不应持有任何
/// 可操作它的对象，因此 `build_async` 的返回类型随两端模式而定：
///
/// * 被动 × 被动 → [`SpscPair`]（一对半部）；
/// * 主动生产 × 被动消费 → 仅消费端半部；
/// * 被动生产 × 主动消费 → 仅生产端半部；
/// * 主动 × 主动 → [`Pipeline`]（流水线 future，由设备驱动）。
///
/// 类型状态链保证 `P` / `C` 只能是四个端类型（`BuffProducer` /
/// `BuffConsumer` / `DeviceProducer` / `DeviceConsumer`），下面的四个实现
/// 覆盖全部组合；密封（`sealed::Sealed`）保证调用者无法为其它类型实现。
#[doc(hidden)]
pub trait BuildOutcome<P, C, B, T, A>: sealed::Sealed
where
    P: TrProducer<Data = T>,
    C: TrConsumer<Data = T>,
    B: BorrowMut<[MaybeUninit<T>]>,
    A: TrMalloc + Clone,
{
    type Output;

    /// 装配构建产物。`alloc` 是构建器的分配器（`Pipeline` 需要它分配断开标志）。
    fn assemble(
        core_ref: CoreRef<P, C, B, T, A>,
        alloc: A,
    ) -> Self::Output;
}

mod sealed {
    /// 密封标记：仅 `()` 实现，外部无法自定义 `BuildOutcome`。
    pub trait Sealed {}
}

impl sealed::Sealed for () {}

impl<B, T, A> BuildOutcome<BufProducer<T>, BufConsumer<T>, B, T, A> for ()
where
    B: Send + Sync +BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    type Output = SpscPair<B, T, A>;

    fn assemble(
        core_ref: CoreRef<BufProducer<T>, BufConsumer<T>, B, T, A>,
        _alloc: A,
    ) -> SpscPair<B, T, A> {
        (Producer::new(core_ref.clone()), Consumer::new(core_ref))
    }
}

impl<I, B, T, A> BuildOutcome<DevProducer<I, T>, BufConsumer<T>, B, T, A> for ()
where
    I: Send + Sync + TrInput<T>,
    B: Send + Sync +BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    type Output = Consumer<DevProducer<I, T>, B, T, A>;

    fn assemble(
        core_ref: CoreRef<DevProducer<I, T>, BufConsumer<T>, B, T, A>,
        _alloc: A,
    ) -> Self::Output {
        Consumer::new(core_ref)
    }
}

impl<O, B, T, A> BuildOutcome<BufProducer<T>, DevConsumer<O, T>, B, T, A> for ()
where
    O: Send + Sync + TrOutput<T>,
    B: Send + Sync +BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    type Output = Producer<DevConsumer<O, T>, B, T, A>;

    fn assemble(
        core_ref: CoreRef<BufProducer<T>, DevConsumer<O, T>, B, T, A>,
        _alloc: A,
    ) -> Self::Output {
        Producer::new(core_ref)
    }
}

impl<I, O, B, T, A> BuildOutcome<DevProducer<I, T>, DevConsumer<O, T>, B, T, A>
    for ()
where
    I: Send + Sync + TrInput<T>,
    O: Send + Sync + TrOutput<T>,
    B: Send + Sync +BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync + 'static,
    A: Send + Sync + TrMalloc + Clone,
{
    type Output = Pipeline<I, O, B, T, A>;

    fn assemble(
        core_ref: CoreRef<DevProducer<I, T>, DevConsumer<O, T>, B, T, A>,
        _alloc: A,
    ) -> Self::Output {
        Pipeline::new(core_ref)
    }
}

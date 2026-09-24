//! 基准公共设施：负载参数、统一通道接口、两种驱动结构。
//!
//! # 负载对齐
//!
//! 负载参数与 asyncband 官方 benchmark（其仓库 `benchmarks/ecosystem/mpsc/support.rs`）
//! 对齐：每批 [`BATCH_MESSAGES`] 条、容量 [`BOUNDED_CAPACITY`] 格、载荷 `usize`。
//! 两侧实现跑的是**完全相同**的工作负载，差异只在通道实现本身。
//!
//! # 两种驱动结构
//!
//! * [`run_local_single_batch`] / [`run_local_multi_batch`]：单线程运行时 +
//!   [`LocalSet`](tokio::task::LocalSet)，生产者与消费者各是一个本地任务。
//!   这是 buffex 上唯一能表达「两个并发任务」的形状：buffex 的 future 不满足
//!   `Send`（见 `tests/async_runtime.rs` 记录的 rust#100013），无法
//!   `tokio::spawn`；`spawn_local` 不要求 `Send`，因此两种实现能用**同一形状**
//!   的驱动。
//! * [`ThreadBatch`]：写端各自一个 OS 线程、读端留在主线程；线程、运行时与通道
//!   跨样本复用，样本内只有消息往返。
//!
//! # 与 asyncband 官方基准的关系
//!
//! asyncband 官方基准用「多线程运行时 + `tokio::spawn`」驱动，而 buffex 的 future
//! 不可 `spawn`。为了做到「完全相同的工作负载」，这里**不**照抄它的驱动，而是
//! 让两侧都跑上面两种（`spawn_local` 与双 OS 线程）驱动——驱动一致，通道才是
//! 唯一的变量。

use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

/// 每批消息条数（对齐 asyncband 官方 benchmark）。
pub const BATCH_MESSAGES: usize = 16_384;

/// 有界通道容量（格数，对齐 asyncband 官方 benchmark）。
pub const BOUNDED_CAPACITY: usize = 64;

/// 生产者数量档位。
pub const PRODUCER_COUNTS: &[usize] = &[1, 8];

/// 一批消息序号之和：`0 + 1 + ... + (BATCH_MESSAGES - 1)`。
///
/// 用来在预热轮断言「不丢、不重、不串序」——把实现跑错的情况挡在计时之外。
pub const EXPECTED_CHECKSUM: usize = BATCH_MESSAGES * (BATCH_MESSAGES - 1) / 2;

/// 统一的「有界、单消费者」通道基准接口。
///
/// buffex 与 asyncband 的通道都经本接口被驱动，从而保证工作负载完全一致。
/// 方法名带 `_` 后缀：本仓库对非 `pub` 函数的命名纪律，trait 方法没有 `pub`
/// 修饰，故一并遵守。
#[allow(async_fn_in_trait)]
pub trait Chan: 'static {
    /// 写端半部。
    type Tx: Send + 'static;

    /// 读端半部。
    type Rx: Send + 'static;

    /// 建一对容量为 `capacity` 的通道。
    async fn make_(capacity: usize) -> (Self::Tx, Self::Rx);

    /// 同步就绪路径：尝试发一条；成功返回 `true`，队满返回 `false`。
    fn try_send_(tx: &mut Self::Tx, value: usize) -> bool;

    /// 同步就绪路径：尝试收一条；暂态无数据返回 `None`。
    fn try_recv_(rx: &mut Self::Rx) -> Option<usize>;

    /// 异步路径：发一条，队满则等待。
    async fn send_(tx: &mut Self::Tx, value: usize);

    /// 异步路径：收一条，队空则等待。
    async fn recv_(rx: &mut Self::Rx) -> usize;
}

/// 写端可复制的多生产者通道。
pub trait MultiChan: Chan {
    /// 复制一份写端（等价于新增一个写者）。
    fn clone_tx_(tx: &Self::Tx) -> Self::Tx;
}

/// 单线程 tokio 运行时：基准只测通道，不测调度器。
pub fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("单线程运行时应当构建成功")
}

/// 单生产者 × 单消费者跑一整批：两端各一个 `LocalSet` 本地任务。
///
/// 返回接收到的序号校验和（应等于 [`EXPECTED_CHECKSUM`]）。
pub async fn run_local_single_batch<C: Chan>() -> usize {
    let (tx, mut rx) = C::make_(BOUNDED_CAPACITY).await;
    let producer = tokio::task::spawn_local(async move {
        let mut tx = tx;
        for value in 0..BATCH_MESSAGES {
            C::send_(&mut tx, value).await;
        }
    });
    let consumer = tokio::task::spawn_local(async move {
        let mut checksum = 0usize;
        for _ in 0..BATCH_MESSAGES {
            checksum = checksum.wrapping_add(C::recv_(&mut rx).await);
        }
        checksum
    });
    let (produced, consumed) = tokio::join!(producer, consumer);
    produced.expect("生产者任务不应 panic");
    consumed.expect("消费者任务不应 panic")
}

/// `producers` 个生产者 × 单消费者跑一整批：全部是 `LocalSet` 本地任务。
///
/// 每个生产者负责 `BATCH_MESSAGES / producers` 条**互不重叠**的连续序号，
/// 因此校验和与单生产者形态相同。
pub async fn run_local_multi_batch<C: MultiChan>(producers: usize) -> usize {
    assert!(producers > 0, "生产者数量必须大于 0");
    assert_eq!(BATCH_MESSAGES % producers, 0, "生产者数量须整除批次条数");

    let (tx, mut rx) = C::make_(BOUNDED_CAPACITY).await;
    let per_producer = BATCH_MESSAGES / producers;

    // 原始写端归生产者 0，其余复制；同时存在的写者数恰好等于 producers。
    let mut senders = Vec::with_capacity(producers);
    senders.push(tx);
    for _ in 1..producers {
        let extra = C::clone_tx_(&senders[0]);
        senders.push(extra);
    }

    let mut producers_tasks = Vec::with_capacity(producers);
    for (index, tx) in senders.into_iter().enumerate() {
        producers_tasks.push(tokio::task::spawn_local(async move {
            let mut tx = tx;
            let first = index * per_producer;
            for offset in 0..per_producer {
                C::send_(&mut tx, first + offset).await;
            }
        }));
    }

    let consumer = tokio::task::spawn_local(async move {
        let mut checksum = 0usize;
        for _ in 0..BATCH_MESSAGES {
            checksum = checksum.wrapping_add(C::recv_(&mut rx).await);
        }
        checksum
    });

    for task in producers_tasks {
        task.await.expect("生产者任务不应 panic");
    }
    consumer.await.expect("消费者任务不应 panic")
}

/// 写端线程池 + 读端主线程的可复用批处理。
///
/// 线程、运行时与通道都跨样本复用：样本内只有消息往返，不含线程创建与首次分配。
/// 生产者线程在批与批之间阻塞在屏障上，因此不会提前写入下一批。
pub struct ThreadBatch<C: Chan> {
    /// 读端所在线程（主线程）的运行时。
    runtime_: tokio::runtime::Runtime,
    /// 读端半部，始终留在主线程。
    receiver_: C::Rx,
    /// 每批开始的会合点：写端线程 + 主线程。
    start_: Arc<Barrier>,
    /// 置位后写端线程退出。
    stop_: Arc<AtomicBool>,
    /// 写端线程句柄。
    workers_: Vec<JoinHandle<()>>,
}

impl<C: Chan> ThreadBatch<C> {
    /// 单生产者形态。
    pub fn new() -> Self {
        let runtime = current_thread_runtime();
        let (tx, receiver) = runtime.block_on(C::make_(BOUNDED_CAPACITY));
        let start = Arc::new(Barrier::new(2));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = spawn_producer_thread::<C>(tx, 0, BATCH_MESSAGES, start.clone(), stop.clone());
        Self {
            runtime_: runtime,
            receiver_: receiver,
            start_: start,
            stop_: stop,
            workers_: vec![worker],
        }
    }

    /// 跑一批：释放写端线程、收满 [`BATCH_MESSAGES`] 条，返回校验和。
    pub fn run(&mut self) -> usize {
        self.start_.wait();
        let receiver = &mut self.receiver_;
        self.runtime_.block_on(async {
            let mut checksum = 0usize;
            for _ in 0..BATCH_MESSAGES {
                checksum = checksum.wrapping_add(C::recv_(receiver).await);
            }
            checksum
        })
    }
}

impl<C: MultiChan> ThreadBatch<C> {
    /// 多生产者形态：每个生产者一个 OS 线程。
    pub fn with_producers(producers: usize) -> Self {
        assert!(producers > 0, "生产者数量必须大于 0");
        assert_eq!(BATCH_MESSAGES % producers, 0, "生产者数量须整除批次条数");

        let runtime = current_thread_runtime();
        let (tx, receiver) = runtime.block_on(C::make_(BOUNDED_CAPACITY));
        let start = Arc::new(Barrier::new(producers + 1));
        let stop = Arc::new(AtomicBool::new(false));
        let per_producer = BATCH_MESSAGES / producers;

        // 原始写端归生产者 0，其余复制；同时存在的写者数恰好等于 producers。
        let mut senders = Vec::with_capacity(producers);
        senders.push(tx);
        for _ in 1..producers {
            let extra = C::clone_tx_(&senders[0]);
            senders.push(extra);
        }

        let workers = senders
            .into_iter()
            .enumerate()
            .map(|(index, tx)| {
                spawn_producer_thread::<C>(
                    tx,
                    index * per_producer,
                    per_producer,
                    start.clone(),
                    stop.clone(),
                )
            })
            .collect();

        Self {
            runtime_: runtime,
            receiver_: receiver,
            start_: start,
            stop_: stop,
            workers_: workers,
        }
    }
}

impl<C: Chan> Default for ThreadBatch<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Chan> Drop for ThreadBatch<C> {
    fn drop(&mut self) {
        self.stop_.store(true, Ordering::Release);
        // 放行最后一批等待中的写端线程，它们会看到 stop 后退出。
        self.start_.wait();
        for worker in self.workers_.drain(..) {
            worker.join().expect("基准生产者线程不应 panic");
        }
    }
}

/// 起一个生产者 OS 线程：自带单线程运行时，批间阻塞在屏障上。
fn spawn_producer_thread<C: Chan>(
    mut tx: C::Tx,
    first: usize,
    count: usize,
    start: Arc<Barrier>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let runtime = current_thread_runtime();
        loop {
            start.wait();
            if stop.load(Ordering::Acquire) {
                break;
            }
            runtime.block_on(async {
                for offset in 0..count {
                    C::send_(&mut tx, first + offset).await;
                }
            });
        }
    })
}

//! 集成测试：在**两个真实后端运行时**（tokio / compio）下驱动 `buffex` 的异步语义。
//!
//! # 为什么这样写
//!
//! * 用例体写成**对运行时泛型**（`Rt: TrDelay`），再用两个具体后端各实例化一次——
//!   **同一个二进制、同一次 `cargo test`** 里两种后端都跑到；
//! * **不用** `abs_art-bridge`：它把「选后端」做成 Cargo feature 并强制唯一，而 Cargo
//!   的 feature 在同一次构建内取并集，所以「两种后端同时要」会直接编译失败（证据见
//!   `abs_art_backend_exp/evidence/11_bridge_conflict.log`）；
//! * 定时器一律用后端 `TrDelay`，**不自己写轮询循环**——后者的唤醒会把被测 future
//!   一遍遍重新轮询，从而把「没被唤醒」这类缺陷盖住。
//!
//! # 已知边界：`buffex` 的 future 目前无法 spawn（rust#100013）
//!
//! 「把读者放进独立任务 + 运行时定时器做超时」是**唯一**能诚实测出「`close()` 有没有
//! 唤醒已 park 的读者」的结构（同一任务里 `join!` + 让出循环必然是 pump，会把缺陷盖住）。
//! 但它在集成层编译不过，且**与 spawn 的机制无关**——诊断实测：
//!
//! | 被 spawn 的 future | 结果 |
//! | --- | --- |
//! | `async { 1u8 }` | ✅ |
//! | `async { tokio::time::sleep(..).await }` | ✅ |
//! | `async move { tx.send_async(1).await }`（`buffex` 的宏生成 future） | ❌ rust#100013 |
//! | 同上，改用原生 `tokio::spawn` / `compio::runtime::spawn` | ❌ 同一处 |
//!
//! 试过的其它构造同样失败：装箱成 `Pin<Box<dyn Future + Send + 'static>>`（装箱那一步就
//! 报）、泛型适配函数保留具体类型、把读者体写成具名 `async fn`、改用 `channels` 层具名
//! future（`SpscReceiver::next_async`）而非核心 `read_async`。
//!
//! 结论：**是 `gen_may_cancel_future` 产出的 future 形状无法满足 `Send + 'static`**，
//! 不是测试层的 spawn 写法问题（换成原生 spawn 也一样）。因此那条用例下沉到**单元测试
//! 层**：在 `#[cfg(test)]` 内给 `wakeslot` 装**计数 waker**，`close()` 之后断言「被
//! signal 过」——那里 waker 是**量具**（观测实现是否 signal），不是驱动被测 future 的
//! 驱动器。本文件只承载集成层能表达的部分。

use core::time::Duration;

use abs_art::TrDelay;
use abs_art_compio::Runtime as CompioRt;
use abs_art_tokio::Runtime as TokioRt;
use buffex::{
    channels::{ChannelError, SpscChannel},
    circular_buff::CoreAlloc,
};

/// 在真实后端运行时下跑一遍 SPSC 的异步收发与关闭（对运行时泛型）。
///
/// - 测试目标：`channels` 的异步写入口（`send_async` / `close_async`）在**真实运行时**
///   里可用，且关闭产生的**带内** `Closing` 信号按顺序出现在已提交载荷之后。
/// - 测试手段：用后端定时器 `TrDelay::delay` 做节奏（同时验证该能力在本后端可用），
///   期间 `send_async` 写 3 条、读端逐条取走，最后 `close_async`。
/// - 判定标准：读出的序列与写入一致；关闭后读端拿到 [`ChannelError::Closing`]。
async fn async_roundtrip_under_runtime_<Rt>()
where
    Rt: TrDelay,
{
    let (mut tx, mut rx) = SpscChannel::with_capacity(8)
        .expect("容量合法")
        .into_parts::<u8, CoreAlloc>()
        .await
        .expect("装配成功");

    for i in 0..3u8 {
        // 用后端定时器让出节奏：证明 `TrDelay` 在两个后端下都可用。
        <Rt as TrDelay>::delay(Duration::from_millis(1)).await;
        tx.send_async(i).await.expect("异步发送");
    }

    let mut got = Vec::new();
    while let Some(v) = rx.recv().expect("接收") {
        got.push(v);
    }
    assert_eq!(got, vec![0u8, 1, 2], "读出的序列应与写入一致");

    tx.close_async().await.expect("异步关闭");
    assert_eq!(
        rx.recv().expect_err("关闭并读空后应是 EOF"),
        ChannelError::Closing
    );
}

/// tokio 后端实例化。
#[tokio::test]
async fn async_roundtrip_tokio_() {
    async_roundtrip_under_runtime_::<TokioRt>().await;
}

/// compio 后端实例化。
#[compio::test]
async fn async_roundtrip_compio_() {
    async_roundtrip_under_runtime_::<CompioRt>().await;
}

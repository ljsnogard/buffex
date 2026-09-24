//! 真实 tokio UNIX domain socket 设备接入 `circular_buff` 的实测（socket 泵）。
//!
//! # 仅 tokio
//!
//! 本文件**仅**在 tokio 下运行（`#[tokio::test]`）：socket 设备直接包装
//! `tokio::net::UnixStream` 并依赖 tokio reactor 唤醒，无法在 compio 下跑
//! （除非给 compio 开 net feature，本仓库刻意不改 `Cargo.toml`）；因此这里的
//! 用例**不**走 `dual_runtime_test_` 的双运行时形态。有界超时用
//! `tokio::time::timeout`（同样仅 tokio）。
//!
//! [`pump_`](super::pump_) 里的设备（`TestInput` / `TestOutput`）是**永远就绪**的
//! 内存设备：`read_async` / `write_async` 立即返回 `Ready`，所以「提交路径上的单次
//! 非阻塞 poll」就能搬完。真实 socket 不同：无数据 / 发送缓冲满时返回 `Pending`，
//! 必须由一个被 executor 正常 `await` 的 future 接住，并由 reactor 唤醒。
//!
//! 本模块用一个测试内部的最小适配器（[`SockInput`] / [`SockOutput`]）把
//! `tokio::net::UnixStream` 包装成 `abs_buff` 的 `TrInput<u8>` / `TrOutput<u8>`
//! （形态对齐 `abs_buff_tokio_adapt` 的同名适配器），在真实 socket 上实测三种拓扑：
//!
//! * [`active_output_pump_stalls_on_partial_socket`]（`#[ignore]`）——被动生产 ×
//!   主动消费：提交路径只做单次非阻塞 poll，socket 返回 `Pending` 之后**没有可被
//!   调用方驱动的泵 future** 接住，逻辑消息的尾段永久滞留在环里（实测：首个提交的
//!   单次 poll 就返回 `Pending`，4 MiB 全部滞留，一个字节都没出去）；
//! * [`caller_driven_pump_over_socket_works`]——推荐用法：**全被动环 + 调用方 async
//!   泵**（泵 `await` 设备、再把结果提交进环），真实 socket 上全量往返成功；
//! * [`active_input_pump_stalls_on_pending_device`]（`#[ignore]`）——主动生产 ×
//!   被动消费：被动读端的 `read_async` 虽然会 `await` 主动生产泵，但该泵在
//!   `react_async` 搬入一批数据后**继续循环、再次 `await` 设备**；真实 socket
//!   第二次轮询返回 `Pending`，泵就此挂起，已经把数据搬进环的读者拿不到读段，
//!   直到设备关闭或把整段可写区填满。协议「等对端答复」因此互相等待。
//!
//! 设备被 `move` 进环核心后测试无法再直接访问，因此需要观察的一侧都留在测试进程里
//! （socket 对端、`Producer` / `Consumer` 半部）。

use core::{
    future::Future,
    mem::MaybeUninit,
    pin::Pin,
    task::{Context, Poll},
};
use std::{vec, vec::Vec};

use abs_buff::{
    Demand,
    io::{TrInput, TrOutput},
    x_deps::{
        abs_cancel::{TrCancellationToken, TrMayCancel},
        anylr::SomeOf,
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
    time::{Duration, timeout},
};

use super::{DefaultBuilder, TestErr, take_segm};

// ---------------------------------------------------------------------------
// 测试内部的最小 socket 设备（TrInput / TrOutput）
// ---------------------------------------------------------------------------

/// 测试内部的 socket 输入设备：**拥有**一条 `tokio::net::UnixStream` 半边，把它
/// 适配成 `TrInput<u8>`。
///
/// 与 `abs_buff_tokio_adapt::ReadAsInput`（借用 `&mut R`）不同，本设备**拥有**
/// socket：`buffex` 的主动端会把设备 `move` 进环核心，因此设备本身必须
/// `Send + Sync`（`tokio::net::UnixStream` 满足；compio 的 socket 半边因内部
/// `Rc` 而不满足，故无法走这条路线）。
struct SockInput {
    sock_: UnixStream,
}

impl TrInput<u8> for SockInput {
    type ReadAsync<'f> = SockRead<'f> where Self: 'f;
    type Err = TestErr;

    fn read_async<'f>(
        &'f mut self,
        target: &'f mut [MaybeUninit<u8>],
    ) -> Self::ReadAsync<'f> {
        SockRead { sock_: &mut self.sock_, target_: target }
    }
}

/// [`SockInput`] 的读 future：poll 真实 socket；无数据时返回 `Pending` **并注册
/// waker**（由 tokio reactor 在可读时唤醒），而不是伪造 `Ready(0)`。
struct SockRead<'f> {
    sock_: &'f mut UnixStream,
    target_: &'f mut [MaybeUninit<u8>],
}

impl Future for SockRead<'_> {
    type Output = SomeOf<usize, TestErr>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        // `ReadBuf::uninit` 直接以 `[MaybeUninit<u8>]` 构造可写视图，读端因此
        // **不需要**任何 `unsafe`：只有已填充前缀才通过 `filled()` 暴露。
        let mut readbuf = tokio::io::ReadBuf::uninit(this.target_);
        match Pin::new(&mut *this.sock_).poll_read(cx, &mut readbuf) {
            Poll::Ready(Ok(())) => {
                Poll::Ready(SomeOf::new_left(readbuf.filled().len()))
            }
            Poll::Ready(Err(_)) => Poll::Ready(SomeOf::new_right(TestErr::Boom)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'a> TrMayCancel<'a> for SockRead<'a> {
    type MayCancelFuture<'f, C> = SockRead<'a>
    where
        'f: 'a,
        Self: 'f,
        C: 'f + TrCancellationToken;

    type MayCancelOutput = SomeOf<usize, TestErr>;

    fn may_cancel_with<C>(self, _: C) -> Self::MayCancelFuture<'a, C>
    where
        C: 'a + TrCancellationToken,
    {
        self
    }
}

/// 测试内部的 socket 输出设备：与 [`SockInput`] 对称（`TrOutput<u8>`）。
struct SockOutput {
    sock_: UnixStream,
}

impl TrOutput<u8> for SockOutput {
    type WriteAsync<'f> = SockWrite<'f> where Self: 'f;
    type Err = TestErr;

    fn write_async<'f>(
        &'f mut self,
        source: &'f [MaybeUninit<u8>],
    ) -> Self::WriteAsync<'f> {
        SockWrite { sock_: &mut self.sock_, source_: source }
    }
}

/// [`SockOutput`] 的写 future：socket 发送缓冲满时返回 `Pending` 并注册 waker。
struct SockWrite<'f> {
    sock_: &'f mut UnixStream,
    source_: &'f [MaybeUninit<u8>],
}

impl Future for SockWrite<'_> {
    type Output = SomeOf<usize, TestErr>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        // SAFETY: 写侧的 `source_` 来自环的可读段，其字节均已初始化（`u8` 无非法
        // 位模式），`poll_write` 只读取该切片。写法与
        // `abs_buff_tokio_adapt::write_as_output` 一致。
        let buf: &[u8] = unsafe {
            core::slice::from_raw_parts(
                this.source_.as_ptr() as *const u8,
                this.source_.len(),
            )
        };
        match Pin::new(&mut *this.sock_).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => Poll::Ready(SomeOf::new_left(n)),
            Poll::Ready(Err(_)) => Poll::Ready(SomeOf::new_right(TestErr::Boom)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'a> TrMayCancel<'a> for SockWrite<'a> {
    type MayCancelFuture<'f, C> = SockWrite<'a>
    where
        'f: 'a,
        Self: 'f,
        C: 'f + TrCancellationToken;

    type MayCancelOutput = SomeOf<usize, TestErr>;

    fn may_cancel_with<C>(self, _: C) -> Self::MayCancelFuture<'a, C>
    where
        C: 'a + TrCancellationToken,
    {
        self
    }
}

// ---------------------------------------------------------------------------
// (i) 当前限制：被动生产 × 主动消费（混合模式）没有可驱动的泵
// ---------------------------------------------------------------------------

/// 测试目标：钉住当前限制——**被动生产 × 主动消费**在混合模式下没有可被调用方
/// `await` 的连续输出泵；提交路径 `advance_write` → `pump_output_sync` 只做单次
/// 非阻塞 poll，真实 socket 返回 `Pending` 后逻辑消息的**尾段**永久滞留环中。
///
/// 手段：构造一条真实 tokio UNIX domain socket；环以
/// `producer_passive().pipe_into_output(SockOutput)` 装配（混合模式，按
/// `BuildOutcome` 只产出生产端半部，调用者拿不到任何泵对象）；另起一个任务持续从
/// socket 对端读取。向环写入 4 MiB（远大于 socket 发送缓冲、且小于环容量，因此写入
/// 侧不会因环满而与泵交互），写完立即检查环内残留量与对端收到量。
///
/// 判定标准（修复后应全部成立；当前在断言 1 处立即失败）：
/// 1. `tx.data_size() == 0`——提交完成后环内不应再滞留数据；
/// 2. 对端在 5 秒内收到全部 4 MiB，且内容一致。
///
/// 实测记录：tokio 的 `UnixStream` 在 reactor 尚未被驱动时，首个 `poll_write` 就
/// 返回 `Pending`；由于该 pump 用 `Waker::noop()` 单次 poll，**4 MiB 全部留在环里**
/// （`tx.data_size() == 4194304`），一个字节都没出去——比「只丢尾段」更严重。
///
/// 本测试用有界超时 + 同步断言，因此修复前是**立刻失败**而不是挂起。若将来采纳的
/// 修复方案改为「调用方显式驱动新的混合泵 future」，本测试需要按该 API 改写后再
/// 去掉 `#[ignore]`（不能直接 un-ignore）。
#[tokio::test]
#[ignore = "修复前失败（不挂起）：混合模式没有可 await 的输出泵 future，advance_write 只做单次非阻塞 poll，socket 返回 Pending 时尾段永久滞留环中；断言 tx.data_size()==0 立即失败。修复后请按新 API 改写本测试再移除 ignore"]
async fn active_output_pump_stalls_on_partial_socket() {
    /// 环容量：大于载荷，保证写入侧不会因环满而触发主动泵。
    const K_CAP: usize = 8 * 1024 * 1024;
    /// 载荷：大于 Linux AF_UNIX 默认发送缓冲（约 208 KiB），保证单次 poll 填不满。
    const K_PAYLOAD: usize = 4 * 1024 * 1024;

    let (peer, sock) = UnixStream::pair().unwrap();
    // 对端持续读取，避免「无人读」这种纯缓冲区背压掩盖问题。
    let reader = tokio::spawn(async move {
        let mut peer = peer;
        let mut got = Vec::with_capacity(K_PAYLOAD);
        let mut buf = vec![0u8; 64 * 1024];
        while got.len() < K_PAYLOAD {
            match peer.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        got
    });

    let mut ready = DefaultBuilder::with_capacity(K_CAP)
        .unwrap()
        .producer_passive()
        .pipe_into_output(SockOutput { sock_: sock });
    let mut tx = ready.build_async().await.unwrap();

    let payload: Vec<u8> = (0..K_PAYLOAD).map(|i| (i % 251) as u8).collect();
    let mut off = 0usize;
    while off < payload.len() {
        let demand = Demand::at_least(1);
        let mut outcome = tx.write_async(&demand).await;
        let put;
        {
            let segm = outcome.as_mut().pick_left().expect("环写端应可用");
            put = segm.as_segm_mut().clone_items_from_buff(&payload[off..]);
        }
        assert!(put > 0, "写段应至少接受 1 字节");
        off += put;
        // `outcome` 在此 drop：提交写入 → advance_write → 单次非阻塞输出泵。
    }

    // 断言 1：提交完成后环内不应有滞留数据（修复前这里失败）。
    let stuck = tx.data_size();
    assert_eq!(
        stuck, 0,
        "被动生产 × 主动消费：写完 {K_PAYLOAD} 字节后环内仍有 {stuck} 字节滞留；\
         混合模式没有可被调用方 await 的输出泵接住 socket 的 Pending"
    );

    // 断言 2：对端最终应收到全部载荷（有界超时，防挂起）。
    let got = timeout(Duration::from_secs(5), reader)
        .await
        .expect("对端读取超时：尾段没有被驱动出环")
        .expect("读任务不应 panic");
    assert_eq!(got.len(), K_PAYLOAD, "对端应收到全部载荷");
    assert_eq!(got, payload, "载荷内容与顺序应一致");
}

// ---------------------------------------------------------------------------
// (ii) 推荐用法：全被动环 + 调用方驱动的 async 泵
// ---------------------------------------------------------------------------

/// 测试目标：验证推荐的 socket 用法——**全被动环 + 调用方 async 泵**在真实
/// socket 上可用（`smux_v1` 最终采用的也是这一形态）。
///
/// 手段：`tokio::join!` 在同一任务内并发推进三个 future：
/// * 入向泵：`SockInput::read_async`（`await` 真实 socket，无数据即 `Pending`）→
///   `ring_tx.write_async` → drop 段提交；
/// * 出向泵：`ring_rx.read_async` → 拷进本地缓冲 → `SockOutput::write_async`
///   （允许部分写，循环写完）→ **上网之后**才 drop 段提交（不丢数据）；
/// * 场景：向源 socket 分两批（中间 `sleep`，迫使泵经历设备 `Pending` 与 reactor
///   唤醒）写入 32 KiB，再从汇 socket 读回同样多的字节。
///
/// 判定标准：读回内容与写入逐字节一致；`timeout(5s)` 兜底，任何一处停滞都以超时
/// 失败而不是挂起。
#[tokio::test]
async fn caller_driven_pump_over_socket_works() {
    /// 环容量：小于载荷，迫使泵在环满时等待下游排空（覆盖背压分支）。
    const K_CAP: usize = 4096;

    let payload: Vec<u8> = (0..(K_CAP * 8)).map(|i| (i % 251) as u8).collect();

    // 源：场景写 `src_w`，入向泵读 `src_r`。
    let (mut src_w, src_r) = UnixStream::pair().unwrap();
    // 汇：出向泵写 `dst_w`，场景读 `dst_r`。
    let (dst_w, mut dst_r) = UnixStream::pair().unwrap();

    // 全被动环：两端都不接设备，搬运完全由调用方的泵负责。
    let ready = DefaultBuilder::with_capacity(K_CAP).unwrap();
    let (mut ring_tx, mut ring_rx) = ready.build_async().await.unwrap();

    let pump_in = async {
        let mut input = SockInput { sock_: src_r };
        let mut buf: Vec<MaybeUninit<u8>> =
            (0..K_CAP).map(|_| MaybeUninit::uninit()).collect();
        while let Some(n) = input.read_async(&mut buf).await.pick_left() {
            if n == 0 {
                break; // EOF：结束入向泵。
            }
            // SAFETY: 设备只把已初始化的前 `n` 字节写入 `buf`；`u8` 按位读取安全。
            let bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(buf.as_ptr() as *const u8, n)
            };
            let mut off = 0usize;
            while off < bytes.len() {
                let demand = Demand::at_least(1);
                let mut outcome = ring_tx.write_async(&demand).await;
                let put;
                {
                    let segm = outcome.as_mut().pick_left().expect("环写端应可用");
                    put = segm.as_segm_mut().clone_items_from_buff(&bytes[off..]);
                }
                assert!(put > 0, "写段应至少接受 1 字节");
                off += put;
                // `outcome` 在此 drop：提交写入 → advance_write → 唤醒读侧。
            }
        }
        ring_tx.close(); // 让出向泵读到环关闭。
    };

    let pump_out = async {
        let mut output = SockOutput { sock_: dst_w };
        loop {
            let demand = Demand::at_least(1);
            let mut outcome = ring_rx.read_async(&demand).await;
            {
                let segm = match outcome.as_mut().pick_left() {
                    Some(segm) => segm,
                    None => return, // 环关闭且已排空：结束出向泵。
                };
                let mut child = segm.as_segm_ref();
                let n = child.least_count();
                let mut dst: Vec<MaybeUninit<u8>> =
                    (0..n).map(|_| MaybeUninit::uninit()).collect();
                // SAFETY: `dst` 是本函数独占的可写切片；`move_items_to_buff` 只
                // 写入其中已初始化的前缀（返回长度给出实际搬移量）。
                let moved = unsafe { child.move_items_to_buff(&mut dst) };
                assert!(moved > 0, "读段应至少搬出 1 字节");
                // 先把 `dst[..moved]` 全部写上网，再 drop 子段提交消费。
                let mut off = 0usize;
                while off < moved {
                    match output.write_async(&dst[off..moved]).await.pick_left() {
                        Some(0) => panic!("socket 写设备不应返回 0"),
                        Some(k) => off += k,
                        None => panic!("socket 写设备报错"),
                    }
                }
                drop(child);
            }
            // `outcome` 在此 drop：提交消费 → advance_read → 唤醒写侧。
        }
    };

    let driver = async {
        let (head, tail) = payload.split_at(payload.len() / 2);
        src_w.write_all(head).await.unwrap();
        // 停顿一下：让入向泵先经历一次设备 `Pending`，再由 reactor 唤醒。
        tokio::time::sleep(Duration::from_millis(20)).await;
        src_w.write_all(tail).await.unwrap();
        drop(src_w); // EOF：入向泵据此关闭环。

        let mut got = Vec::with_capacity(payload.len());
        let mut buf = vec![0u8; 8192];
        while got.len() < payload.len() {
            match dst_r.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) => panic!("汇 socket 读失败：{e}"),
            }
        }
        got
    };

    let ((), (), got) = timeout(Duration::from_secs(5), async {
        tokio::join!(pump_in, pump_out, driver)
    })
    .await
    .expect("socket 泵在超时前未完成");

    assert_eq!(got, payload, "全被动环 + 调用方泵应完整往返载荷");
}

// ---------------------------------------------------------------------------
// (iii) 当前限制：主动生产 × 被动消费的泵在设备 Pending 时不交还控制权
// ---------------------------------------------------------------------------

/// 测试目标：钉住第二处混合模式限制——**主动生产 × 被动消费**（`pipe_from_input`）
/// 在真实 socket 上停滞。被动读端的 `read_async` 确实会 `await` 主动生产泵
/// （`core_.rs` 的 `core_passive_read_async_` 调用 `producer.pump_async(core)`），
/// 但该泵的循环（`circ_buff_.rs` 的 `dev_producer_pump_async_`）在
/// `react_async` 搬入一批数据后**不返回**，而是继续下一轮 `await` 设备；真实 socket
/// 第二次轮询返回 `Pending`，泵于是挂起——此时数据其实已经通过
/// `advance_write` 提交进环了，读者却因为拿不到泵返回而无法执行 `try_read_`。
///
/// 手段：环以 `pipe_from_input(SockInput).consumer_passive()` 装配；读侧先发起
/// `rx.read_async`（环空、对端尚未写入，读 future 先 `Pending` 在设备上），50 ms
/// 后对端写入 1 KiB；此后 1 KiB 会被泵搬进环，但泵继续等更多数据而不再返回。
///
/// 判定标准（修复后应成立；当前读侧在设备读到 1 KiB 之后仍然超时）：
/// 读侧在 2 秒内拿到全部 1 KiB 且内容一致。`timeout(2s)` 兜底，因此本测试是
/// **有界失败**而不是永久挂起。
#[tokio::test]
#[ignore = "修复前失败（2s 有界超时）：active 生产泵在搬入一批数据后继续 await 设备，真实 socket 的第二次 poll 返回 Pending，泵不交还控制权，已提交进环的数据无法被被动读端取走；修复后请移除本 ignore"]
async fn active_input_pump_stalls_on_pending_device() {
    /// 环容量：远大于本测试载荷（泵因此不会因「段满」而返回）。
    const K_CAP: usize = 64 * 1024;

    let payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();

    let (mut peer, sock) = UnixStream::pair().unwrap();
    let mut ready = DefaultBuilder::with_capacity(K_CAP)
        .unwrap()
        .pipe_from_input(SockInput { sock_: sock })
        .consumer_passive();
    let mut rx = ready.build_async().await.unwrap();

    let writer = async {
        // 先让读侧把 waker 注册到设备上（经历 `Pending`），再写入。
        tokio::time::sleep(Duration::from_millis(50)).await;
        peer.write_all(&payload).await.unwrap();
    };

    let reader = async {
        let mut got = Vec::with_capacity(payload.len());
        while got.len() < payload.len() {
            let demand = Demand::at_least(1);
            let mut outcome = timeout(Duration::from_secs(2), async {
                rx.read_async(&demand).await
            })
            .await
            .expect(
                "主动生产泵在搬入数据后继续 await 设备并挂起，\
                 已提交进环的数据取不出来",
            );
            let rs = outcome.as_mut().pick_left().expect("读端不应报错");
            let n = rs.least_count();
            got.extend(take_segm(rs, n));
            // `outcome` 在此 drop：提交读取 → advance_read → 驱动输入泵补位。
        }
        got
    };

    let ((), got) = tokio::join!(writer, reader);
    assert_eq!(got, payload, "主动生产 × 被动消费应拿到迟到的 socket 数据");
}

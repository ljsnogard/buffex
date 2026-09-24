//! `buffex::channels` 的性能基准：对标 asyncband 的同族有界通道。
//!
//! # 跑法
//!
//! ```text
//! cargo bench --bench channels                 # 全部
//! cargo bench --bench channels -- try_round    # 按名字筛选
//! cargo bench --bench channels -- --help       # divan 的全部选项
//! ```
//!
//! # 对标关系与结论怎么读
//!
//! | 基准 | buffex | 基线 |
//! | --- | --- | --- |
//! | `*_spsc_throughput` | `SpscChannel` | asyncband `mpsc::bounded` 单生产者用法 |
//! | `*_mpsc_throughput` | `MpscChannel` | asyncband `mpsc::bounded` |
//! | `try_round_trip` / `local_round_trip` | 两种通道 | asyncband `mpsc::bounded` |
//!
//! **SPSC 一行不是「SPSC vs SPSC」**：asyncband 没有 SPSC 实现（0.7.2 只有 mpsc，
//! main 新增的是单生产者多消费者的 `spmc`），因此只能取它 `mpsc::bounded` 的
//! 单生产者用法作为最接近的同族基线。解读该行比值时务必带上这个前提。
//!
//! # 负载完全一致
//!
//! 每批 16384 条、容量 64 格、载荷 `usize`，与 asyncband 官方 benchmark 的参数
//! 相同；两侧实现跑同一份驱动代码（见 `support`），通道实现是唯一变量。
//!
//! # 三类场景
//!
//! * `sync_`：同步就绪路径，测单次 `try_send` + `recv` 的开销；
//! * `local_`：单线程运行时 + `LocalSet` 本地任务，测异步握手与任务调度成本；
//! * `threads_`：写端线程 × 主线程读端，测含跨线程唤醒的真实吞吐。
//!
//! 在 `local_` 里用 `spawn_local` 而非 `tokio::spawn`，是因为 buffex 的 future
//! 不满足 `Send`（`tests/async_runtime.rs` 记录的 rust#100013）；为了公平，基线
//! 也用同一种驱动。

mod adapters;
mod local_;
mod support;
mod sync_;
mod threads_;

fn main() {
    divan::main();
}

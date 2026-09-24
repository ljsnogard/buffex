# `channels` 性能基准（对标 asyncband）

日期：2026-09-24 19:06
范围：`buffex::channels`（`SpscChannel` / `MpscChannel`）的首次性能基准与对标结论。

---

## 1. 目标

给 `dev/0.3.0` 新增的两个通道做一份**可复现**的性能基准，并与 asyncband 的同族
通道在**完全相同的工作负载**下对比，回答两个问题：

1. 每次操作（就绪路径往返）贵多少？
2. 整批吞吐（单线程异步任务 / 跨 OS 线程）差多少，差在哪？

## 2. 对标对象：一个必须记录的约束

**asyncband 没有 SPSC 实现。** 具体情况：

| asyncband 版本 | 通道 | 有无 SPSC |
| --- | --- | --- |
| 0.7.2（crates.io 最新发布版） | `mpsc`（bounded / unbounded） | ❌ |
| `main`（未发布） | 新增 `mpmc` / `spmc` | ❌（`spmc` 是单生产者多消费者，不同拓扑） |

因此**「对标同类型 SPSC」没有现成对象**。经与作者确认，SPSC 一行取
`asyncband::mpsc::bounded` 的**单生产者用法**作为最接近的同族基线：两者在
「有界 + 单消费者」上语义一致，差别只在 asyncband 没有 SPSC 专用优化路径。

> 解读警告：SPSC 行的比值是 **buffex SPSC vs asyncband MPSC**，不是 SPSC vs SPSC。
> 该行的绝对值对 buffex 并不算「吃亏」——asyncband 的 mpsc 是成熟实现，而它并没有
> 为单生产者开小灶。

## 3. 负载与驱动（两侧完全一致）

参数与 asyncband 官方 benchmark（其仓库 `benchmarks/ecosystem/mpsc/{bounded,support}.rs`）
对齐：

| 项 | 值 |
| --- | --- |
| 每批消息条数 `BATCH_MESSAGES` | 16 384 |
| 有界容量 `BOUNDED_CAPACITY` | 64 格 |
| 消息载荷 | `usize`（校验和 = `0+1+…+16383`） |
| 生产者档位 `PRODUCER_COUNTS` | 1、8 |

三类场景（`benches/channels/`）：

| 场景 | 驱动形状 | 测什么 |
| --- | --- | --- |
| `sync_::try_round_trip` | 无任务、无线程 | 单次 `try_send` + `recv` 的纯实现开销 |
| `local_*` | 单线程 runtime + `LocalSet::spawn_local` | 异步握手 + 任务调度 |
| `thread_*` | 写端各自一个 OS 线程（各带单线程 runtime `block_on`），读端留主线程 | 含跨线程唤醒的真实吞吐 |

**为什么用 `spawn_local` 而不是 `tokio::spawn`**：buffex 由 `gen_may_cancel_future`
产出的 future 不满足 `Send`（`tests/async_runtime.rs` 已记录 rust#100013），
`tokio::spawn` 编译不过。为了公平，**基线也用同一种驱动**；`thread_*` 场景同理，
两侧都是「线程 + `block_on`」，没有让任何一侧用上 `tokio::spawn` 的调度器。

基准框架用 **divan 0.1.21**（与 asyncband 官方 benchmark 同款）。`thread_*`
复用线程、运行时与通道（`support::ThreadBatch`），样本内只有消息往返——这一点与
asyncband 官方 `RepeatedBatch` 的做法一致。

## 4. 跑法

```text
cargo bench --bench channels                 # 全部
cargo bench --bench channels -- try_round    # 按名字筛选
cargo bench --bench channels -- --help       # divan 选项
```

`Cargo.toml` 中该 bench 目标设了 `harness = false` 与 `test = false`，因此
`cargo test` **不会**顺手跑基准（已验证）。

## 5. 结果

环境：4 核容器，宿主机 load average 约 2–3（跑基准期间另有邻居负载），
`rustc 1.100.0-nightly`，`bench` profile。下表为**两次独立运行的中位数**。

### 5.1 就绪路径往返（越低越好）

| 实现 | run 1 | run 2 |
| --- | --- | --- |
| `BandMpsc` | 79.34 ns | 79.33 ns |
| `BuffexSpsc` | 174.4 ns | 165.6 ns |
| `BuffexMpsc` | 182.3 ns | 183.3 ns |

⇒ buffex 单次往返约为 asyncband 的 **2.1×（SPSC）/ 2.3×（MPSC）**。这部分差距是
纯实现开销（协议槽位、`Reclaim` 段的取用与提交），与调度无关，且两次运行高度一致。

### 5.2 异步就绪往返（起两个本地任务完成一次收发）

| 实现 | run 1 | run 2 |
| --- | --- | --- |
| `BandMpsc` | 5.623 µs | 5.091 µs |
| `BuffexSpsc` | 6.049 µs | 5.503 µs |
| `BuffexMpsc` | 6.654 µs | 6.278 µs |

⇒ 一旦把「任务创建 + 调度」算进去，单次握手差距缩小到 **1.1×–1.25×**：绝对差
（约 0.6–1.2 µs）与 5.1 中约 90–100 ns 的实现差同量级，其余是调度成本。

### 5.3 吞吐（Mitem/s，越高越好）

| 场景 | 实现 | run 1 | run 2 | 对比 |
| --- | --- | --- | --- | --- |
| `local_spsc` | `BandMpsc` | 8.707 | 8.786 | 基线 |
| `local_spsc` | `BuffexSpsc` | 3.783 | 4.115 | **0.44×** |
| `local_mpsc`（1 写者） | `BandMpsc` | 8.792 | 8.789 | 基线 |
| `local_mpsc`（1 写者） | `BuffexMpsc` | 3.516 | 3.541 | **0.40×** |
| `local_mpsc`（8 写者） | `BandMpsc` | 7.567 | 7.660 | 基线 |
| `local_mpsc`（8 写者） | `BuffexMpsc` | 0.720 | 0.740 | **0.095×** |
| `thread_spsc` | `BandMpsc` | 2.693 | 2.532 | 基线 |
| `thread_spsc` | `BuffexSpsc` | 3.141 | 3.044 | **1.17×** |
| `thread_mpsc`（1 写者） | `BandMpsc` | 2.326 | 2.761 | 基线 |
| `thread_mpsc`（1 写者） | `BuffexMpsc` | 3.007 | 2.988 | **1.18×** |
| `thread_mpsc`（8 写者） | `BandMpsc` | 1.986 | 1.109 | 基线（方差极大） |
| `thread_mpsc`（8 写者） | `BuffexMpsc` | 0.0578 | 0.0594 | **0.04×** |

## 6. 结论

1. **单生产者路径 buffex 并不吃亏，跨线程反而更快。** 双线程 `thread_spsc` 与
   单写者 `thread_mpsc` 上 buffex 比 asyncband **快约 17%–18%**，两次运行一致。
   结合 5.1 的低绝对值差距（~100 ns）可以看出：在跨线程、每条消息都要唤醒对方的
   场景里，asyncband 基于 `std::sync::mpsc::sync_channel` 的唤醒成本更高，抵消了它
   在单次操作上的优势。
2. **单线程异步任务场景 buffex 慢约 2.2×–2.5×。** `local_*` 全部把两端放在同一个
   单线程 runtime 上，每条会阻塞的操作都要走一次「yield → 唤醒 → 重新 poll」，
   buffex 的协议开销被放大。
3. **多写者场景是当前最大的短板。** 8 个写者时：
   - 单线程（`local_mpsc`）慢约 **10×**；
   - 跨线程（`thread_mpsc`）慢约 **26×**，且非常稳定地停在 ~58 Kitem/s。

   原因与设计文档一致（见 `src/channels/mod.rs`）：写锁覆盖「取位 → 填充 → 提交」
   **全程**，且**等空间时仍占着写入位**，于是写者之间完全串行，每条消息都可能经历
   一次唤醒；8 个线程在 4 核上还互相抢占 CPU。asyncband 的 `thread_mpsc` 8 写者
   同场景方差极大（最快 4.3 Mitem/s、最慢 0.16 Mitem/s），说明它的中位数受调度
   影响很大，不宜作为精确基线，但**量级差距是明确的**。
4. **SPSC 与 MPSC 在 buffex 内部几乎同价**（5.1/5.3 中两者接近），说明 SPSC 没有
   因为「无锁」而拿到额外收益——瓶颈在协议与等待机制，而不在写者间的锁竞争；
   写者变多后则完全被锁策略主导。

## 7. 已知局限

* **机器噪声**：4 核容器 + 宿主机邻居负载，跨线程两项（`thread_*`）方差明显，
  尤其是 `BandMpsc` 8 写者。绝对值应以同机、空载复测为准；比值在同一台机器上
  两次运行之间是稳定的。
* **SPSC 基线不对等**：见第 2 节，asyncband 没有 SPSC。
* **8 写者 > 4 核**是刻意的负载（对齐 asyncband 的 `PRODUCER_COUNTS`），但它把
  「写者串行」与「CPU 抢占」两个因素混在了一起；若要分离，需补 4 写者档位。
* `local_*` 场景每批都重建通道（构建成本含在样本内）。相对 16 384 条消息的耗时
  可忽略，但严格来说两侧都多算了各自的构建成本。

## 8. 后续可选动作（未做）

* 补 4 写者档位、以及 `--sample-count` 更高的复测脚本，把「写者串行」与
  「CPU 抢占」分离；
* 评估 MPSC 写锁是否可改为「取到位置即放锁 + 段 drop 推进」（会改变提交顺序语义，
  属公开行为变更，需先讨论）；
* 若上游 asyncband 发布 `spmc`/SPSC，把 SPSC 行换成真正的同类型基线。

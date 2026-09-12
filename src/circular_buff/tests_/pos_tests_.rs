//! [`IoPos`](super::super::core_::IoPos)（`core_` 的环形位置状态）单元测试：
//! 验证**REVERSION 位置约定**。
//!
//! # 被测约定（REVERSION 方案，取代「空一槽」）
//!
//! 读写位置（`rp` / `wp`）以**物理索引**（`[0, capacity_)`）打包进 `atm_stat_`
//! 状态字低位，容量**全部可用**——不再像旧约定那样始终保留一个空槽。满 / 空
//! 由专门的 [`REVERSION`](super::super::core_::REVERSION) 标志区分：
//!
//! * **未跨末端**（`rv == false`）：原始 `wp >= rp`，数据量 = `wp - rp`；
//!   其中 `wp == rp` 表示**空**（data = 0）；
//! * **已跨末端**（`rv == true`）：写者位置越过了缓冲区物理末端（原始
//!   `wp <= rp`），数据量 = `wp + capacity - rp`；其中 `wp == rp` 表示**满**
//!   （整环都是数据，data = capacity）——这是旧「空一槽」约定无法表达的状态，
//!   也是本方案引入 REVERSION 的直接原因。
//!
//! 标志的维护规则（`IoPos::advance_wp` / `advance_rp`）：
//!
//! * `advance_wp(amount)`：`wp += amount`（物理上环绕）；**越过物理末端
//!   （`wp + amount >= capacity`）时置位 REVERSION**，且置位后保持到读者追上；
//! * `advance_rp(amount)`：`rp += amount`（物理上环绕）；**越过物理末端
//!   （`rp + amount >= capacity`）时清除 REVERSION**——读者跨过后，写者原始
//!   位置重新位于读者之前，恢复未跨状态。
//!
//! 两个推进函数的前置：`amount` 分别不超过 `free_size` / `data_size`（写不
//! 过头、读不空读）。它们返回**新的位置状态**（`IoPos`，不含任何标志位）。
//!
//! # pack 的「基座状态字」契约（本次重设计的核心）
//!
//! [`IoPos`] **不能**直接打包出 `atm_stat_` 所需的状态字——它不保存标志位。
//! 写回时以 **`pack(base)`** 完成：传入一个**保留了原来状态**的基座状态字
//! `base`，`pack` 用自身的新值**覆盖** `base` 中本类型「拥有」的位
//! （[`IoPos::MASK`] = 两个位置字段 + REVERSION），其余位（关闭、待机、待办泵
//! 等标志）由 `base` **原样保留**。因此 `pack` 是「**覆盖**」而非「或」——
//! 若按位或，基座中的旧位置会残留并与新位置混合。
//!
//! 典型用法（与 `core_` 的 `advance_write` / `advance_read` 一致）：
//!
//! ```text
//! let pos = IoPos::unpack(state_word, cap);
//! let new_word = pos.advance_wp(amount).pack(state_word);
//! ```
//!
//! # 测试的构造与判定方式（总述）
//!
//! * **构造**：用 [`state`] 辅助函数按位拼出原始状态字（`rp` 低 `POS_BITS`
//!   位、`wp` 次 `POS_BITS` 位、需要时置 REVERSION 位），再经
//!   [`IoPos::unpack`](super::super::core_::IoPos::unpack) 得到被测对象；推进
//!   直接调用 `advance_wp` / `advance_rp` 获得新位置状态；需要状态字时用
//!   `新状态.pack(基座)` 打包，必要时再 `unpack` 回读验证。
//! * **判定**：以「新位置状态的 `rp` / `wp` / `rv` 是否符合约定推导值」、
//!   「`data_size` / `free_size` 是否符合约定的空 / 满 / 部分量公式」以及
//!   「`pack(base)` 是否保留 `base` 的标志位、并用新值覆盖位置与 REVERSION
//!   （而非与旧位置相或）」为标志。

use super::super::core_::{
    FLAG_MASK, IoPos, POS_BITS, POS_MASK, REVERSION,
};

/// 测试用的环容量。
const CAP: usize = 8;

/// 按位构造一个原始状态字：`rp`（低 `POS_BITS` 位）+ `wp`（次 `POS_BITS` 位）
/// + 可选 REVERSION 位。其余标志位默认 0。
///
/// 测试用「先拼出状态字 → `unpack` 得到 [`IoPos`]」的方式构造被测目标，等价于
/// 核心从 `atm_stat_` 读到状态后解出位置的过程。
fn state(rp: usize, wp: usize, rv: bool) -> usize {
    let mut s = (rp & POS_MASK) | ((wp & POS_MASK) << POS_BITS);
    if rv {
        s |= REVERSION;
    }
    s
}

// ---------------------------------------------------------------------------
// unpack / pack：状态字的双向转换
// ---------------------------------------------------------------------------

/// # 被测约定
/// 状态字的位布局：`rp` 占低 `POS_BITS` 位、`wp` 占次 `POS_BITS` 位、
/// REVERSION 占 `usize::BITS - 5` 位；`unpack` 必须把这三者如实解出。
///
/// # 构造
/// 分别拼出「未跨」（`rp = 3, wp = 5, rv = false`）与「已跨」
/// （`rp = 5, wp = 3, rv = true`，即 `wp < rp`）两个原始状态字，再 `unpack`。
///
/// # 判定
/// 解出的 `rp` / `wp` / `rv` 字段与拼入值逐一相等——只要任一字段错位或
/// 被掩码截断，断言即失败。
#[test]
fn unpack_extracts_positions_and_reversion() {
    let s = state(3, 5, false);
    let pos = IoPos::unpack(s, CAP);
    assert_eq!((pos.rp, pos.wp, pos.rv), (3, 5, false));

    let s = state(5, 3, true);
    let pos = IoPos::unpack(s, CAP);
    assert_eq!((pos.rp, pos.wp, pos.rv), (5, 3, true));
}

/// # 被测约定
/// `pack` 的「基座状态字」契约（本次重设计的核心）：`pack(base)` 保留 `base`
/// 中本类型不拥有的位（[`IoPos::MASK`] 之外——其他标志位），用自身的新值
/// **覆盖**拥有的位（`rp` / `wp` 位置字段 + REVERSION）。判定要点是**覆盖而
/// 非或**：若按位或，`base` 中的旧位置会残留并与新位置混合（例如新 `rp = 0`
/// 与旧 `rp = 3` 相或得 3）。
///
/// # 构造
/// 构造一个挂着「旧位置 + 其他标志」的基座 `base`，再构造一个位置 / rv 与
/// `base` 不同的 [`IoPos`]（模拟「从基座解出旧位置 → 推进 → 以原基座打包」的
/// 真实流程），分别覆盖 `rv = false` 与 `rv = true` 两个方向。
///
/// # 判定
/// (1) 打包字中 `MASK` 之外的位与 `base` 完全一致（其他标志原样保留）；
/// (2) 打包字解出的 `rp` / `wp` / `rv` 为 `IoPos` 的新值（旧位置无残留、
/// REVERSION 被覆盖为新值）——(1)(2) 同时满足才算遵守「保留基座 + 覆盖」契约。
#[test]
fn pack_overwrites_owned_bits_and_preserves_base_flags() {
    let other = FLAG_MASK & !REVERSION;

    // rv=false 方向：IoPos 位置 (0,5)，基座位置 (3,0) 且挂着其他标志。
    let base = state(3, 0, false) | other;
    let pos = IoPos::unpack(state(0, 5, false), CAP);
    let word = pos.pack(base);
    assert_eq!(
        word & !IoPos::MASK,
        base & !IoPos::MASK,
        "MASK 之外的位（其他标志）必须与基座一致"
    );
    assert_eq!(
        (IoPos::unpack(word, CAP).rp, IoPos::unpack(word, CAP).wp),
        (0, 5),
        "位置必须被覆盖：rp 字段应为 0（基座旧 rp=3 不得残留，即不得按位或）"
    );
    assert_eq!(word & REVERSION, 0, "rv=false：REVERSION 位被覆盖为 0");

    // rv=true 方向：IoPos 置位 rv，基座无 REVERSION；位置亦不同。
    let base = state(6, 1, false) | other;
    let pos = IoPos::unpack(state(2, 4, true), CAP);
    let word = pos.pack(base);
    assert_eq!(word & !IoPos::MASK, base & !IoPos::MASK);
    assert_eq!(
        (IoPos::unpack(word, CAP).rp, IoPos::unpack(word, CAP).wp),
        (2, 4),
        "位置必须被覆盖（基座旧位置 (6,1) 不得残留）"
    );
    assert_ne!(word & REVERSION, 0, "rv=true：REVERSION 位被覆盖为 1");

    // 反向覆盖：IoPos rv=false 覆盖基座中已置位的 REVERSION。
    let base = state(1, 2, true) | other;
    let pos = IoPos::unpack(state(3, 6, false), CAP);
    let word = pos.pack(base);
    assert_eq!(word & REVERSION, 0, "rv=false 必须清除基座中的 REVERSION 位");
    assert_eq!(
        (IoPos::unpack(word, CAP).rp, IoPos::unpack(word, CAP).wp),
        (3, 6)
    );
    assert_eq!(word & other, other, "其他标志位仍由基座保留");
}

/// # 被测约定
/// `unpack` 与 `pack` 互为逆：对任意合法状态字 `s`，`unpack(s).pack(s) == s`
/// ——位置与 REVERSION 从 `s` 解出后原样覆盖回 `s`，其他标志位自然保留。
///
/// # 构造
/// 遍历空 / 满 / 未跨 / 已跨等典型位置组合逐对往返；并在其中一个状态字上
/// 叠加全部非 REVERSION 标志位（`FLAG_MASK & !REVERSION`），验证带标志的
/// 状态字也能完整往返。
///
/// # 判定
/// `unpack(s).pack(s) == s` 严格相等——任一位置位、REVERSION 位或保留标志位
/// 在往返中丢失 / 改变都会使断言失败。
#[test]
fn pack_roundtrips_the_full_state_word() {
    for (rp, wp, rv) in [
        (0, 0, false), // 空
        (0, 0, true),  // 满
        (2, 5, false), // 未跨
        (5, 2, true),  // 已跨未满
        (7, 0, true),  // 写者绕回开头
        (3, 3, true),  // 已跨且追上读者（满）
    ] {
        let s = state(rp, wp, rv);
        assert_eq!(IoPos::unpack(s, CAP).pack(s), s, "positions {rp},{wp}, rv={rv}");
    }

    // 带其余标志位的状态字也必须完整往返。
    let other_flags = FLAG_MASK & !REVERSION;
    let s = state(2, 5, false) | other_flags;
    assert_eq!(IoPos::unpack(s, CAP).pack(s), s);
}

// ---------------------------------------------------------------------------
// data_size / free_size：按约定计算数据量（不再空一槽）
// ---------------------------------------------------------------------------

/// # 被测约定
/// 未跨末端（`rv == false, wp > rp`）与已跨末端未满（`rv == true, wp < rp`）
/// 时，数据量分别为 `wp - rp` 与 `wp + capacity - rp`（即统一为
/// `(wp - rp) mod capacity`），可写空间为 `capacity - data`。
///
/// # 构造
/// 对同一容量 `CAP = 8` 拼出两个部分量状态：`(rp, wp) = (2, 5)` 未跨、
/// `(5, 2)` 已跨——两者数据量互为补数（3 与 5）。
///
/// # 判定
/// `data_size` / `free_size` 与约定公式的手算值相等，且 `data + free == CAP`
/// 恒成立。
#[test]
fn data_size_and_free_size_of_partial_states() {
    let pos = IoPos::unpack(state(2, 5, false), CAP);
    assert_eq!(pos.data_size(), 3, "未跨：data = wp - rp = 5 - 2");
    assert_eq!(pos.free_size(), 5, "未跨：free = 8 - 3");

    let pos = IoPos::unpack(state(5, 2, true), CAP);
    assert_eq!(pos.data_size(), 5, "已跨：data = wp + cap - rp = 2 + 8 - 5");
    assert_eq!(pos.free_size(), 3, "已跨：free = 8 - 5");
}

/// # 被测约定
/// 位置重合（`wp == rp`）且 `rv == false` 表示**空环**：data = 0、free = 容量。
/// 这是旧「空一槽」约定与 REVERSION 方案共有的空态。
///
/// # 构造
/// 拼出 `(rp, wp) = (3, 3)`、不置 REVERSION 的状态字并 `unpack`。
///
/// # 判定
/// `data_size() == 0`、`free_size() == CAP`——若空环被误判为有数据（例如把
/// `wp == rp` 一律按满环处理），断言即失败。
#[test]
fn data_size_empty_when_positions_coincide_without_reversion() {
    let pos = IoPos::unpack(state(3, 3, false), CAP);
    assert_eq!(pos.data_size(), 0, "rv=false 且 wp==rp：空环");
    assert_eq!(pos.free_size(), CAP);
}

/// # 被测约定
/// **本方案的核心约定**：位置重合（`wp == rp`）且 `rv == true` 表示**满环**——
/// 写者跨过末端后恰好追上读者，整环都是数据：data = capacity、free = 0。
/// 旧「空一槽」约定下该状态不合法（始终留一空槽，data 最大 capacity - 1）；
/// REVERSION 的存在正是为了把「空」与「满」两个 `wp == rp` 状态区分开。
///
/// # 构造
/// 拼出与空环测试**完全相同的位置对** `(rp, wp) = (3, 3)`，仅额外置上
/// REVERSION 位——同一位置对、仅翻转一个标志位，是区分空 / 满的判定关键。
///
/// # 判定
/// `data_size() == CAP`、`free_size() == 0`——若实现仍按朴素公式
/// `(wp - rp) mod capacity` 计算（得到 0），或丢弃 REVERSION 信息，断言即失败；
/// 这正是「不再空一槽」约定是否被遵守的直接标志。
#[test]
fn data_size_full_when_positions_coincide_with_reversion() {
    let pos = IoPos::unpack(state(3, 3, true), CAP);
    assert_eq!(pos.data_size(), CAP, "rv=true 且 wp==rp：满环（不再空一槽）");
    assert_eq!(pos.free_size(), 0);
}

// ---------------------------------------------------------------------------
// advance_wp：推进写者位置（越过物理末端 → 置位 REVERSION）
// ---------------------------------------------------------------------------

/// # 被测约定
/// 写者在环内推进（`wp + amount < capacity`，未越过物理末端）时，**不得**置位
/// REVERSION：`rv` 保持 false、`wp` 前进 `amount`。
///
/// # 构造
/// 从未跨状态 `(rp, wp) = (2, 3)` 出发（data = 1、free = 7），推进 2 格到
/// `wp = 5`，仍在物理末端（7）之内。
///
/// # 判定
/// 返回的新位置状态 `(rp, wp, rv) == (2, 5, false)` 且 data 由 1 增至 3——
/// 若实现误把未跨的推进也置了位，`rv` 断言即失败。
#[test]
fn advance_wp_stays_clear_within_the_ring() {
    let pos = IoPos::unpack(state(2, 3, false), CAP);
    let next = pos.advance_wp(2);
    assert_eq!((next.rp, next.wp, next.rv), (2, 5, false));
    assert_eq!(next.data_size(), 3);
}

/// # 被测约定
/// 写者**越过物理末端**（`wp + amount >= capacity`，新位置绕回开头）时，必须
/// **置位 REVERSION**：`wp` 环绕为 `(wp + amount) % capacity`，`rv` 变为 true。
///
/// # 构造
/// 从未跨状态 `(rp, wp) = (2, 6)` 出发（data = 4、free = 4），推进 3 格：
/// `6 + 3 = 9 >= 8`，新写者位置 `9 % 8 = 1`——写者越过末端 7、绕回开头。
///
/// # 判定
/// 返回的新位置状态 `(rp, wp, rv) == (2, 1, true)`，且 data 由 4 增至 7
/// （`1 + 8 - 2`）——`rv` 是否因「越过物理末端」而置位，是本测试的标志。
#[test]
fn advance_wp_sets_reversion_when_crossing_the_end() {
    let pos = IoPos::unpack(state(2, 6, false), CAP);
    let next = pos.advance_wp(3);
    assert_eq!((next.rp, next.wp, next.rv), (2, 1, true));
    assert_eq!(next.data_size(), 7);
}

/// # 被测约定
/// 「恰好越过」同样算越界：`wp + amount == capacity` 时新位置为 0（绕回开头），
/// 同样必须置位 REVERSION——物理末端的**边界本身**属于「越过」。
///
/// # 构造
/// 从未跨状态 `(rp, wp) = (1, 5)` 出发，推进 3 格：`5 + 3 == 8`，写者恰好
/// 停在末端、下一步即绕回 0。
///
/// # 判定
/// 返回的新位置状态 `(rp, wp, rv) == (1, 0, true)`——若实现用 `>` 而非 `>=`
/// 判越界，`rv` 将保持 false，断言即失败。
#[test]
fn advance_wp_exact_end_cross_sets_reversion() {
    let pos = IoPos::unpack(state(1, 5, false), CAP);
    let next = pos.advance_wp(3);
    assert_eq!((next.rp, next.wp, next.rv), (1, 0, true));
    assert_eq!(next.data_size(), 7);
}

/// # 被测约定
/// 推进恰好**写满**（`amount == free_size`）时进入满态：新 `wp == rp` 且
/// REVERSION 置位，`data_size == capacity`、`free_size == 0`——满环必须由
/// 「位置重合 + REVERSION」共同表达（见 `data_size_full_...`）。
///
/// # 构造
/// 从未跨状态 `(rp, wp) = (2, 6)`（data = 4、free = 4）出发，一次推进满
/// `free_size = 4` 格：`6 + 4 = 10`，新写者位置 `10 % 8 = 2 == rp`。
///
/// # 判定
/// 返回的新位置状态 `(rp, wp, rv) == (2, 2, true)`，且 `data_size == CAP`、
/// `free_size == 0`——若满态未被置位 / 被误判为空，data 或 rv 断言即失败。
#[test]
fn advance_wp_to_full_ends_at_coincident_positions_with_reversion() {
    let pos = IoPos::unpack(state(2, 6, false), CAP);
    let next = pos.advance_wp(4);
    assert_eq!((next.rp, next.wp, next.rv), (2, 2, true));
    assert_eq!(next.data_size(), CAP);
    assert_eq!(next.free_size(), 0);
}

/// # 被测约定
/// REVERSION 置位后（写者已跨末端），写者继续在环内推进（未再越界）时
/// **保持**置位——只有读者追上并跨过末端才会清除（见 `advance_rp_*`）。
///
/// # 构造
/// 从已跨状态 `(rp, wp) = (5, 2)`（rv = true，data = 5、free = 3）出发，
/// 推进 2 格：`2 + 2 = 4 < 8` 未越界，新写者位置 4——写者仍在读者之后。
///
/// # 判定
/// 返回的新位置状态 `(rp, wp, rv) == (5, 4, true)` 且 data 由 5 增至 7——
/// `rv` 是否在未越界的推进中保持置位，是本测试的标志。
#[test]
fn advance_wp_keeps_reversion_while_writer_stays_wrapped() {
    let pos = IoPos::unpack(state(5, 2, true), CAP);
    let next = pos.advance_wp(2);
    assert_eq!((next.rp, next.wp, next.rv), (5, 4, true));
    assert_eq!(next.data_size(), 7);
}

// ---------------------------------------------------------------------------
// advance_rp：推进读者位置（越过物理末端 → 清除 REVERSION）
// ---------------------------------------------------------------------------

/// # 被测约定
/// 读者在环内推进（`rp + amount < capacity`，未越过物理末端）时，REVERSION
/// **保持原值**：`rv` 为 true 则仍为 true（写者仍在读者之后），`rv` 为 false
/// 则仍为 false；`rp` 前进 `amount`。
///
/// # 构造
/// 两个场景：已跨状态 `(rp, wp) = (5, 2)`（rv = true）推进 1 格（rp → 6）；
/// 未跨状态 `(rp, wp) = (2, 5)`（rv = false）推进 2 格（rp → 4）。
///
/// # 判定
/// 两场景分别得到 `(6, 2, true)` 与 `(4, 5, false)`，data 相应减少——
/// `rv` 在未越界的读者推进中是否保持不变，是本测试的标志。
#[test]
fn advance_rp_keeps_reversion_without_crossing() {
    // 已跨（rv = true）：保持置位。
    let pos = IoPos::unpack(state(5, 2, true), CAP);
    let next = pos.advance_rp(1);
    assert_eq!((next.rp, next.wp, next.rv), (6, 2, true));
    assert_eq!(next.data_size(), 4, "5 - 1");

    // 未跨（rv = false）：保持清零。
    let pos = IoPos::unpack(state(2, 5, false), CAP);
    let next = pos.advance_rp(2);
    assert_eq!((next.rp, next.wp, next.rv), (4, 5, false));
    assert_eq!(next.data_size(), 1, "3 - 2");
}

/// # 被测约定
/// 读者**越过物理末端**（`rp + amount >= capacity`，新位置绕回开头）时，必须
/// **清除 REVERSION**：读者跨过后，写者的原始位置重新位于读者之前，恢复未跨
/// 状态（此后 `wp >= rp` 成立，`rv` 不再需要）。
///
/// # 构造
/// 从已跨状态 `(rp, wp) = (5, 2)`（rv = true，data = 5）出发，推进 4 格：
/// `5 + 4 = 9 >= 8`，新读者位置 `9 % 8 = 1`——读者越过末端 7、绕回开头，
/// 此时 `wp = 2 >= 1`。
///
/// # 判定
/// 返回的新位置状态 `(rp, wp, rv) == (1, 2, false)` 且 data 由 5 减至 1
/// （`2 - 1`）——`rv` 是否因「越过物理末端」而清除，是本测试的标志。
#[test]
fn advance_rp_clears_reversion_when_crossing_the_end() {
    let pos = IoPos::unpack(state(5, 2, true), CAP);
    let next = pos.advance_rp(4);
    assert_eq!((next.rp, next.wp, next.rv), (1, 2, false));
    assert_eq!(next.data_size(), 1);
}

/// # 被测约定
/// 推进恰好**读空**（`amount == data_size`）时回到空态：新 `rp == wp` 且
/// REVERSION 清除，`data_size == 0`、`free_size == capacity`——空环由
/// 「位置重合 + 无 REVERSION」表达（与满环互补）。
///
/// # 构造
/// 从已跨状态 `(rp, wp) = (5, 2)`（data = 5）出发，一次推进 `data_size = 5`
/// 格：`5 + 5 = 10`，新读者位置 `10 % 8 = 2 == wp`。
///
/// # 判定
/// 返回的新位置状态 `(rp, wp, rv) == (2, 2, false)`，且 `data_size == 0`、
/// `free_size == CAP`——若读空后 `rv` 未被清除（与满态混淆），断言即失败。
#[test]
fn advance_rp_to_empty_ends_at_coincident_positions_without_reversion() {
    let pos = IoPos::unpack(state(5, 2, true), CAP);
    let next = pos.advance_rp(5);
    assert_eq!((next.rp, next.wp, next.rv), (2, 2, false));
    assert_eq!(next.data_size(), 0);
    assert_eq!(next.free_size(), CAP);
}

// ---------------------------------------------------------------------------
// 推进 + pack：以原状态字为基座打包，验证标志保留与位置覆盖
// ---------------------------------------------------------------------------

/// # 被测约定
/// `advance_*` 只产生新的位置状态；写回 `atm_stat_` 必须经
/// `新状态.pack(原状态字)`：原状态字中的其余标志位（关闭、待机、待办泵等，
/// 掩码 `FLAG_MASK & !REVERSION`）**原样保留**，位置字段与 REVERSION 位被
/// 新值**覆盖**（旧位置不得残留）。
///
/// # 构造
/// 在原始状态字上叠加全部非 REVERSION 标志位 `other`，模拟核心中「位置提交时
/// 恰好挂着其他标志」（如对端正待机 / 泵有待办）的真实状态字；对带标志的状态
/// 分别执行 `advance_wp`（未跨、已跨各一例）与 `advance_rp`（清除 rv 一例），
/// 再以原状态字为基座 `pack`。
///
/// # 判定
/// 打包字中 `other` 位逐一保持（`word & other == other`）、`MASK` 之外的位与
/// 基座一致；同时解出的位置为新值、REVERSION 位按推进路径重算（未跨推进保持
/// 0、已跨推进保持 1、读者越界清除为 0）——「保留其余标志 + 覆盖位置与
/// REVERSION」两者同时满足才算遵守约定。
#[test]
fn advance_then_pack_preserves_unrelated_flags() {
    let other = FLAG_MASK & !REVERSION;
    assert_eq!(other & REVERSION, 0, "other 必须不含 REVERSION 位");

    // 未跨状态推进写者：rv 保持 0，other 保留。
    let s = state(2, 5, false) | other;
    let word = IoPos::unpack(s, CAP).advance_wp(2).pack(s);
    assert_eq!(word & !IoPos::MASK, s & !IoPos::MASK, "其余标志位原样保留");
    assert_eq!(word & REVERSION, 0, "未跨推进不得置位 REVERSION");
    assert_eq!((IoPos::unpack(word, CAP).rp, IoPos::unpack(word, CAP).wp), (2, 7));

    // 已跨状态推进写者：rv 保持 1，other 保留。
    let s = state(5, 2, true) | other;
    let word = IoPos::unpack(s, CAP).advance_wp(2).pack(s);
    assert_eq!(word & !IoPos::MASK, s & !IoPos::MASK, "其余标志位原样保留");
    assert_ne!(word & REVERSION, 0, "已跨推进保持 REVERSION 置位");
    assert_eq!((IoPos::unpack(word, CAP).rp, IoPos::unpack(word, CAP).wp), (5, 4));

    // 已跨状态推进读者并越过末端：rv 清除为 0，other 保留。
    let s = state(5, 2, true) | other;
    let word = IoPos::unpack(s, CAP).advance_rp(4).pack(s);
    assert_eq!(word & !IoPos::MASK, s & !IoPos::MASK, "其余标志位原样保留");
    assert_eq!(word & REVERSION, 0, "读者越界必须清除 REVERSION");
    assert_eq!((IoPos::unpack(word, CAP).rp, IoPos::unpack(word, CAP).wp), (1, 2));
}

// ---------------------------------------------------------------------------
// 完整循环：把 advance_* 当状态机驱动，验证量与约定自洽
// ---------------------------------------------------------------------------

/// # 被测约定
/// 把 [`IoPos`] 当作环形状态机驱动一轮「空 → 写满 → 读空」以及一段交错
/// 「写 3、读 2、写 5、读 6」的序列，任意时刻必须满足：
/// (1) `data_size + free_size == capacity`；
/// (2) 每次 `advance_wp(n)` 后 data 恰好 +n，每次 `advance_rp(n)` 后 data
/// 恰好 -n；
/// (3) 终态与初态一致（空环，`wp == rp` 且无 REVERSION）。
/// 这综合验证 REVERSION 置位 / 清除的边界（写满、读空、跨末端）与 `data_size`
/// 的满环公式相互自洽。
///
/// # 构造
/// 不构造真实缓冲，只把 `unpack → advance_* → pack → unpack` 串成状态迁移链
/// （pack 以当前状态字为基座，与原 `core_` 提交路径一致）：从空态 `(0, 0,
/// false)` 开始，依次执行上述推进序列，逐步记录位置与量。
///
/// # 判定
/// 每步断言位置 / `rv` / 数据量符合约定推导值，并断言
/// `data_size + free_size == CAP` 守恒；最后断言终态与初态完全相同。
#[test]
fn write_read_roundtrip_conserves_the_ring() {
    // —— 空 → 写满（advance_wp(8)，8 = 空环的 free）→ 读空（advance_rp(8)）——
    let mut pos = IoPos::unpack(state(0, 0, false), CAP);
    assert_eq!((pos.data_size(), pos.free_size()), (0, CAP));

    let word = pos.advance_wp(8).pack(state(0, 0, false));
    pos = IoPos::unpack(word, CAP);
    assert_eq!(
        (pos.rp, pos.wp, pos.rv, pos.data_size(), pos.free_size()),
        (0, 0, true, CAP, 0),
        "写满：wp==rp 且 rv=true，data=容量（不再空一槽）"
    );

    let word = pos.advance_rp(8).pack(word);
    pos = IoPos::unpack(word, CAP);
    assert_eq!(
        (pos.rp, pos.wp, pos.rv, pos.data_size(), pos.free_size()),
        (0, 0, false, 0, CAP),
        "读空：回到空态"
    );

    // —— 交错序列：写 3 → 读 2 → 写 5（跨末端）→ 读 6（跨末端）——
    let mut word = state(0, 0, false);
    pos = IoPos::unpack(word, CAP);

    word = pos.advance_wp(3).pack(word); // wp=3, data=3
    pos = IoPos::unpack(word, CAP);
    assert_eq!((pos.rp, pos.wp, pos.rv, pos.data_size()), (0, 3, false, 3));

    word = pos.advance_rp(2).pack(word); // rp=2, data=1
    pos = IoPos::unpack(word, CAP);
    assert_eq!((pos.rp, pos.wp, pos.rv, pos.data_size()), (2, 3, false, 1));

    word = pos.advance_wp(5).pack(word); // 3+5=8 越过末端 → wp=0, rv=true, data=6
    pos = IoPos::unpack(word, CAP);
    assert_eq!((pos.rp, pos.wp, pos.rv, pos.data_size()), (2, 0, true, 6));
    assert_eq!(pos.data_size() + pos.free_size(), CAP);

    word = pos.advance_rp(6).pack(word); // 2+6=8 越过末端 → rp=0, rv=false, data=0
    pos = IoPos::unpack(word, CAP);
    assert_eq!((pos.rp, pos.wp, pos.rv, pos.data_size()), (0, 0, false, 0));
    assert_eq!(pos.data_size() + pos.free_size(), CAP);
}

//! 重试退避：指数递增 + 可选抖动，用可播种的小型 PRNG 保证可复现。
//!
//! 为什么不用 `rand`：抖动只需要「同一请求可复现」的均匀取值，
//! splitmix64 十几行就够，还省掉一个依赖。种子从请求
//! （`RequestCtx::request_id`）确定性派生——同一请求的重试序列在任何
//! 机器上都一致，测试无需容忍随机误差。
//!
//! # 退避序列（与模块文档一致）
//!
//! 第 n 次重试（0 起）的 `基础退避 = initial × multiplier^n`，封顶 `max`；
//! `jitter: full` 在 `[0, 基础退避]` 均匀取值（打散重试风暴），
//! `jitter: none` 固定用基础退避（确定性，排障友好）。

use std::time::Duration;

/// 抖动策略（`backoff.jitter`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Jitter {
    /// 在 `[0, delay]` 均匀取值（默认，打散重试风暴）。
    Full,
    /// 固定 delay（确定性，测试与排障友好）。
    None,
}

/// 退避策略（`backoff` 子参数归一化后的形态，值不可变）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Backoff {
    /// 首次退避时长（必须 > 0）。
    pub(crate) initial: Duration,
    /// 退避上限。
    pub(crate) max: Duration,
    /// 每次递增倍率（>= 1.0）。
    pub(crate) multiplier: f64,
    /// 抖动策略。
    pub(crate) jitter: Jitter,
}

impl Backoff {
    /// 缺省：initial 50ms、max 2s、multiplier 2.0、jitter full（契约默认）。
    pub(crate) const DEFAULT: Backoff = Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_secs(2),
        multiplier: 2.0,
        jitter: Jitter::Full,
    };

    /// 第 `attempt` 次重试（0 起）的基础退避：`initial × multiplier^attempt`，封顶 `max`。
    pub(crate) fn base_delay(&self, attempt: u32) -> Duration {
        let factor = self.multiplier.powi(attempt as i32);
        let scaled = self.initial.as_nanos() as f64 * factor;
        let capped = scaled.min(self.max.as_nanos() as f64);
        Duration::from_nanos(capped.round() as u64)
    }

    /// 第 `attempt` 次重试的实际睡眠时长：基础退避经抖动映射（不超 `max`）。
    pub(crate) fn delay(&self, attempt: u32, rng: &mut SplitMix64) -> Duration {
        let base = self.base_delay(attempt);
        match self.jitter {
            Jitter::None => base,
            Jitter::Full => rng.uniform_within(base),
        }
    }
}

/// splitmix64：极小的可播种 PRNG（状态仅一个 `u64`）。
///
/// 只服务「抖动取值」一件事，**绝不**用于安全用途。
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// 以种子构造（同种子同序列）。
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// 下一个伪随机 u64（splitmix64 步进，混位常数取自原算法）。
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `[0, cap]` 内的均匀抖动值（纳秒粒度；`cap` 为 0 时恒返回 0）。
    pub(crate) fn uniform_within(&mut self, cap: Duration) -> Duration {
        let span = cap.as_nanos().min(u128::from(u64::MAX)) as u64;
        if span == 0 {
            return Duration::ZERO;
        }
        // 乘高 64 位映射：u64 均匀 → [0, span] 近似均匀（误差远小于 1ns 级抖动）。
        let draw = u128::from(self.next_u64());
        let scaled = (draw * (u128::from(span) + 1)) >> 64;
        Duration::from_nanos(scaled as u64)
    }
}

#[cfg(test)]
#[path = "backoff_tests.rs"]
mod tests;

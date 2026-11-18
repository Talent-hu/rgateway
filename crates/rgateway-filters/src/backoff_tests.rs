//! 退避计算与 PRNG 测试：递增/封顶表格、抖动取值区间、播种确定性。

use super::*;

/// 基础退避序列表（模块级常量使断言函数保持 <50 行）。
/// 每行 = (initial, max, multiplier, attempt, want)——覆盖递增、封顶与
/// 非整倍率（1.5）的精确取值。
const BASE_DELAY_CASES: &[(Duration, Duration, f64, u32, Duration)] = &[
    (
        Duration::from_millis(50),
        Duration::from_secs(2),
        2.0,
        0,
        Duration::from_millis(50),
    ),
    (
        Duration::from_millis(50),
        Duration::from_secs(2),
        2.0,
        1,
        Duration::from_millis(100),
    ),
    (
        Duration::from_millis(50),
        Duration::from_secs(2),
        2.0,
        2,
        Duration::from_millis(200),
    ),
    (
        Duration::from_millis(50),
        Duration::from_secs(2),
        2.0,
        5,
        Duration::from_millis(1600),
    ),
    // 封顶：第 6 次 3200ms、第 9 次 25.6s 都被压到上限 2s
    (
        Duration::from_millis(50),
        Duration::from_secs(2),
        2.0,
        6,
        Duration::from_secs(2),
    ),
    (
        Duration::from_millis(50),
        Duration::from_secs(2),
        2.0,
        9,
        Duration::from_secs(2),
    ),
    // 非整倍率递增：50 → 75 → 112.5ms
    (
        Duration::from_millis(50),
        Duration::from_secs(2),
        1.5,
        1,
        Duration::from_millis(75),
    ),
    (
        Duration::from_millis(50),
        Duration::from_secs(2),
        1.5,
        2,
        Duration::from_nanos(112_500_000),
    ),
    // 立即封顶（初始即超上限）
    (
        Duration::from_secs(3),
        Duration::from_secs(1),
        2.0,
        0,
        Duration::from_secs(1),
    ),
];

/// 基础退避序列表格：遍历 [`BASE_DELAY_CASES`] 精确断言。
#[test]
fn base_delay_sequence_table() {
    for (initial, max, multiplier, attempt, want) in BASE_DELAY_CASES {
        let backoff = Backoff {
            initial: *initial,
            max: *max,
            multiplier: *multiplier,
            jitter: Jitter::None,
        };
        assert_eq!(
            backoff.base_delay(*attempt),
            *want,
            "initial={initial:?} max={max:?} x{multiplier} attempt={attempt}"
        );
    }
}

/// jitter=none 的实际 delay 必须等于基础退避（确定性序列，测试可精确断言）。
#[test]
fn jitter_none_is_deterministic() {
    let backoff = Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_secs(2),
        multiplier: 2.0,
        jitter: Jitter::None,
    };
    let mut rng = SplitMix64::new(42);
    let got: Vec<Duration> = (0..4).map(|i| backoff.delay(i, &mut rng)).collect();
    assert_eq!(
        got,
        vec![
            Duration::from_millis(50),
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(400),
        ]
    );
}

/// jitter=full 的取值必须落在 [0, 基础退避] 内（均匀满幅抖动），且
/// **非退化**：采样中必须出现严格小于 base 的值、不同种子的序列必须不同
/// ——若实现悄悄退化成恒等（全等于 base）或恒零，这两条会立刻抓住。
#[test]
fn jitter_full_stays_within_bounds() {
    let backoff = Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_secs(2),
        multiplier: 2.0,
        jitter: Jitter::Full,
    };
    // 每行 = (种子, 第几次重试)；多种子多轮采样验证区间
    let cases: &[(u64, u32)] = &[
        (0, 0),
        (1, 0),
        (7, 1),
        (0xDEAD_BEEF, 2),
        (u64::MAX, 3),
        (12345, 6),
    ];
    let mut seed42_samples = Vec::new();
    for (seed, attempt) in cases {
        let mut rng = SplitMix64::new(*seed);
        let mut samples = Vec::new();
        for _ in 0..16 {
            let delay = backoff.delay(*attempt, &mut rng);
            let base = backoff.base_delay(*attempt);
            assert!(
                delay <= base,
                "seed={seed} attempt={attempt}: {delay:?} must be <= {base:?}"
            );
            samples.push(delay);
        }
        // 非退化：full 抖动必须把部分采样压到 base 之下（恒等映射会挂在这里）
        assert!(
            samples.iter().any(|d| *d < backoff.base_delay(*attempt)),
            "seed={seed} attempt={attempt}: full 抖动应产生 < base 的采样"
        );
        if *seed == 7 {
            seed42_samples = samples;
        }
    }
    // 异种子序列必须不同（恒零/常量映射会挂在这里）
    let mut other = SplitMix64::new(8);
    let other_samples: Vec<Duration> = (0..16).map(|_| backoff.delay(1, &mut other)).collect();
    assert_ne!(
        seed42_samples, other_samples,
        "不同种子的 full 抖动序列应不同"
    );
}

/// splitmix64 播种确定性：同种子同序列，异种子大概率不同序列。
#[test]
fn splitmix64_is_deterministic_per_seed() {
    let mut a = SplitMix64::new(7);
    let mut b = SplitMix64::new(7);
    let mut c = SplitMix64::new(8);
    let seq_a: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
    let seq_b: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
    let seq_c: Vec<u64> = (0..8).map(|_| c.next_u64()).collect();
    assert_eq!(seq_a, seq_b, "same seed must give the same sequence");
    assert_ne!(seq_a, seq_c, "different seeds must diverge");
    assert!(
        seq_a.iter().any(|v| *v != 0),
        "sequence must not be degenerate"
    );
}

/// zero 上限的抖动恒为 0（退避为 0 时不引入随机等待）。
#[test]
fn uniform_within_zero_cap_is_zero() {
    let mut rng = SplitMix64::new(1);
    for _ in 0..4 {
        assert_eq!(rng.uniform_within(Duration::ZERO), Duration::ZERO);
    }
}

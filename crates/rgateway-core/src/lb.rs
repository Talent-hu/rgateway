//! 负载均衡器：轮询（RoundRobin）、伪随机（RandomBalancer）与平滑加权
//! 轮询（SmoothWeightedRoundRobin，nginx SWRR）。
//!
//! 三种实现共用同一可用性过滤（见 `usable`）：只在 `healthy && weight > 0`
//! 的实例中选择，一个可用实例都没有时返回 `None`，由上层转成“无可用
//! 实例”错误。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::route::RequestCtx;

/// 注册到服务发现的一个后端实例。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// 稳定标识（默认取 `ip:port`）；作为熔断器等治理状态的 key。
    pub id: Arc<str>,
    /// 转发目标地址。
    pub addr: SocketAddr,
    /// 加权均衡器使用的相对权重；`0` 表示摘出流量（不参与选择）。
    pub weight: u32,
    /// 服务发现上报的健康状态；不健康的实例直接跳过。
    pub healthy: bool,
}

impl Instance {
    /// 以 `ip:port` 字符串作为 id、默认健康的新实例。
    pub fn new(addr: SocketAddr, weight: u32) -> Self {
        Self {
            id: Arc::from(addr.to_string().as_str()),
            addr,
            weight,
            healthy: true,
        }
    }
}

/// 为每次转发尝试挑选一个后端实例。
///
/// 实现通常带共享状态（计数器、权重表），会被并发请求同时调用，因此要求
/// `Send + Sync`；`pick` 只读取 `RequestCtx`、不做 I/O，也不应长时间持锁。
pub trait LoadBalancer: Send + Sync {
    /// 在健康且权重 > 0 的实例中挑一个；集合为空或全部不可用时返回 `None`。
    fn pick(&self, instances: &[Instance], ctx: &RequestCtx) -> Option<Instance>;

    /// 稳定名称，用于诊断与管理接口输出。
    fn name(&self) -> &'static str;
}

/// 过滤出可用实例（healthy 且 weight > 0），保持输入顺序。
fn usable(instances: &[Instance]) -> Vec<&Instance> {
    instances
        .iter()
        .filter(|i| i.healthy && i.weight > 0)
        .collect()
}

/// 经典轮询：共享原子计数器对可用列表取模。
///
/// 计数器用 `AtomicU64` + `Relaxed`（只需互斥递增，无需跨变量顺序保证），
/// 全程无锁；实例集变化后按新列表长度重新取模，不会偏向某个实例。
#[derive(Debug, Default)]
pub struct RoundRobin {
    counter: AtomicU64,
}

impl LoadBalancer for RoundRobin {
    fn pick(&self, instances: &[Instance], _ctx: &RequestCtx) -> Option<Instance> {
        let usable = usable(instances);
        if usable.is_empty() {
            return None;
        }
        let n = self.counter.fetch_add(1, Ordering::Relaxed) as usize;
        Some(usable[n % usable.len()].clone())
    }

    fn name(&self) -> &'static str {
        "RoundRobin"
    }
}

/// 由请求 id 与共享计数器共同播种的伪随机选择。
///
/// 为什么不引入 `rand` 依赖：负载均衡只要求散布均匀，不需要不可预测性；
/// splitmix64 混合器的统计质量对此绰绰有余，还能省掉一个依赖及 RNG 的
/// 播种/状态管理。种子混合了 request_id 与计数器，使同一请求的重试也
/// 倾向于落到不同实例。
#[derive(Debug, Default)]
pub struct RandomBalancer {
    counter: AtomicU64,
}

impl LoadBalancer for RandomBalancer {
    fn pick(&self, instances: &[Instance], ctx: &RequestCtx) -> Option<Instance> {
        let usable = usable(instances);
        if usable.is_empty() {
            return None;
        }
        // 先用计数器出基种子，再把 request_id 每个字节折进种子——相邻
        // 请求（计数器相邻）也会得到毫不相关的散列值，避免步进式偏移。
        let mut seed = splitmix64(self.counter.fetch_add(1, Ordering::Relaxed));
        for b in ctx.request_id.as_bytes() {
            seed = splitmix64(seed ^ u64::from(*b));
        }
        Some(usable[(seed % usable.len() as u64) as usize].clone())
    }

    fn name(&self) -> &'static str {
        "Random"
    }
}

/// splitmix64 —— 极小的整数混合函数（经典公开算法）。
///
/// 两轮「异或移位 + wrapping 乘法」就把低位影响扩散到全 64 位，输出对
/// 实例数取余后可近似均匀地落在下标上。
fn splitmix64(mut x: u64) -> u64 {
    // 黄金比例常数的 2^64 倍：保证相邻输入被推到相距很远的起点。
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// nginx 风格平滑加权轮询（Smooth Weighted Round-Robin, SWRR）：
/// 一个完整周期内各实例被选中的次数严格按权重比分配，同时选择被打散
/// ——高权重节点不会连续突发（对比普通加权轮询的成段突发）。
///
/// `current` 存每个实例的 current_weight。用 `Mutex<HashMap<..>>` 而非
/// 无锁结构，是因为一次 pick 需要「全体累加、选最大、命中减总和」三步
/// 在同一临界区内保持一致。
#[derive(Debug, Default)]
pub struct SmoothWeightedRoundRobin {
    current: Mutex<HashMap<Arc<str>, i64>>,
}

impl LoadBalancer for SmoothWeightedRoundRobin {
    fn pick(&self, instances: &[Instance], _ctx: &RequestCtx) -> Option<Instance> {
        let usable = usable(instances);
        if usable.is_empty() {
            return None;
        }
        let Ok(mut current) = self.current.lock() else {
            // 锁被毒化（上个持锁者 panic）：退回第一个可用实例——宁可牺牲
            // 均衡性也不让请求失败。
            return Some(usable[0].clone());
        };

        // —— nginx SWRR 单轮选择，三步 ——
        // 1) 每个可用实例 current_weight += 自身权重（effective weight）；
        let total: i64 = usable.iter().map(|i| i64::from(i.weight)).sum();
        let mut best: Option<(&Instance, i64)> = None;
        for inst in &usable {
            let entry = current.entry(inst.id.clone()).or_insert(0);
            *entry += i64::from(inst.weight);
            let score = *entry;
            // 2) 本轮选 current_weight 最大者（并列取先遍历到的）；
            if best.map(|(_, b)| score > b).unwrap_or(true) {
                best = Some((inst, score));
            }
        }
        let (chosen, score) = best?;
        // 3) 命中者 current_weight -= 总权重，把本轮“预支”的权重还回去。
        //    效果：权重 4:2:1 的序列是 a b a c a b a —— 次数精确、均匀穿插。
        current.insert(chosen.id.clone(), score - total);
        Some(chosen.clone())
    }

    fn name(&self) -> &'static str {
        "SmoothWeightedRoundRobin"
    }
}

impl SmoothWeightedRoundRobin {
    /// 清掉已下线实例的权重条目（如服务发现变更后调用），防止 map 膨胀
    /// 并避免旧实例的 current_weight 残留干扰新一轮选择。
    pub fn retain(&self, live: &[Instance]) {
        if let Ok(mut current) = self.current.lock() {
            let keep: std::collections::HashSet<&Arc<str>> = live.iter().map(|i| &i.id).collect();
            current.retain(|k, _| keep.contains(k));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::Protocol;
    use std::net::{IpAddr, Ipv4Addr};

    fn inst(port: u16, weight: u32) -> Instance {
        Instance::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            weight,
        )
    }

    fn ctx() -> RequestCtx {
        RequestCtx::new(Protocol::Http1, None)
    }

    fn addrs(picks: &[Instance]) -> Vec<u16> {
        picks.iter().map(|i| i.addr.port()).collect()
    }

    /// 验证：轮询严格按实例顺序循环选择（1,2,3,1,2,3）。
    #[test]
    fn round_robin_cycles() {
        let lb = RoundRobin::default();
        let instances = vec![inst(1, 100), inst(2, 100), inst(3, 100)];
        let picks: Vec<Instance> = (0..6)
            .map(|_| lb.pick(&instances, &ctx()).unwrap())
            .collect();
        assert_eq!(addrs(&picks), vec![1, 2, 3, 1, 2, 3]);
    }

    /// 验证：不健康或权重 0 的实例被跳过，只在可用实例间轮询。
    #[test]
    fn round_robin_skips_unusable() {
        let lb = RoundRobin::default();
        let instances = vec![
            inst(1, 100),
            Instance {
                healthy: false,
                ..inst(2, 100)
            },
            inst(3, 0),
            inst(4, 100),
        ];
        let picks: Vec<Instance> = (0..4)
            .map(|_| lb.pick(&instances, &ctx()).unwrap())
            .collect();
        assert_eq!(addrs(&picks), vec![1, 4, 1, 4]);
    }

    /// 验证：实例集为空或全部不可用时 pick 返回 None。
    #[test]
    fn empty_or_unusable_returns_none() {
        let lb = RoundRobin::default();
        assert!(lb.pick(&[], &ctx()).is_none());
        assert!(lb
            .pick(
                &[Instance {
                    healthy: false,
                    ..inst(1, 1)
                }],
                &ctx()
            )
            .is_none());
    }

    /// 验证：伪随机选择大致均匀覆盖所有实例（300 次抽样统计）。
    #[test]
    fn random_balancer_spreads() {
        let lb = RandomBalancer::default();
        let instances = vec![inst(1, 100), inst(2, 100), inst(3, 100)];
        let mut counts = HashMap::new();
        for _ in 0..300 {
            let p = lb.pick(&instances, &ctx()).unwrap();
            *counts.entry(p.addr.port()).or_insert(0u32) += 1;
        }
        // 三个实例都应被选中且频次大致均匀（容许统计波动）
        for port in [1u16, 2, 3] {
            let c = counts[&port];
            assert!(c > 50 && c < 150, "port {port} picked {c} times");
        }
    }

    /// 验证：权重 4:2:1 时命中序列与 nginx 平滑 WRR 完全一致（a b a c a b a）。
    ///
    /// 对应 nginx ngx_http_upstream 的 init_peer 语义：加有效权重、取最大、
    /// 减总权重；序列打散高权重节点，同时严格保持权重比。
    #[test]
    fn smooth_wrr_matches_nginx_sequence() {
        let lb = SmoothWeightedRoundRobin::default();
        let instances = vec![inst(1, 4), inst(2, 2), inst(3, 1)];
        let picks: Vec<u16> = (0..7)
            .map(|_| lb.pick(&instances, &ctx()).unwrap().addr.port())
            .collect();
        assert_eq!(picks, vec![1, 2, 1, 3, 1, 2, 1]);
    }

    /// 验证：权重 5:1:1 时计数精确且选择保持穿插（连选同一实例不超过 2 次）。
    ///
    /// 锁定“平滑”这一行为特征：命中次数严格等于权重比，同时高权重实例
    /// 不会形成长段突发。
    #[test]
    fn smooth_wrr_5_1_1_is_smooth() {
        let lb = SmoothWeightedRoundRobin::default();
        let instances = vec![inst(1, 5), inst(2, 1), inst(3, 1)];
        let picks: Vec<u16> = (0..7)
            .map(|_| lb.pick(&instances, &ctx()).unwrap().addr.port())
            .collect();
        assert_eq!(picks, vec![1, 1, 2, 1, 3, 1, 1]);
        let mut run = 1;
        for w in picks.windows(2) {
            if w[0] == w[1] {
                run += 1;
                assert!(run <= 2, "burst detected: {picks:?}");
            } else {
                run = 1;
            }
        }
    }

    /// 验证：一个周期（12 次）内命中次数严格等于权重比 3:2:1（6:4:2）。
    #[test]
    fn smooth_wrr_exact_ratio_over_cycle() {
        let lb = SmoothWeightedRoundRobin::default();
        let instances = vec![inst(1, 3), inst(2, 2), inst(3, 1)];
        let mut counts = HashMap::new();
        for _ in 0..12 {
            let p = lb.pick(&instances, &ctx()).unwrap();
            *counts.entry(p.addr.port()).or_insert(0u32) += 1;
        }
        assert_eq!(counts[&1], 6);
        assert_eq!(counts[&2], 4);
        assert_eq!(counts[&3], 2);
    }

    /// 验证：实例增减（retain 清理旧权重条目）后仍能正常工作且只返回存活实例。
    #[test]
    fn smooth_wrr_survives_instance_churn() {
        let lb = SmoothWeightedRoundRobin::default();
        let v1 = vec![inst(1, 1), inst(2, 1)];
        for _ in 0..3 {
            lb.pick(&v1, &ctx()).unwrap();
        }
        let v2 = vec![inst(1, 1), inst(3, 1)];
        lb.retain(&v2);
        // 必须保持可用，且只返回存活实例
        for _ in 0..4 {
            let p = lb.pick(&v2, &ctx()).unwrap();
            assert!(p.addr.port() == 1 || p.addr.port() == 3);
        }
    }
}

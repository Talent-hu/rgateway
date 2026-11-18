//! RateLimit 过滤器：governor GCRA 键控限流（槽位 `RateLimit`）。
//!
//! 参数：`key`（必填，`client_ip` 按对端 IP、`route` 按路由 id）、
//! `rate`（必填，每 `period` 的许可数，`1..=u32::MAX`）、`period`
//! （可选时长，缺省 1s，必须 > 0）。
//!
//! # 为什么桶表必须有界
//!
//! `key=client_ip` 的桶键来自客户端地址，攻击者伪造海量源 IP 即可放大
//! 桶表内存（每 IP 一个 GCRA 桶 ≈ 百字节级）。因此桶表带容量上限常量
//! [`MAX_RATE_LIMIT_BUCKETS`]，写满时按 **LRU 批量淘汰**：淘汰最久未
//! 用的 [`EVICTION_BATCH_PERCENT`]%（至少 1 个）。热桶（高频 IP）靠
//! 最近访问序号保命；洪水式新 IP 只会反复淘汰冷桶，表长恒有上界。
//! 被淘汰的桶等于从未限流过——攻击者换 IP 本就绕过限流，淘汰只牺牲
//! 冷 IP 的额度连续性，不影响安全边界。
//!
//! # 为什么经 preserved 槽保留
//!
//! 限流语义必须跨配置热重载连续（README 承诺「限流桶跨重载存活」），
//! 故桶表挂在 [`RouteState::preserved`] 的 [`SLOT`] 槽上，reload 后同
//! 名路由复用同一张表。槽内按「维度 + 速率 + 周期」签名分子表：同一
//! 路由上的多个 RateLimit 过滤器配置互不抢桶，签名相同的重载复用原桶。
//!
//! governor 0.8 API 形状已对照本地 registry 源码（governor-0.8.1）验证：
//! `Quota::with_period(...)?.allow_burst(...)`、`RateLimiter::direct_with_clock`、
//! `check() -> Result<(), NotUntil<_>>`。

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use governor::clock::{Clock, QuantaClock};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use rgateway_config::{ConfigError, FilterBuildCtx, FilterFactory, FilterSpec, ServiceWrapper};
use rgateway_core::{BoxedService, GatewayError, ProxyBody};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

use crate::params::{positive_duration, positive_u32, value_error};
use crate::util::ctx_of;

/// 桶表容量上限（每路由每过滤器配置一张表）。
pub const MAX_RATE_LIMIT_BUCKETS: usize = 4096;
/// 单次淘汰批次占比：最久未用的 10%（至少 1 个）。
const EVICTION_BATCH_PERCENT: usize = 10;
/// `period` 缺省值：1 秒。
const DEFAULT_PERIOD: Duration = Duration::from_secs(1);
/// [`RouteState::preserved`](rgateway_core::RouteState::preserved) 槽名。
const SLOT: &str = "rate_limit";

/// 单桶（单个限流键）的 GCRA 限流器类型别名。
type CellLimiter<C> =
    RateLimiter<NotKeyed, InMemoryState, C, NoOpMiddleware<<C as Clock>::Instant>>;

/// 限流维度：桶键的第二段（第一段是路由，经 preserved 槽按路由分表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dimension {
    /// 按客户端 IP（`ctx.peer_addr` 的 IP 部分；缺失归 `"unknown"`）。
    ClientIp,
    /// 按路由 id（整条路由共用一个桶）。
    Route,
}

impl Dimension {
    /// 解析 `key` 参数值；只接受 `"client_ip"` / `"route"`。
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "client_ip" => Ok(Self::ClientIp),
            "route" => Ok(Self::Route),
            other => Err(format!(
                "unsupported dimension {other:?} (expected \"client_ip\" or \"route\")"
            )),
        }
    }

    /// 稳定标签（桶签名与错误消息用）。
    fn label(self) -> &'static str {
        match self {
            Self::ClientIp => "client_ip",
            Self::Route => "route",
        }
    }
}

/// 归一化后的限流参数。
struct Params {
    dimension: Dimension,
    /// 每 `period` 的许可数（已保证可放入 `NonZeroU32`）。
    rate: u32,
    period: Duration,
    quota: Quota,
}

/// 由「每 period rate 个许可」构造 GCRA 配额：补给间隔 = period/rate，
/// 突发容量 = rate。放不下（截断为 0）时返回 `None`。
fn make_quota(rate: u32, period: Duration) -> Option<Quota> {
    let burst = NonZeroU32::new(rate)?;
    let per_cell = period.checked_div(rate)?;
    Some(Quota::with_period(per_cell)?.allow_burst(burst))
}

/// 单个限流桶：GCRA 状态 + 最近访问序号（LRU 淘汰依据）。
struct BucketEntry<C: Clock + Clone> {
    limiter: CellLimiter<C>,
    last_used: AtomicU64,
}

/// 有界的键 → 桶表（LRU 批量淘汰，见模块文档）。
///
/// 泛型时钟：生产用 [`QuantaClock`]，测试用 governor 的 `FakeRelativeClock`
/// 精确推进补给节奏（`advance` 与其克隆共享同一时间轴）。
pub(crate) struct BoundedBuckets<C: Clock + Clone> {
    /// 容量上限：绝不超过 [`MAX_RATE_LIMIT_BUCKETS`]（或测试自定义值）。
    cap: usize,
    quota: Quota,
    clock: C,
    /// 全局访问序号（单调递增），entry 的 `last_used` 取此刻的序号。
    seq: AtomicU64,
    map: DashMap<String, Arc<BucketEntry<C>>>,
}

impl<C: Clock + Clone + Send + Sync + 'static> BoundedBuckets<C> {
    /// 以容量上限、配额与时钟构建空表。
    pub(crate) fn with_quota(cap: usize, quota: Quota, clock: C) -> Self {
        Self {
            cap,
            quota,
            clock,
            seq: AtomicU64::new(0),
            map: DashMap::new(),
        }
    }

    /// 消耗一个许可；超出配额返回 `false`。
    pub(crate) fn check(&self, key: &str) -> bool {
        let entry = self.entry_for(key);
        entry
            .last_used
            .store(self.seq.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        entry.limiter.check().is_ok()
    }

    /// 当前桶数（有界性断言用）。
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// 取（或创建）键对应的桶；表满时先 LRU 淘汰再插入，插入后二次校正。
    ///
    /// 并发插入下「查了没有 → 淘汰 → 插入」是竞态的：多个调用方可能同时
    /// 穿过淘汰检查，瞬时把表推到 `cap` 之上。插入完成后立即循环校正
    /// （`while len() > cap` 再淘汰），使有界性**收敛后严格成立**；瞬时窗口
    /// 内可略超 `cap`，但绝不会无界增长（每次插入都自带校正）。
    fn entry_for(&self, key: &str) -> Arc<BucketEntry<C>> {
        if let Some(existing) = self.map.get(key) {
            return Arc::clone(existing.value());
        }
        self.evict_if_full();
        let entry = Arc::clone(
            self.map
                .entry(key.to_string())
                .or_insert_with(|| {
                    Arc::new(BucketEntry {
                        limiter: RateLimiter::direct_with_clock(self.quota, self.clock.clone()),
                        last_used: AtomicU64::new(self.seq.fetch_add(1, Ordering::Relaxed)),
                    })
                })
                .value(),
        );
        // 竞态校正：并发插入可能已瞬时突破 cap，插完立刻收敛回有界。
        while self.len() > self.cap {
            self.evict_if_full();
        }
        entry
    }

    /// 表满时淘汰最久未用的一批（[`EVICTION_BATCH_PERCENT`]%，至少 1 个）。
    fn evict_if_full(&self) {
        if self.len() < self.cap {
            return;
        }
        let mut by_age: Vec<(u64, String)> = self
            .map
            .iter()
            .map(|e| (e.value().last_used.load(Ordering::Relaxed), e.key().clone()))
            .collect();
        by_age.sort_unstable();
        let batch = std::cmp::max(1, self.cap * EVICTION_BATCH_PERCENT / 100);
        for (_, key) in by_age.into_iter().take(batch) {
            self.map.remove(&key);
        }
    }
}

/// preserved 槽的持有者：按「维度:速率:周期」签名分桶表。
///
/// 同签名的配置热重载复用同一张表（桶存活）；不同签名（改了速率/维度）
/// 各占一张表，互不抢额度。签名种类数受配置变更次数约束，量级极小。
#[derive(Default)]
struct RateLimitSlot {
    tables: DashMap<String, Arc<BoundedBuckets<QuantaClock>>>,
}

impl RateLimitSlot {
    /// 取（或创建）签名对应的桶表。
    fn table_for(&self, signature: &str, quota: Quota) -> Arc<BoundedBuckets<QuantaClock>> {
        Arc::clone(
            self.tables
                .entry(signature.to_string())
                .or_insert_with(|| {
                    Arc::new(BoundedBuckets::with_quota(
                        MAX_RATE_LIMIT_BUCKETS,
                        quota,
                        QuantaClock::default(),
                    ))
                })
                .value(),
        )
    }
}

/// RateLimit 过滤器工厂（canonical 名 `"RateLimit"`，槽 [`rgateway_config::FilterSlot::RateLimit`]）。
pub struct RateLimitFactory;

impl FilterFactory for RateLimitFactory {
    fn name(&self) -> &'static str {
        "RateLimit"
    }

    fn build(
        &self,
        spec: &FilterSpec,
        ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        let params = parse_params(spec)?;
        let signature = format!(
            "{}:{}:{}",
            params.dimension.label(),
            params.rate,
            params.period.as_nanos()
        );
        let slot: Arc<RateLimitSlot> = ctx
            .route_state
            .preserved(SLOT, || Arc::new(RateLimitSlot::default()));
        let buckets = slot.table_for(&signature, params.quota);
        let dimension = params.dimension;
        let route_id = Arc::clone(&ctx.route_id);
        Ok(Box::new(move |inner: BoxedService| {
            let buckets = Arc::clone(&buckets);
            let route_id = Arc::clone(&route_id);
            BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
                let buckets = Arc::clone(&buckets);
                let route_id = Arc::clone(&route_id);
                let inner = inner.clone();
                async move {
                    let value = dimension_value(dimension, &route_id, &req)?;
                    if !buckets.check(&value) {
                        // 内部细节（路由 id / 维度值）只进日志；客户端只见泛化文案。
                        return Err(GatewayError::RateLimited(format!(
                            "route {route_id}: {}={value}",
                            dimension.label()
                        )));
                    }
                    inner.oneshot(req).await
                }
            }))
        }))
    }
}

/// 计算本次请求的维度值：`client_ip` 取对端 IP（缺失归 `"unknown"`），
/// `route` 取路由 id。
fn dimension_value(
    dimension: Dimension,
    route_id: &Arc<str>,
    req: &http::Request<ProxyBody>,
) -> Result<String, GatewayError> {
    Ok(match dimension {
        Dimension::ClientIp => ctx_of(req)?
            .peer_addr
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        Dimension::Route => route_id.to_string(),
    })
}

/// 解析并校验参数（错误带过滤器 `source` 定位，由编译管线改写到条目）。
fn parse_params(spec: &FilterSpec) -> Result<Params, ConfigError> {
    let dimension = spec
        .param_str("key")
        .and_then(|raw| Dimension::parse(raw).map_err(|m| value_error(spec, m)))?;
    let rate = positive_u32(spec, "rate")?
        .ok_or_else(|| value_error(spec, "parameter \"rate\": missing".to_string()))?;
    let period = positive_duration(spec, "period")?.unwrap_or(DEFAULT_PERIOD);
    let quota = make_quota(rate, period).ok_or_else(|| {
        value_error(
            spec,
            "parameter \"rate\": too high for the given period (zero replenish interval)"
                .to_string(),
        )
    })?;
    Ok(Params {
        dimension,
        rate,
        period,
        quota,
    })
}

#[cfg(test)]
#[path = "rate_limit_tests.rs"]
mod tests;

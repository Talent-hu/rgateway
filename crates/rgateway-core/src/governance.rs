//! 跨配置热重载保留的治理状态：熔断器（Circuit Breaker）与过滤器保留
//! 状态（限流桶、负载均衡计数器等）。
//!
//! 设计要点：reload 编译出全新不可变快照，但经 [`StateRegistry`] 复用本
//! 模块对象，热更新绝不重置熔断滑动窗口或限流桶——治理效果必须跨配置
//! 版本连续，否则一次 reload 就会把仍在熔断期的实例放回流量。

use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use tokio::time::{Duration, Instant};

use crate::error::GatewayError;
use crate::lb::Instance;

/// 保留的实例级治理状态标识（管理 API 用它定位熔断器/状态槽）：
/// `route_id` 为路由 id，`instance_id` 与 [`crate::lb::Instance::id`] 同源。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StateKey {
    pub route_id: Arc<str>,
    pub instance_id: Arc<str>,
}

impl StateKey {
    /// 从 `&str`/`String` 等廉价构造状态键（转入 `Arc<str>`）。
    pub fn new(route_id: impl Into<Arc<str>>, instance_id: impl Into<Arc<str>>) -> Self {
        Self {
            route_id: route_id.into(),
            instance_id: instance_id.into(),
        }
    }
}

/// 熔断器生命周期三态：Closed 放行 / Open 拒绝 / HalfOpen 限流探测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

impl BreakerState {
    /// 稳定标签（指标与管理输出用），如 `"half_open"`。
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

/// 滑动窗口错误率熔断参数（来自 YAML 过滤器配置）。
#[derive(Debug, Clone, PartialEq)]
pub struct BreakerConfig {
    /// 滑动窗口长度（按已完成调用计数）。
    pub sliding_window: usize,
    /// 触发熔断的失败率阈值，取值 `[0, 1]`。
    pub failure_rate: f64,
    /// 窗口内至少累计多少次完成调用才允许触发（防小样本误判）。
    pub min_calls: usize,
    /// Open 状态持续多久后允许转入 HalfOpen 探测。
    pub open_duration: Duration,
    /// HalfOpen 期间允许并发在途的探测调用上限。
    pub half_open_max_calls: u32,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            sliding_window: 20,
            failure_rate: 0.5,
            min_calls: 10,
            open_duration: Duration::from_secs(5),
            half_open_max_calls: 3,
        }
    }
}

struct BreakerInner {
    state: BreakerState,
    /// 当前 Open 期的起点（Closed/HalfOpen 时为 None）。
    opened_at: Option<Instant>,
    /// 滑动窗口里的已完成调用（true = 成功）。
    window: VecDeque<bool>,
    /// HalfOpen 期间在途的探测数（permit 存续即占用一席）。
    half_open_in_flight: u32,
}

/// 实例级熔断器（滑动窗口错误率三态机）。
///
/// 状态与转移条件（[`Self::check`] 取许可、[`BreakerPermit::record`] 收结果）：
/// * **Closed** —— 放行全部调用，结果逐条压入滑动窗口（长 `sliding_window`，
///   超出挤掉最旧）；样本数 ≥ `min_calls` 且失败率 ≥ `failure_rate` 转 **Open**。
/// * **Open** —— [`Self::check`] 直接 `Err(CircuitOpen)`；`opened_at` 起 `open_duration`
///   到期后，下一次 check 转 **HalfOpen** 并记为首个探测。
/// * **HalfOpen** —— 最多 `half_open_max_calls` 个探测并发在途，超限仍
///   `Err(CircuitOpen)`；探测成功立即回 **Closed**（清空窗口），失败回
///   **Open** 并重启计时。
///
/// [`BreakerPermit`] 未 record 即被 drop（取消/panic）按失败计入：安全侧
/// 默认，也防探测槽泄漏。
pub struct Breaker {
    route_id: Arc<str>,
    instance_id: Arc<str>,
    config: Mutex<BreakerConfig>,
    inner: Mutex<BreakerInner>,
}

/// [`Breaker::check`] 返回的调用许可；用 [`BreakerPermit::record`] 上报结果。
pub struct BreakerPermit<'a> {
    breaker: &'a Breaker,
    half_open: bool,
    recorded: bool,
}

impl<'a> BreakerPermit<'a> {
    /// 上报调用结果（`success`）并释放 HalfOpen 探测槽；消耗自身。
    pub fn record(mut self, success: bool) {
        self.recorded = true;
        self.breaker.record_outcome(success, self.half_open);
    }
}

impl<'a> Drop for BreakerPermit<'a> {
    fn drop(&mut self) {
        // 未 record 就 drop（结果没来得及上报）：按失败计，见 Breaker 文档。
        if !self.recorded {
            self.breaker.record_outcome(false, self.half_open);
        }
    }
}

impl Breaker {
    /// 创建熔断器，初始 Closed、窗口为空；`route_id`/`instance_id` 用于
    /// 错误消息与指标标签。
    pub fn new(
        route_id: impl Into<Arc<str>>,
        instance_id: impl Into<Arc<str>>,
        config: BreakerConfig,
    ) -> Self {
        Self {
            route_id: route_id.into(),
            instance_id: instance_id.into(),
            config: Mutex::new(config),
            inner: Mutex::new(BreakerInner {
                state: BreakerState::Closed,
                opened_at: None,
                window: VecDeque::new(),
                half_open_in_flight: 0,
            }),
        }
    }

    /// 当前状态；内部锁中毒时保守按 Closed 报告（不阻塞管理查询）。
    pub fn state(&self) -> BreakerState {
        self.inner
            .lock()
            .map(|i| i.state)
            .unwrap_or(BreakerState::Closed)
    }

    /// 配置热重载时替换阈值参数，滑动窗口/状态等运行数据原样保留。
    pub fn reconfigure(&self, config: BreakerConfig) {
        if let Ok(mut cfg) = self.config.lock() {
            *cfg = config;
        }
    }

    /// 尝试获取一次调用许可；`Err(CircuitOpen)`：Open 且未到探测期，或
    /// HalfOpen 并发探测已达上限。Open 到期后的首次 check 在此转 HalfOpen。
    pub fn check(&self) -> Result<BreakerPermit<'_>, GatewayError> {
        let mut inner = self.lock_inner();
        match inner.state {
            // Closed：无条件放行，结果稍后由 record_outcome 计入窗口。
            BreakerState::Closed => Ok(BreakerPermit {
                breaker: self,
                half_open: false,
                recorded: false,
            }),
            BreakerState::Open => {
                let cfg = self.config();
                let opened_at = inner.opened_at.unwrap_or_else(Instant::now);
                // 探测期已到：转入 HalfOpen，当前调用占用第一个探测槽。
                if opened_at + cfg.open_duration <= Instant::now() {
                    self.transition(&mut inner, BreakerState::HalfOpen);
                    inner.half_open_in_flight = 0;
                    inner.half_open_in_flight += 1;
                    Ok(BreakerPermit {
                        breaker: self,
                        half_open: true,
                        recorded: false,
                    })
                } else {
                    Err(GatewayError::CircuitOpen(format!(
                        "{}/{}",
                        self.route_id, self.instance_id
                    )))
                }
            }
            BreakerState::HalfOpen => {
                let cfg = self.config();
                if inner.half_open_in_flight < cfg.half_open_max_calls {
                    inner.half_open_in_flight += 1;
                    Ok(BreakerPermit {
                        breaker: self,
                        half_open: true,
                        recorded: false,
                    })
                } else {
                    Err(GatewayError::CircuitOpen(format!(
                        "{}/{} half-open probes exhausted",
                        self.route_id, self.instance_id
                    )))
                }
            }
        }
    }

    /// 管理接口：强制回 Closed 并清空滑动窗口。
    pub fn reset(&self) {
        let mut inner = self.lock_inner();
        inner.window.clear();
        inner.opened_at = None;
        inner.half_open_in_flight = 0;
        self.transition(&mut inner, BreakerState::Closed);
    }

    fn record_outcome(&self, success: bool, half_open: bool) {
        let mut inner = self.lock_inner();
        // 无论成败，先归还 HalfOpen 探测槽。
        if half_open && inner.half_open_in_flight > 0 {
            inner.half_open_in_flight -= 1;
        }

        // 迟到的「半开探测」结果（上报时状态已离开 HalfOpen）直接丢弃，
        // 避免污染当前状态。注意此守卫只针对 half_open permit：Closed 期
        // 签发的许可若拖到 HalfOpen 才上报，会落入下方 HalfOpen 分支，被
        // 当作一次探测成败参与转移（成功则提前 Closed、失败则重开并重启计时）。
        if half_open && inner.state != BreakerState::HalfOpen {
            return;
        }

        match inner.state {
            BreakerState::HalfOpen => {
                // 半开期一次成功即完全恢复；一次失败立即重新熔断并重启计时。
                if success {
                    inner.window.clear();
                    inner.opened_at = None;
                    self.transition(&mut inner, BreakerState::Closed);
                } else {
                    inner.opened_at = Some(Instant::now());
                    self.transition(&mut inner, BreakerState::Open);
                }
            }
            BreakerState::Closed => {
                // 正常计数：压入窗口、挤掉最旧，再看错误率是否越线。
                let cfg = self.config();
                inner.window.push_back(success);
                while inner.window.len() > cfg.sliding_window {
                    inner.window.pop_front();
                }
                if inner.window.len() >= cfg.min_calls {
                    let failures = inner.window.iter().filter(|s| !**s).count();
                    let rate = failures as f64 / inner.window.len() as f64;
                    if rate >= cfg.failure_rate {
                        inner.opened_at = Some(Instant::now());
                        self.transition(&mut inner, BreakerState::Open);
                    }
                }
            }
            BreakerState::Open => {
                // 熔断打开前漏过的调用在此迟到上报：忽略，不影响 Open。
            }
        }
    }

    /// 执行状态转移并打点：`breaker_transitions_total` 计数、`breaker_state`
    /// gauge 报当前态（Closed=0 / Open=1 / HalfOpen=2）。
    fn transition(&self, inner: &mut BreakerInner, to: BreakerState) {
        let from = inner.state;
        if from == to {
            return;
        }
        inner.state = to;
        let route = self.route_id.to_string();
        let instance = self.instance_id.to_string();
        metrics::counter!(
            "breaker_transitions_total",
            "route" => route.clone(),
            "instance" => instance.clone(),
            "from" => from.as_label().to_string(),
            "to" => to.as_label().to_string()
        )
        .increment(1);
        metrics::gauge!(
            "breaker_state",
            "route" => route,
            "instance" => instance
        )
        .set(match to {
            BreakerState::Closed => 0.0,
            BreakerState::Open => 1.0,
            BreakerState::HalfOpen => 2.0,
        });
    }

    /// 熔断器此刻是否“无许可即拒绝”：状态为 Open 且 `open_duration` 尚未
    /// 到期。对 Open 熔断器一旦翻为 `false`（探测期已到），下一次
    /// [`Self::check`] 就执行 open → half-open 转移并放探测——目标选择据此
    /// 把到期实例重新放回可选集，恢复链路才跑得起来（与 [`RouteState::breaker_open`] 一致）。
    fn open_not_elapsed(&self) -> bool {
        let inner = self.lock_inner();
        if inner.state != BreakerState::Open {
            return false;
        }
        let cfg = self.config();
        let opened_at = inner.opened_at.unwrap_or_else(Instant::now);
        match opened_at.checked_add(cfg.open_duration) {
            Some(due) => due > Instant::now(),
            // open 窗口超出时钟可表示范围时，视为永不结束（一直保持 Open）。
            None => true,
        }
    }

    fn config(&self) -> BreakerConfig {
        self.config.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, BreakerInner> {
        // 锁中毒意味着有人在更新中途 panic；取回当前（基本一致的）状态
        // 继续用，而不是把整个网关拖垮。
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 单条路由的治理状态，跨配置热重载保留（由 [`StateRegistry`] 托管复用）。
pub struct RouteState {
    pub route_id: Arc<str>,
    /// 需要全局计数的均衡器共享的轮询计数器。
    pub call_counter: AtomicU64,
    /// 实例级熔断器表，key 为实例 id。
    breakers: DashMap<Arc<str>, Arc<Breaker>>,
    /// 按槽名保存的过滤器保留状态（限流桶等），见 [`Self::preserved`]。
    extras: DashMap<&'static str, Arc<dyn Any + Send + Sync>>,
}

impl RouteState {
    /// 创建空状态：计数器归零，无熔断器、无保留槽。
    pub fn new(route_id: impl Into<Arc<str>>) -> Self {
        Self {
            route_id: route_id.into(),
            call_counter: AtomicU64::new(0),
            breakers: DashMap::new(),
            extras: DashMap::new(),
        }
    }

    /// 取（或首次创建）某实例的熔断器；已存在则用 `config` 原地
    /// reconfigure——保留窗口与当前状态，只换阈值（热重载语义）。
    pub fn breaker_for(&self, instance_id: &str, config: BreakerConfig) -> Arc<Breaker> {
        if let Some(b) = self.breakers.get(instance_id) {
            b.value().reconfigure(config);
            return b.value().clone();
        }
        let breaker = Arc::new(Breaker::new(
            self.route_id.clone(),
            Arc::from(instance_id),
            config,
        ));
        self.breakers
            .insert(Arc::from(instance_id), breaker.clone());
        breaker
    }

    /// 管理接口：重置指定实例的熔断器；`instance_id: None` 重置全部实例。返回重置个数。
    pub fn reset_breaker(&self, instance_id: Option<&str>) -> usize {
        match instance_id {
            Some(id) => {
                if let Some(b) = self.breakers.get(id) {
                    b.reset();
                    1
                } else {
                    0
                }
            }
            None => {
                for entry in self.breakers.iter() {
                    entry.value().reset();
                }
                self.breakers.len()
            }
        }
    }

    /// 快照当前各实例熔断状态（instance → state），供管理接口输出。
    pub fn breaker_states(&self) -> Vec<(Arc<str>, BreakerState)> {
        self.breakers
            .iter()
            .map(|e| (e.key().clone(), e.value().state()))
            .collect()
    }

    /// 实例此刻是否应被目标选择跳过：其熔断器存在、处于 Open **且** Open
    /// 期尚未到时长。HalfOpen 不算 open（探测要继续走）。
    ///
    /// 时间感知语义：`open_duration` 一到，即便状态仍是
    /// [`BreakerState::Open`]，本方法也翻回 `false`——目标选择于是把该实例
    /// 重新放回可选集，下一次转发落到它头上时 [`Breaker::check`] 才能执行
    /// open → half-open 恢复探测。没有这条“到期即重新可选”的规则，被跳过
    /// 的实例永远没人探测，熔断器永远无法恢复。
    pub fn breaker_open(&self, instance_id: &str) -> bool {
        self.breakers
            .get(instance_id)
            .map(|b| b.open_not_elapsed())
            .unwrap_or(false)
    }

    /// 按槽名取（或首次创建）保留的类型化状态。
    ///
    /// `make` 仅在首次访问时调用；reload 后再次访问返回先前保留的值，限流
    /// 桶等状态因此跨热重载存活。同名槽若类型变了则整体替换。
    pub fn preserved<T, F>(&self, slot: &'static str, make: F) -> Arc<T>
    where
        T: Any + Send + Sync,
        F: FnOnce() -> Arc<T>,
    {
        if let Some(existing) = self.extras.get(slot) {
            if let Ok(v) = existing.clone().downcast::<T>() {
                return v;
            }
            // 配置热重载后类型变了：替换该 slot。
            drop(existing);
        }
        let fresh = make();
        self.extras.insert(slot, fresh.clone());
        fresh
    }

    /// 丢弃实例已不在 `live` 中的治理状态（实例下线后清理，防状态表膨胀）。
    pub fn retain_instances(&self, live: &[Instance]) {
        let keep: std::collections::HashSet<&Arc<str>> = live.iter().map(|i| &i.id).collect();
        self.breakers.retain(|k, _| keep.contains(k));
    }

    /// 下一个计数器值（共享此状态的均衡器用；Relaxed 原子递增）。
    pub fn next_count(&self) -> u64 {
        self.call_counter.fetch_add(1, Ordering::Relaxed)
    }
}

/// 按路由 id 保存治理状态的注册表，跨热重载保留。
///
/// 由配置层的运行时状态持有（`RuntimeStateRegistry` 复用本类型），每次
/// 新编译出的快照都从这里取各路由的 [`RouteState`]。
#[derive(Default)]
pub struct StateRegistry {
    routes: DashMap<Arc<str>, Arc<RouteState>>,
}

impl StateRegistry {
    /// 创建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取（或首次创建）某路由的保留状态。
    pub fn route_state(&self, route_id: &str) -> Arc<RouteState> {
        if let Some(s) = self.routes.get(route_id) {
            return s.value().clone();
        }
        let state = Arc::new(RouteState::new(Arc::from(route_id)));
        self.routes.insert(Arc::from(route_id), state.clone());
        state
    }

    /// 当前有保留状态的全部路由 id。
    pub fn route_ids(&self) -> Vec<Arc<str>> {
        self.routes.iter().map(|e| e.key().clone()).collect()
    }

    /// 重置熔断器；`instance_id: None` 重置该路由全部实例。返回重置个数。
    pub fn reset_breaker(&self, route_id: &str, instance_id: Option<&str>) -> usize {
        self.routes
            .get(route_id)
            .map(|s| s.reset_breaker(instance_id))
            .unwrap_or(0)
    }

    /// 移除配置中已不存在的路由的状态（reload 完成后调用）；`live` 为现存
    /// 路由 id，不在列表中的条目连同其熔断器/保留槽一并丢弃。
    pub fn retain_routes(&self, live: &[Arc<str>]) {
        let keep: std::collections::HashSet<&Arc<str>> = live.iter().collect();
        self.routes.retain(|k, _| keep.contains(k));
    }
}

#[cfg(test)]
#[path = "governance_tests.rs"]
mod tests;

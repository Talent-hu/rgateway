//! 管理面共享状态：快照存储 + 治理状态注册表 + 指标渲染闭包的注入点。
//!
//! # 为什么单独成型
//!
//! 管理面端点全都只读「当前生效快照」与「跨重载治理状态」，两者都由
//! 装配方构造好后注入：本 crate 不拥有配置热重载，也不持有全局单例，
//! 测试因此可以用真实编译快照驱动每个端点。指标渲染闭包同样注入
//! （`/metrics` 端点用），由装配方决定何时安装指标导出器。

use std::sync::Arc;

use rgateway_config::SnapshotStore;
use rgateway_core::StateRegistry;

/// 渲染 Prometheus 指标文本的闭包（`/metrics` 端点用）。
///
/// 返回 `None` 表示当前无指标可输出（导出器尚未就绪），调用方按需降级。
pub type MetricsRenderer = dyn Fn() -> Option<String> + Send + Sync;

/// 管理面请求处理所需的全部依赖（不可变，构造后只读）。
///
/// `store` 提供生效中的 [`CompiledConfig`](rgateway_config::CompiledConfig)
/// 视图与版本/失败计数；`states` 是跨热重载存活的熔断器等治理状态；
/// `metrics` 是可选的指标渲染闭包（本阶段仅注入点）。
pub struct MgmtState {
    store: Arc<SnapshotStore>,
    states: Arc<StateRegistry>,
    metrics: Option<Arc<MetricsRenderer>>,
}

impl MgmtState {
    /// 构造管理面状态：注入快照存储与治理状态注册表，默认无指标闭包。
    pub fn new(store: Arc<SnapshotStore>, states: Arc<StateRegistry>) -> Self {
        Self {
            store,
            states,
            metrics: None,
        }
    }

    /// 注入指标渲染闭包（消费自身、返回新值，保持不可变风格）。
    pub fn with_metrics(self, metrics: Arc<MetricsRenderer>) -> Self {
        Self {
            metrics: Some(metrics),
            ..self
        }
    }

    /// 当前生效配置的快照存储（含版本号与失败重载计数）。
    pub fn store(&self) -> &Arc<SnapshotStore> {
        &self.store
    }

    /// 跨热重载保留的治理状态注册表（熔断器等）。
    pub fn states(&self) -> &Arc<StateRegistry> {
        &self.states
    }

    /// 指标渲染闭包（`None` = 装配方未注入，指标面未启用）。
    pub fn metrics(&self) -> Option<&Arc<MetricsRenderer>> {
        self.metrics.as_ref()
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

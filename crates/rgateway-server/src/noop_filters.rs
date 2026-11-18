//! 12 个规范过滤器名的恒等（no-op）占位工厂。
//!
//! 背景：`config/rgateway.yaml` 示例配置用了 `RewritePath` / `RateLimit` /
//! `Retry` / `JwtAuth` / `Timeout` / `CircuitBreaker` 等过滤器，而过滤器的
//! 运行时实现属 P4（`rgateway-filters`）。为了让示例配置在实现落地前就能
//! 加载，且「配置通过校验、槽位排入链骨架、运行时无拦截效果」成立，server
//! 启动期为全部 12 个 canonical 名注册恒等包装工厂：`build` 产出的
//! [`ServiceWrapper`] 直接原样返回内层链，只在构建时记一条 debug 日志提示
//! 「本阶段无运行时效果」。
//!
//! canonical 名集合以 [`rgateway_config::FILTER_NAMES`] 为唯一事实源，
//! 测试做双向对拍（注册表 ↔ FILTER_NAMES ↔ `slot_for`）：config 侧增删
//! 名字而注册表漏改时测试立即失败。唯一测不到的方向是「有人给 `slot_for`
//! 加了 match 分支却不动 `FILTER_NAMES`」——编译期无法枚举 match 域，运行
//! 期兜底是编译链查不到工厂时报 `UnknownFilter`（启动期编译即失败）。

use std::sync::Arc;

use rgateway_config::{
    ConfigError, FilterBuildCtx, FilterFactory, FilterRegistry, FilterSpec, ServiceWrapper,
    FILTER_NAMES,
};

/// 全部 canonical 过滤器名（转发自 [`rgateway_config::FILTER_NAMES`]）。
pub const CANONICAL_FILTER_NAMES: [&str; 12] = FILTER_NAMES;

/// 恒等包装工厂：按 canonical 名登记，`build` 返回 `|inner| inner`。
struct NoopFactory {
    name: &'static str,
}

impl FilterFactory for NoopFactory {
    fn name(&self) -> &'static str {
        self.name
    }

    fn build(
        &self,
        _spec: &FilterSpec,
        _ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        // 占位阶段的行为提示：槽位已排入链骨架，但运行期零拦截效果。
        tracing::debug!("filter {} has no runtime effect in this phase", self.name);
        Ok(Box::new(|inner| inner))
    }
}

/// 构造覆盖全部 canonical 名的恒等工厂注册表（server 启动期注册用）。
pub fn noop_registry() -> FilterRegistry {
    let mut registry = FilterRegistry::new();
    for name in CANONICAL_FILTER_NAMES {
        registry.register(Arc::new(NoopFactory { name }));
    }
    registry
}

#[cfg(test)]
#[path = "noop_filters_tests.rs"]
mod tests;

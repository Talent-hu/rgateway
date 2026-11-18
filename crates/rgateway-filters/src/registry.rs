//! 内置过滤器工厂注册表：只注册本 crate 实装的过滤器。
//!
//! 其余过滤器名（JwtAuth / RewritePath / CORS 等）属 P5/P6 阶段，
//! 不在此注册——注册表就是运行时能力的权威清单，多注册一个空壳工厂
//! 比缺注册更危险（配置能编译但行为缺失）。

use std::sync::Arc;

use rgateway_config::FilterRegistry;

use crate::{
    ApiKeyAuthFactory, BodyLimitFactory, CircuitBreakerFactory, IpFilterFactory, JwtAuthFactory,
    RateLimitFactory, RetryFactory, TimeoutFactory,
};

/// 构建内置过滤器注册表：`ApiKeyAuth` / `BodyLimit` / `CircuitBreaker` /
/// `IpFilter` / `JwtAuth` / `RateLimit` / `Retry` / `Timeout` 八个工厂
/// （[`FilterRegistry::names`] 字典序可见）。
pub fn registry() -> FilterRegistry {
    let mut registry = FilterRegistry::new();
    registry.register(Arc::new(ApiKeyAuthFactory));
    registry.register(Arc::new(BodyLimitFactory));
    registry.register(Arc::new(CircuitBreakerFactory));
    registry.register(Arc::new(IpFilterFactory));
    registry.register(Arc::new(JwtAuthFactory));
    registry.register(Arc::new(RateLimitFactory));
    registry.register(Arc::new(RetryFactory));
    registry.register(Arc::new(TimeoutFactory));
    registry
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;

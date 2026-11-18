//! CircuitBreaker 过滤器：实例级熔断（槽位 `Breaker`，Target 之内、
//! Timeout 之外）。
//!
//! 参数（全部可选，缺省用 [`BreakerConfig::default`]：20/0.5/10/5s/3）：
//! `sliding_window`（> 0）、`failure_rate`（0..=1，接受 `"50%"`）、
//! `min_calls`（<= sliding_window）、`open_duration`（> 0）、
//! `half_open_max_calls`（> 0）。
//!
//! # 为什么复用 core 的实例级 Breaker
//!
//! 熔断器由 [`RouteState::breaker_for`] 按 **实例 id**（`ctx.target()`
//! 选定的 [`UpstreamTarget`](rgateway_core::UpstreamTarget)）获取：同实例
//! 跨请求、跨配置热重载共用同一个三态机（reconfigure 只换阈值，滑动
//! 窗口与状态原样保留）。本过滤器**绝不**自建全局单例——熔断窗口跨
//! 重载复用是 core 既有语义，重建单例会在 reload 后把仍处熔断期的
//! 实例放回流量。链序保证 Target 在本层外侧：熔断判的就是「本次已
//! 选定的这个实例」。
//!
//! # 结果分类（记入滑动窗口的成败）
//!
//! 记**失败**：传输错误（`Unavailable`）、`CircuitOpen`、
//! `DeadlineExceeded`、`Internal`，以及响应 `status >= 500`。
//! 记**成功**：其余（客户端故障如 4xx/`InvalidArgument`、被上游正常
//! 处理的 `RateLimited` 等不代表实例故障）。取舍：熔断衡量的是「上
//! 游实例是否可服务」——超时、连接失败、5xx 是实例侧信号；4xx 是客户端
//! 行为，把它们计入失败会让坏客户端把好实例熔断掉。
//!
//! [`BreakerPermit`] 未 record 即 drop（取消/panic）按失败计入是 core
//! 既有安全侧语义——实现必须经 permit 上报结果，绝不绕过。

use std::sync::Arc;

use rgateway_config::{ConfigError, FilterBuildCtx, FilterFactory, FilterSpec, ServiceWrapper};
use rgateway_core::{BoxedService, BreakerConfig, ErrorKind, GatewayError, ProxyBody};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

use crate::params::{
    opt_usize, positive_duration, positive_u32, positive_usize, unit_f64, value_error,
};
use crate::util::ctx_of;

/// CircuitBreaker 过滤器工厂（canonical 名 `"CircuitBreaker"`，
/// 槽 [`rgateway_config::FilterSlot::Breaker`]）。
pub struct CircuitBreakerFactory;

impl FilterFactory for CircuitBreakerFactory {
    fn name(&self) -> &'static str {
        "CircuitBreaker"
    }

    fn build(
        &self,
        spec: &FilterSpec,
        ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        let config = parse_config(spec)?;
        let route_state = Arc::clone(&ctx.route_state);
        Ok(Box::new(move |inner: BoxedService| {
            let route_state = Arc::clone(&route_state);
            let config = config.clone();
            BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
                let route_state = Arc::clone(&route_state);
                let config = config.clone();
                let inner = inner.clone();
                async move {
                    let rctx = ctx_of(&req)?;
                    let Some(target) = rctx.target() else {
                        // 目标尚未选定（Target 槽在本层外侧负责选实例）：
                        // 熔断无判据，防御性直通且不创建熔断器。
                        return inner.oneshot(req).await;
                    };
                    // breaker_for 热重载语义：已存在则 reconfigure 保留窗口。
                    let breaker = route_state.breaker_for(target.instance_id.as_ref(), config);
                    // check() 被拒（Open 未到探测期 / 半开探测占满）→ CircuitOpen。
                    let permit = breaker.check()?;
                    let result = inner.oneshot(req).await;
                    // 经 permit 上报（未 record 的 drop 按失败计——core 语义）。
                    permit.record(!is_failure(&result));
                    result
                }
            }))
        }))
    }
}

/// 判定一次调用结果是否算「实例侧失败」（分类理由见模块文档）。
fn is_failure(result: &Result<http::Response<ProxyBody>, GatewayError>) -> bool {
    match result {
        Ok(resp) => resp.status().as_u16() >= 500,
        Err(err) => matches!(
            err.kind(),
            ErrorKind::Unavailable
                | ErrorKind::CircuitOpen
                | ErrorKind::DeadlineExceeded
                | ErrorKind::Internal
        ),
    }
}

/// 解析熔断参数（全部可选，缺省 [`BreakerConfig::default`]）并做跨字段
/// 校验：`min_calls <= sliding_window`、数值范围（validate 只覆盖部分
/// 组合，构建期做最终把关，坏配置在编译期快速失败）。
fn parse_config(spec: &FilterSpec) -> Result<BreakerConfig, ConfigError> {
    let defaults = BreakerConfig::default();
    let sliding_window = positive_usize(spec, "sliding_window")?.unwrap_or(defaults.sliding_window);
    let failure_rate = unit_f64(spec, "failure_rate")?.unwrap_or(defaults.failure_rate);
    let min_calls = opt_usize(spec, "min_calls")?.unwrap_or(defaults.min_calls);
    if min_calls > sliding_window {
        return Err(value_error(
            spec,
            format!("parameter \"min_calls\": must be <= sliding_window ({sliding_window})"),
        ));
    }
    let open_duration = positive_duration(spec, "open_duration")?.unwrap_or(defaults.open_duration);
    let half_open_max_calls =
        positive_u32(spec, "half_open_max_calls")?.unwrap_or(defaults.half_open_max_calls);
    Ok(BreakerConfig {
        sliding_window,
        failure_rate,
        min_calls,
        open_duration,
        half_open_max_calls,
    })
}

#[cfg(test)]
#[path = "breaker_tests.rs"]
mod tests;

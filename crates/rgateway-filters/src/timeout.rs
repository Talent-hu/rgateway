//! Timeout 过滤器：单次尝试的响应头超时（槽位 `Timeout`，链上最贴近
//! handler 的一层）。
//!
//! 参数：`per_attempt` 或 `timeout`（二选一必填时长，`per_attempt` 优先，
//! 与 validate 的约定一致），必须 > 0。
//!
//! # 契约：只约束响应头（与 Deadline 同语义）
//!
//! 预算约束的是拿到响应 **headers** 的时刻——超时映射
//! [`GatewayError::DeadlineExceeded`]（内部消息 `"per-attempt timeout …"`，
//! 只进日志）；抢在预算内完成的调用原样透传，其响应体**流式继续**，
//! 不被超时截断。背后的取舍：响应体可能是长流（SSE、大文件下载），
//! 纳入超时会截断正常长流；Timeout 承诺的是「多久拿到响应头」。
//!
//! 与链上 Deadline 层的分工：Deadline 在 Retry 之外约束整请求总预算
//! （含全部重试），Timeout 在 Breaker 之内约束**单次上游尝试**，重试的
//! 每次尝试各自获得完整预算。

use std::time::Duration;

use rgateway_config::{
    ConfigError, FilterBuildCtx, FilterFactory, FilterSpec, Location, ServiceWrapper,
};
use rgateway_core::{BoxedService, GatewayError, ProxyBody};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

/// Timeout 过滤器工厂（canonical 名 `"Timeout"`，槽 [`rgateway_config::FilterSlot::Timeout`]）。
pub struct TimeoutFactory;

impl FilterFactory for TimeoutFactory {
    fn name(&self) -> &'static str {
        "Timeout"
    }

    fn build(
        &self,
        spec: &FilterSpec,
        _ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        let budget = parse_budget(spec)?;
        Ok(Box::new(move |inner: BoxedService| {
            BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
                let inner = inner.clone();
                async move {
                    // 只把关响应头：响应体在 timeout 之外继续流式（模块契约）。
                    match tokio::time::timeout(budget, inner.oneshot(req)).await {
                        Ok(result) => result,
                        Err(_elapsed) => Err(per_attempt_error(budget)),
                    }
                }
            }))
        }))
    }
}

/// 解析单次尝试预算：`per_attempt` 优先、`timeout` 兜底，二选一必填且
/// 必须 > 0（零预算等于全拒绝，视为配置笔误快速失败）。
fn parse_budget(spec: &FilterSpec) -> Result<Duration, ConfigError> {
    let at = |message: String| ConfigError::value(Location::section(&spec.source), message);
    for key in ["per_attempt", "timeout"] {
        if let Some(dur) = spec.param_duration_opt(key)? {
            if dur.is_zero() {
                return Err(at(format!("parameter {key:?}: must be > 0")));
            }
            return Ok(dur);
        }
    }
    Err(at(
        "parameter \"per_attempt\" (or \"timeout\") missing or not a duration".to_string(),
    ))
}

/// 超时错误：`DeadlineExceeded`，内部消息点名 per-attempt（仅日志通道）。
fn per_attempt_error(budget: Duration) -> GatewayError {
    GatewayError::DeadlineExceeded(format!("per-attempt timeout after {budget:?}"))
}

#[cfg(test)]
#[path = "timeout_tests.rs"]
mod tests;

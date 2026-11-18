//! Retry 过滤器：外层重试（槽位 `Retry`，包在 Target 之外）。
//!
//! # 语义（与 README「每次重试重新选目标」一致）
//!
//! * **位置**：Retry 槽包在 Target 外侧——重试一轮 = 重新走内层链一次，
//!   Target 会重新选实例并覆写 `ctx.target`，熔断仍处 Open 的实例被
//!   跳过。本层只决定「再走一轮」，绝不自己挑实例。
//! * **可重试判定**：方法闸（`methods` 非空时请求方法必须在列表内，
//!   否则绝不重试）+ 三选一触发——inner 返回 `Err` 且 kind 为
//!   `Unavailable` / `CircuitOpen`（`RateLimited` / `DeadlineExceeded` /
//!   `Unauthenticated` 等一律不重试）；或响应 status 在 `statuses`；或
//!   gRPC 家族响应的 `grpc-status`（响应头或 trailer 语义）在
//!   `grpc_statuses`。每次重试前丢弃已收下的响应体。
//! * **次数与退避**：总尝试 = 1 + `retries`（≤ 10，防重试风暴）；
//!   退避 `initial × multiplier^n` 封顶 `max`，jitter 见 [`crate::backoff`]。
//!   睡眠用 `tokio::time::sleep`（暂停时钟可精确测试）。
//! * **重放缓冲取舍**：可重放要先把请求体收进内存，收多少就是攻击面
//!   ——`FilterBuildCtx::max_retry_buffer` 是硬上限，超限的请求**禁用
//!   重试**直接单次透传（行为与无 Retry 完全一致），细节见
//!   [`crate::replay`]。
//! * **错误文案红线**：本层不构造新错误；耗尽次数原样返回最后一次的
//!   错误/响应。客户端文案一律经 [`GatewayError::public_message`] /
//!   [`GatewayError::grpc_message`] 泛化，内部细节只走日志通道。

use bytes::Bytes;
use rgateway_config::{ConfigError, FilterBuildCtx, FilterFactory, FilterSpec, ServiceWrapper};
use rgateway_core::{full_body, BoxedService, ErrorKind, GatewayError, ProxyBody};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

use crate::backoff::{Backoff, Jitter, SplitMix64};
use crate::params::value_error;
use crate::replay::{self, Replay};
use crate::util::{ctx_of, grpc_status_in, is_grpc_family};

/// `retries` 上限：防重试风暴（契约与 validate 一致）。
const MAX_RETRIES: u64 = 10;

/// `retries` 缺省值：对齐 Spring Cloud Gateway 的 `DEFAULT_RETRIES = 3`。
///
/// 参数契约（`docs/config-reference.md`）把 `retries` 定为可选、缺省由运行
/// 时决定；显式声明 `Retry` 过滤器就是要求重试语义，缺省 0（不重试）会让
/// `{name: Retry}` 变成空操作，故取 SCG 语义的 3。
const DEFAULT_RETRIES: u64 = 3;

/// Retry 过滤器工厂（canonical 名 `"Retry"`，槽 [`rgateway_config::FilterSlot::Retry`]）。
pub struct RetryFactory;

impl FilterFactory for RetryFactory {
    fn name(&self) -> &'static str {
        "Retry"
    }

    fn build(
        &self,
        spec: &FilterSpec,
        ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        let policy = RetryPolicy::parse(spec)?;
        let max_retry_buffer = ctx.max_retry_buffer;
        Ok(Box::new(move |inner: BoxedService| {
            // 外层 Fn 闭包会被多路由复用：先把策略克隆出来再交给内层。
            let policy = policy.clone();
            BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
                let inner = inner.clone();
                let policy = policy.clone();
                async move { policy.run(inner, req, max_retry_buffer).await }
            }))
        }))
    }
}

/// 归一化后的重试策略（解析后不可变，多路由并发共享一份拷贝）。
#[derive(Debug, Clone)]
pub(crate) struct RetryPolicy {
    /// 重试次数（总尝试 = 1 + retries，≤ [`MAX_RETRIES`]）。
    retries: u64,
    /// 方法闸：非空时仅列表内方法参与重试。
    methods: Vec<String>,
    /// 触发重试的 HTTP 状态码。
    statuses: Vec<u16>,
    /// 触发重试的 gRPC 状态码（响应头或 trailer 语义）。
    grpc_statuses: Vec<u64>,
    /// 退避策略。
    backoff: Backoff,
}

impl RetryPolicy {
    /// 解析 Retry 参数（契约同 validate 的 `Retry` 分支）：
    /// `retries` 可选（缺省 [`DEFAULT_RETRIES`]）、给出时 ≤ [`MAX_RETRIES`]；
    /// `methods` / `statuses` / `grpc_statuses` 可选列表；`backoff` 可选
    /// 嵌套 map（`initial` 若给出必须 > 0）。
    pub(crate) fn parse(spec: &FilterSpec) -> Result<Self, ConfigError> {
        let retries = spec.param_u64_opt("retries")?.unwrap_or(DEFAULT_RETRIES);
        if retries > MAX_RETRIES {
            return Err(value_error(
                spec,
                format!("parameter \"retries\": must be <= {MAX_RETRIES}"),
            ));
        }
        let methods = spec.param_str_list("methods")?;
        let statuses = parse_statuses(spec)?;
        let grpc_statuses = spec.param_u64_list("grpc_statuses")?;
        let backoff = parse_backoff(spec)?;
        Ok(Self {
            retries,
            methods,
            statuses,
            grpc_statuses,
            backoff,
        })
    }

    /// 执行请求：可重试路径走「重放缓冲 + 尝试循环」，禁用重试的请求
    /// （retries=0 / 方法闸不过 / 缓冲超限）单次透传，行为与无 Retry 完全一致。
    async fn run(
        &self,
        inner: BoxedService,
        req: http::Request<ProxyBody>,
        max_retry_buffer: usize,
    ) -> Result<http::Response<ProxyBody>, GatewayError> {
        if self.retries == 0 || !self.method_allowed(&req) {
            return inner.oneshot(req).await;
        }
        let seed = seed_of(&req);
        match replay::prepare(req, max_retry_buffer).await {
            Replay::Passthrough(req) => inner.oneshot(req).await,
            Replay::Buffered { parts, body } => {
                let mut rng = SplitMix64::new(seed);
                let mut attempt = 0u64;
                loop {
                    let req =
                        http::Request::from_parts(parts.clone(), full_body(Bytes::clone(&body)));
                    match inner.clone().oneshot(req).await {
                        Err(err) => {
                            if !is_retryable_err(&err) || attempt >= self.retries {
                                return Err(err);
                            }
                        }
                        Ok(resp) => {
                            let (retryable, resp) = self.classify(resp, max_retry_buffer).await;
                            if !retryable || attempt >= self.retries {
                                return Ok(resp);
                            }
                            // 每次重试前丢弃已收下的响应体（模块契约）。
                            drop(resp);
                        }
                    }
                    let delay = self.backoff.delay(attempt as u32, &mut rng);
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// 方法闸：`methods` 非空时请求方法必须在列表内（RFC 方法区分大小写）。
    fn method_allowed(&self, req: &http::Request<ProxyBody>) -> bool {
        self.methods.is_empty() || self.methods.iter().any(|m| m == req.method().as_str())
    }

    /// 判定一次响应是否触发重试；响应总是完整可回放的（原样或重建）。
    async fn classify(
        &self,
        resp: http::Response<ProxyBody>,
        max_retry_buffer: usize,
    ) -> (bool, http::Response<ProxyBody>) {
        if self.statuses.contains(&resp.status().as_u16()) {
            return (true, resp);
        }
        if self.grpc_statuses.is_empty() || !is_grpc_family(resp.headers()) {
            return (false, resp);
        }
        if let Some(code) = grpc_status_in(resp.headers()) {
            return (self.grpc_statuses.contains(&code), resp);
        }
        // trailer 语义：有界收集找 trailer 里的 grpc-status（超限不判定）。
        replay::peek_trailer(resp, &self.grpc_statuses, max_retry_buffer).await
    }
}

/// HTTP status 列表：每项必须是标准状态码（100..=599，快速失败防笔误）。
fn parse_statuses(spec: &FilterSpec) -> Result<Vec<u16>, ConfigError> {
    spec.param_u64_list("statuses")?
        .into_iter()
        .map(|raw| {
            u16::try_from(raw)
                .ok()
                .filter(|code| (100..=599).contains(code))
                .ok_or_else(|| {
                    value_error(
                        spec,
                        format!("parameter \"statuses\": {raw} is not a valid HTTP status"),
                    )
                })
        })
        .collect()
}

/// 解析 `backoff` 嵌套 map（缺省 [`Backoff::DEFAULT`]）；子键复用
/// `FilterSpec` 类型化访问器（与 validate 对 backoff 的处理同构）。
fn parse_backoff(spec: &FilterSpec) -> Result<Backoff, ConfigError> {
    let defaults = Backoff::DEFAULT;
    if !spec.has("backoff") {
        return Ok(defaults);
    }
    let sub = FilterSpec {
        name: spec.name.clone(),
        params: spec.param_map("backoff")?,
        source: spec.source.clone(),
    };
    let initial = crate::params::positive_duration(&sub, "initial")?.unwrap_or(defaults.initial);
    let max = crate::params::positive_duration(&sub, "max")?.unwrap_or(defaults.max);
    let multiplier = match sub.param_f64_opt("multiplier")? {
        None => defaults.multiplier,
        Some(m) if m.is_finite() && m >= 1.0 => m,
        Some(m) => {
            return Err(value_error(
                &sub,
                format!("parameter \"multiplier\": must be >= 1 and finite, got {m}"),
            ))
        }
    };
    let jitter = match sub.param_str_opt("jitter")? {
        None => defaults.jitter,
        Some(s) => match s.trim().to_ascii_lowercase().as_str() {
            "full" => Jitter::Full,
            "none" => Jitter::None,
            other => {
                return Err(value_error(
                    &sub,
                    format!("parameter \"jitter\": must be \"full\" or \"none\", got {other:?}"),
                ))
            }
        },
    };
    Ok(Backoff {
        initial,
        max,
        multiplier,
        jitter,
    })
}

/// 可重试的错误分类：仅 `Unavailable` / `CircuitOpen`（模块文档语义）。
fn is_retryable_err(err: &GatewayError) -> bool {
    matches!(err.kind(), ErrorKind::Unavailable | ErrorKind::CircuitOpen)
}

/// 抖动种子：从请求的 `request_id` 确定性派生（同请求重试序列可复现）；
/// 缺失 RequestCtx 时退化为常量 0（不为此引入新错误）。
fn seed_of(req: &http::Request<ProxyBody>) -> u64 {
    ctx_of(req)
        .map(|ctx| {
            let id = ctx.request_id.as_u128();
            id as u64 ^ (id >> 64) as u64
        })
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "retry_behavior_tests.rs"]
mod behavior_tests;
#[cfg(test)]
#[path = "retry_test_util.rs"]
mod test_util;
#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;

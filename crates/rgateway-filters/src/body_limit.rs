//! BodyLimit 过滤器：请求体大小限长（槽位 `Route`，请求改写类）。
//!
//! 参数：`max_body`（必填大小，number 按字节、字符串带 `B/KB/MB/GB`，
//! 1024 进制）。
//!
//! # 两道防线
//!
//! 1. **Content-Length 预检**：声明长度已超限的请求直接拒绝，不进
//!    inner、不读 body——省掉无谓的传输与解析。
//! 2. **流式限长**：请求体经 [`http_body_util::Limited`] 逐帧核算，
//!    没有（或谎报）Content-Length 的超限体在越界帧处截断。
//!
//! # 错误归类：客户端故障 = InvalidArgument
//!
//! 超限与非限长的请求体错误（客户端断流、传输残缺等）统一映射
//! [`GatewayError::InvalidArgument`]（HTTP 400 / gRPC 3，与 dispatch 的
//! 400 语义一致）——它们是客户端行为，**不**归 `Unavailable`（那会误报
//! 上游故障并误导重试/告警）。错误变体携带的细节（长度、底层错误）
//! 只进日志，客户端文案是泛化的 "invalid argument"。

use http_body_util::BodyExt;
use http_body_util::Limited;
use rgateway_config::{ConfigError, FilterBuildCtx, FilterFactory, FilterSpec, ServiceWrapper};
use rgateway_core::{boxed_body, BoxedService, GatewayError, ProxyBody, ProxyError};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

use crate::util::content_length_of;

/// BodyLimit 过滤器工厂（canonical 名 `"BodyLimit"`，槽 [`rgateway_config::FilterSlot::Route`]）。
pub struct BodyLimitFactory;

impl FilterFactory for BodyLimitFactory {
    fn name(&self) -> &'static str {
        "BodyLimit"
    }

    fn build(
        &self,
        spec: &FilterSpec,
        _ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        let max_body = spec.param_size("max_body")?;
        Ok(Box::new(move |inner: BoxedService| {
            BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
                let inner = inner.clone();
                async move {
                    // 防线 1：Content-Length 预检（声明超限即拒，不进 inner）
                    if let Some(declared) = content_length_of(&req) {
                        if declared > max_body {
                            return Err(GatewayError::InvalidArgument(format!(
                                "content-length {declared} exceeds max_body {max_body}"
                            )));
                        }
                    }
                    // 防线 2：流式限长（错误统一归客户端故障，见模块文档）
                    let (parts, body) = req.into_parts();
                    let limited =
                        Limited::new(body, max_body).map_err(move |e| body_error(e, max_body));
                    let req = http::Request::from_parts(parts, boxed_body(limited));
                    inner.oneshot(req).await
                }
            }))
        }))
    }
}

/// 请求体错误归一：限长与非限长都按客户端故障映射
/// [`GatewayError::InvalidArgument`]（区别只在内部消息，供日志定位）。
fn body_error(err: Box<dyn std::error::Error + Send + Sync>, max_body: usize) -> ProxyError {
    let gateway = if err
        .downcast_ref::<http_body_util::LengthLimitError>()
        .is_some()
    {
        GatewayError::InvalidArgument(format!("request body exceeds max_body {max_body}"))
    } else {
        GatewayError::InvalidArgument(format!("request body read error: {err}"))
    };
    ProxyError::Gateway(gateway)
}

#[cfg(test)]
#[path = "body_limit_tests.rs"]
mod tests;

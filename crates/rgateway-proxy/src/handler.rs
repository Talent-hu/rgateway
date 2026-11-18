//! 协议处理器（handler）：HTTP 转发的最内层服务与准入闸门。
//!
//! P3 只把 HTTP/1.1 与 HTTP/2 **明文**转发到上游；每个请求依次过三个闸：
//!
//! 1. **scheme 闸**（路由静态）：[`RouteCompileInfo`] 的 `uri` 为 Direct 且
//!    scheme 是 `https` 时拒绝——严禁把 https 当明文转发（防 TLS 静默降级）。
//!    必须读 `RouteCompileInfo.uri` 而不是 `UpstreamTarget`：目标选择层只
//!    产出 `ip:port`，scheme 信息在那边已被丢弃。`lb://` 路由放行（静态
//!    实例只有 ip:port 无 scheme）。**先于协议闸**：它是路由级配置属性，
//!    与请求无关，7 种协议都应落在同一拒绝形态上。
//! 2. **协议闸**（请求级）：`ctx.protocol` 为 Http1/Http2 才转发；WebSocket /
//!    Grpc / GrpcWeb / GrpcWebText / RestJson 一律拒绝（后续阶段落地）。
//! 3. **目标闸**：`ctx.target()` 缺失（目标选择层未选中实例）时拒绝。
//!
//! # 错误渲染不在这里
//!
//! 闸门与转发只返回 [`GatewayError`]，由 dispatch 的 `error_render` 按
//! `ctx.protocol` 渲染（HTTP 状态码 + JSON / gRPC trailers）。客户端可见
//! 消息只能是 `public_message()` 的静态文案；内部细节（协议名、route id、
//! 后续阶段字样）只进日志通道（`Display`）。

use std::sync::Arc;
use std::time::Duration;

use http::Request;
use tower::service_fn;
use tower::util::BoxCloneService;

use rgateway_config::spec::{RouteCompileInfo, RouteHandlerFactory};
use rgateway_config::target::request_ctx;
use rgateway_core::{BoxedService, GatewayError, Protocol, ProxyBody, RouteUri};

use crate::upstream::UpstreamClient;

/// P3 支持转发的协议：HTTP/1.1 与 HTTP/2（明文）。
const FORWARD_PROTOCOLS: [Protocol; 2] = [Protocol::Http1, Protocol::Http2];

/// HTTP 转发处理器工厂：实现 [`RouteHandlerFactory`]，由装配方注入 config
/// 编译期（依赖倒置：config 不感知代理实现）。server 级设置（建连超时等）
/// 在构造时注入，build 出的每个路由服务共享同一上游客户端配置。
pub struct HttpHandlerFactory {
    connect_timeout: Duration,
}

impl HttpHandlerFactory {
    /// 构造工厂：`connect_timeout` 等 server 级设置由装配方注入。
    pub fn new(connect_timeout: Duration) -> Self {
        Self { connect_timeout }
    }
}

impl RouteHandlerFactory for HttpHandlerFactory {
    fn build(&self, route: &RouteCompileInfo) -> BoxedService {
        let client = Arc::new(UpstreamClient::new(self.connect_timeout));
        let route_uri = route.uri.clone();
        let route_id = route.id.clone();
        BoxCloneService::new(service_fn(move |req: Request<ProxyBody>| {
            let client = Arc::clone(&client);
            let route_uri = route_uri.clone();
            let route_id = route_id.clone();
            async move { serve(&client, &route_uri, &route_id, req).await }
        }))
    }
}

/// 单请求处理：取上下文 → 过闸（scheme → 协议 → 目标）→ 转发。
///
/// # 错误
///
/// 闸门拒绝与转发失败的 [`GatewayError`]；渲染交给 dispatch 层。
async fn serve(
    client: &UpstreamClient,
    route_uri: &RouteUri,
    route_id: &str,
    req: Request<ProxyBody>,
) -> Result<http::Response<ProxyBody>, GatewayError> {
    let ctx =
        request_ctx(&req).ok_or_else(|| GatewayError::Internal("missing request ctx".into()))?;
    gate_scheme(route_uri)?;
    gate_protocol(ctx.protocol)?;
    let target = ctx.target().ok_or_else(|| {
        GatewayError::Unavailable(format!("no upstream target selected for route {route_id}"))
    })?;
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    client.forward(req, target.addr, &path_and_query).await
}

/// scheme 闸：直连 `https` 上游拒绝（严禁 TLS 静默降级），`lb://` 放行。
///
/// 内部细节点名 `https upstream planned for a later phase`，客户端只见
/// 泛化文案（见 `GatewayError` 双通道契约）。
fn gate_scheme(route_uri: &RouteUri) -> Result<(), GatewayError> {
    if let RouteUri::Direct(uri) = route_uri {
        if uri.scheme_str() == Some("https") {
            return Err(GatewayError::Unavailable(
                "https upstream planned for a later phase".to_string(),
            ));
        }
    }
    Ok(())
}

/// 协议闸：P3 只转发 HTTP/1.1 与 HTTP/2；其余协议待后续阶段落地
/// （内部细节带 `planned for a later phase` 字样，仅进日志）。
fn gate_protocol(protocol: Protocol) -> Result<(), GatewayError> {
    if FORWARD_PROTOCOLS.contains(&protocol) {
        Ok(())
    } else {
        Err(GatewayError::Unavailable(format!(
            "protocol {} forwarding is planned for a later phase",
            protocol.as_label()
        )))
    }
}

#[cfg(test)]
#[path = "handler_tests.rs"]
mod tests;

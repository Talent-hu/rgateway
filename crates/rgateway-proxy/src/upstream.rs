//! 上游（upstream）HTTP 转发：把入站请求明文转发到选定的上游实例。
//!
//! # 设计要点
//!
//! * **客户端**：hyper-util legacy `Client` + `HttpConnector`（明文 TCP，
//!   无 ALPN），按 `connect_timeout` 限制建连时间；连接池跨请求复用。
//! * **出站 URI**：`http://{target}{path_and_query}`——目标是 [`SocketAddr`]
//!   （由目标选择层解析好），路径与查询串由处理器从入站 URI 透传。
//! * **版本归一**：出站请求的 `version` 一律归一为 HTTP/1.1。hyper-util 对
//!   h1 池化连接上的 HTTP/2 请求直接报 `UserUnsupportedVersion`，而出口
//!   固定明文 HTTP/1.1（`HttpConnector` 无 ALPN），不归一必失败。
//! * **头治理**：请求头走 [`prepare_upstream`]、响应头走
//!   [`finalize_response`]（语义见 `headers` 模块）。
//! * **错误双通道**：客户端只见 [`GatewayError::public_message`] 泛化文案；
//!   上游地址、io 原文等内部细节只进 `Display`（日志通道）。
//! * **错误还原**：请求体（数据面）抛出的 [`ProxyError`] 会被 hyper 包装成
//!   user body 错误；转发失败时沿 `source()` 链 `downcast` 还原原语义，
//!   还原不了才走 `Unavailable("upstream request failed: …")` 兜底。

use std::error::Error as StdError;
use std::net::SocketAddr;
use std::time::Duration;

use http::{Request, Response, Uri, Version};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use rgateway_config::target::request_ctx;
use rgateway_core::{boxed_body, ErrorKind, GatewayError, ProxyBody, ProxyError};

use crate::headers::{finalize_response, prepare_upstream};

/// 上游 HTTP 客户端：持有连接池，跨请求复用 TCP 连接。
pub struct UpstreamClient {
    client: Client<HttpConnector, ProxyBody>,
}

impl UpstreamClient {
    /// 构造上游客户端：`connect_timeout` 限制单次建连（含 DNS）时长，
    /// TCP 建连后开启 `TCP_NODELAY`（网关转发以小包低延迟为主）。
    pub fn new(connect_timeout: Duration) -> Self {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        connector.set_connect_timeout(Some(connect_timeout));
        Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
        }
    }

    /// 把请求转发到 `target` 并返回上游响应（体为流式 [`ProxyBody`]）。
    ///
    /// * 出站 URI = `http://{target}{path_and_query}`（`path_and_query` 非法
    ///   时按 [`GatewayError::InvalidArgument`] 快速失败）；
    /// * 请求头经 [`prepare_upstream`] 治理，出站版本归一为 HTTP/1.1；
    /// * 响应头经 [`finalize_response`] 治理，响应体零拷贝流式透传；
    /// * 请求上下文从 request extensions 取出（缺失按内部错误处理）。
    ///
    /// # 错误
    ///
    /// [`GatewayError`]：上游连接/传输失败（`Unavailable`）、出站 URI
    /// 非法（`InvalidArgument`）、请求上下文缺失（`Internal`）。
    pub async fn forward(
        &self,
        req: Request<ProxyBody>,
        target: SocketAddr,
        path_and_query: &str,
    ) -> Result<Response<ProxyBody>, GatewayError> {
        let ctx = request_ctx(&req)
            .ok_or_else(|| GatewayError::Internal("missing request ctx".into()))?;
        let request_id = ctx.request_id.to_string();
        let peer = ctx.peer_addr;
        let client_scheme = ctx.client_scheme.as_str();
        let original_host = original_host(&req);

        let (mut parts, body) = req.into_parts();
        // 版本归一：h1 池化连接对 HTTP/2 出站请求直接报 UserUnsupportedVersion。
        parts.version = Version::HTTP_11;
        let uri = outbound_uri(target, path_and_query)?;
        prepare_upstream(
            &mut parts.headers,
            peer,
            original_host.as_deref(),
            &request_id,
            client_scheme,
        );
        let mut out = Request::from_parts(parts, body);
        *out.uri_mut() = uri;

        let resp = self
            .client
            .request(out)
            .await
            .map_err(|err| recover_request_error(&err))?;
        let (mut resp_parts, resp_body) = resp.into_parts();
        finalize_response(&mut resp_parts.headers, &request_id);
        Ok(Response::from_parts(
            resp_parts,
            map_response_body(resp_body),
        ))
    }
}

/// 出站 URI：`http://{target}{path_and_query}`。`path_and_query` 是系统边界
/// 输入（来自客户端请求行），畸形时快速失败，绝不把非法 URI 发给上游。
fn outbound_uri(target: SocketAddr, path_and_query: &str) -> Result<Uri, GatewayError> {
    format!("http://{target}{path_and_query}")
        .parse()
        .map_err(|e| {
            GatewayError::InvalidArgument(format!("invalid path and query {path_and_query:?}: {e}"))
        })
}

/// 本次请求的原始 Host：`Host` 头优先，其次 URI authority（HTTP/2 的
/// `:authority`）。用于 `X-Forwarded-Host` 覆写。
fn original_host(req: &Request<ProxyBody>) -> Option<String> {
    if let Some(host) = req.headers().get(http::header::HOST) {
        if let Ok(text) = host.to_str() {
            return Some(text.to_string());
        }
    }
    req.uri().authority().map(|a| a.as_str().to_string())
}

/// 转发（请求侧）错误还原：沿 `source()` 链命中 [`ProxyError`] 就还原其
/// 语义（见 [`recover_proxy`]），还原不了走 `Unavailable` 兜底——内部消息
/// 带底层细节（Debug 含 cause 链），客户端只见泛化文案。
fn recover_request_error(err: &hyper_util::client::legacy::Error) -> GatewayError {
    recover_proxy(err)
        .map(ProxyError::into_gateway_error)
        .unwrap_or_else(|| GatewayError::Unavailable(format!("upstream request failed: {err:?}")))
}

/// 响应体（数据面）错误映射：沿链命中 [`ProxyError`] 还原，否则归入
/// [`ProxyError::Upstream`]（内部细节进消息，仅日志可见）。
fn map_response_body(body: hyper::body::Incoming) -> ProxyBody {
    boxed_body(body.map_err(|e| {
        recover_proxy(&e)
            .unwrap_or_else(|| ProxyError::Upstream(format!("upstream response failed: {e}")))
    }))
}

/// 沿 `source()` 链寻找被 hyper 包装的 [`ProxyError`]，重建拥有型副本
/// （hyper 的 user body 错误把原错误挂在 cause 链上，只读引用无法消费，
/// 重建规则见 [`rebuild_proxy`]）。
fn recover_proxy(err: &(dyn StdError + 'static)) -> Option<ProxyError> {
    let mut current = Some(err);
    while let Some(e) = current {
        if let Some(pe) = e.downcast_ref::<ProxyError>() {
            return Some(rebuild_proxy(pe));
        }
        current = e.source();
    }
    None
}

/// 按变体重建 [`ProxyError`]：原值不可克隆，重建保留错误语义与内部细节。
fn rebuild_proxy(pe: &ProxyError) -> ProxyError {
    match pe {
        ProxyError::Upstream(msg) => ProxyError::Upstream(msg.clone()),
        ProxyError::Io(e) => ProxyError::Io(std::io::Error::new(e.kind(), e.to_string())),
        ProxyError::Gateway(g) => ProxyError::Gateway(rebuild_gateway(g)),
    }
}

/// 按 [`ErrorKind`] 浅重建 [`GatewayError`]：原值不可克隆，`Display` 文本
/// （含原内部细节）作为新细节，状态码映射由 kind 保持一致。
fn rebuild_gateway(g: &GatewayError) -> GatewayError {
    let text = g.to_string();
    match g.kind() {
        ErrorKind::Unavailable => GatewayError::Unavailable(text),
        ErrorKind::DeadlineExceeded => GatewayError::DeadlineExceeded(text),
        ErrorKind::RateLimited => GatewayError::RateLimited(text),
        ErrorKind::CircuitOpen => GatewayError::CircuitOpen(text),
        ErrorKind::Unauthenticated => GatewayError::Unauthenticated(text),
        ErrorKind::PermissionDenied => GatewayError::PermissionDenied(text),
        ErrorKind::NotFound => GatewayError::NotFound(text),
        ErrorKind::InvalidArgument => GatewayError::InvalidArgument(text),
        ErrorKind::Internal => GatewayError::Internal(text),
    }
}

#[cfg(test)]
#[path = "upstream_tests.rs"]
mod tests;

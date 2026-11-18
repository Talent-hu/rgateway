//! 过滤器共用的小工具：请求上下文 / Content-Length / gRPC 状态读取。

use std::sync::Arc;

use rgateway_core::{GatewayError, ProxyBody, RequestCtx};

/// 从 request extensions 取出注入的 `Arc<RequestCtx>`。
///
/// 缺失视为内部错误（分发层必然注入，缺失即 bug）：错误文本是内部
/// 细节，只进日志，客户端看到的是泛化的 "internal error"。
pub(crate) fn ctx_of(req: &http::Request<ProxyBody>) -> Result<Arc<RequestCtx>, GatewayError> {
    rgateway_config::target::request_ctx(req)
        .ok_or_else(|| GatewayError::Internal("missing request ctx".into()))
}

/// 取 Content-Length 声明值；头缺失或非法（非数字）返回 `None`
/// （非法声明不预检，交给流式核算兜底）。
pub(crate) fn content_length_of<B>(req: &http::Request<B>) -> Option<usize> {
    req.headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
}

/// 是否 gRPC 家族响应：content-type 前缀 `application/grpc`
/// （覆盖 `application/grpc+proto` 与 grpc-web 变体）。
pub(crate) fn is_grpc_family(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("application/grpc"))
}

/// 读 `grpc-status` 键值（响应头或 trailer 里同名键）；缺失或非数字
/// 返回 `None`。
pub(crate) fn grpc_status_in(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get("grpc-status")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

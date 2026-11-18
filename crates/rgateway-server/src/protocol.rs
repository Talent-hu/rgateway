//! 协议探测与解析：从请求头判定线上协议，并解析路由的协议提示覆写。
//!
//! 分发层在构造 [`rgateway_core::RequestCtx`] 之前调用 [`detect`] 得到探测
//! 协议；路由声明了 `protocol` 提示时再经 [`resolve`] 决定最终协议——
//! 后续的错误渲染（`rgateway-proxy::error_render`）与协议处理都以最终协议
//! 为准。

use rgateway_core::{Protocol, ProtocolHint};

/// 从请求头探测线上协议（content-type → upgrade → HTTP 版本）。
///
/// # 探测顺序（为什么是这个顺序）
///
/// 1. `Content-Type` 头（去分号参数、trim、大小写不敏感）按前缀匹配
///    gRPC 家族——**必须从最具体到最泛**：`grpc-web-text` 是 `grpc-web`
///    的子串、`grpc-web` 是 `grpc` 的子串，顺序颠倒会把 gRPC-Web-Text
///    误判成 gRPC-Web、把 gRPC-Web 误判成原生 gRPC；
/// 2. `Upgrade: websocket`（大小写不敏感）判 WebSocket——普通 gRPC 流量
///    不带 Upgrade 头，放在 content-type 之后不冲突；
/// 3. 其余按 HTTP 版本分派 `Http2` / `Http1`。
pub fn detect(parts: &http::request::Parts) -> Protocol {
    if let Some(mime) = media_type(parts) {
        // 前缀匹配顺序即「最具体到最泛」，见函数文档。
        if mime.starts_with("application/grpc-web-text") {
            return Protocol::GrpcWebText;
        }
        if mime.starts_with("application/grpc-web") {
            return Protocol::GrpcWeb;
        }
        if mime.starts_with("application/grpc") {
            return Protocol::Grpc;
        }
    }
    if is_websocket_upgrade(parts) {
        return Protocol::WebSocket;
    }
    if parts.version >= http::Version::HTTP_2 {
        return Protocol::Http2;
    }
    Protocol::Http1
}

/// 解析最终协议：`Fixed(p)` 强制 `p`（转码路由固定 `RestJson` 等），
/// `Auto` 采纳探测结果。
pub fn resolve(hint: ProtocolHint, detected: Protocol) -> Protocol {
    match hint {
        ProtocolHint::Fixed(p) => p,
        ProtocolHint::Auto => detected,
    }
}

/// 取 `Content-Type` 的媒体类型部分：去分号参数、trim、转小写。
fn media_type(parts: &http::request::Parts) -> Option<String> {
    let raw = parts
        .headers
        .get(http::header::CONTENT_TYPE)?
        .to_str()
        .ok()?;
    // split 至少产出一段，next 恒为 Some；unwrap_or 兜底避免 unwrap。
    let base = raw.split(';').next().map(str::trim).unwrap_or("");
    Some(base.to_ascii_lowercase())
}

/// `Upgrade` 头是否声明 `websocket`（大小写不敏感，容忍前后空白）。
fn is_websocket_upgrade(parts: &http::request::Parts) -> bool {
    parts
        .headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;

//! 协议感知的错误渲染：把 [`GatewayError`] 渲染成对应协议的错误响应。
//!
//! # 分派矩阵（每格的理由）
//!
//! | 协议 | 表达方式 | 为什么 |
//! |------|----------|--------|
//! | `Http1` / `Http2` / `WebSocket` / `RestJson` | HTTP status + JSON body | 没有 gRPC 状态通道，错误只能占 HTTP status；WebSocket 在握手失败阶段仍是普通 HTTP 响应；RestJson 面向 REST 客户端，按 HTTP 语义表达 |
//! | `Grpc` | 200 OK + `grpc-status` / `grpc-message` 头（trailers-only） | gRPC 家族的错误表达走 grpc-status，**不占** HTTP status——映射表的 HTTP 列用于 HTTP 渲染、grpc 列用于 gRPC 渲染 |
//! | `GrpcWeb` | 200 OK + body 末尾二进制 trailer 帧 | gRPC-Web 无原生 trailer 通道，trailer 编码成帧（见 [`crate::framing`]） |
//! | `GrpcWebText` | 200 OK + base64 文本帧 | 同上，文本变体整帧 base64 |
//!
//! 状态码与文案**一律**取自 [`GatewayError`] 的方法（`http_status()` /
//! `grpc_status()` / `public_message()` / `grpc_message()`），本模块不自建任何
//! 映射表——唯一权威是 `rgateway-core/src/error.rs`，新变体在那边登记后这里
//! 自动生效。
//!
//! # 双通道红线
//!
//! 客户端可见内容只允许 `public_message()` / `grpc_message()`（泛化静态文案）；
//! 变体携带的内部细节（上游地址、实例 id、io 错误原文）只走 `Display`/`Debug`
//! 进日志。测试用 `SECRET_DETAIL` 载荷对响应头与响应体做泄露检测。

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Response, StatusCode};

use rgateway_core::{empty_body, full_body, GatewayError, Protocol, ProxyBody};

use crate::framing::{encode_trailer_frame, encode_trailer_frame_text};
use crate::headers::header_value;

/// `grpc-status` 头名。
const GRPC_STATUS: HeaderName = HeaderName::from_static("grpc-status");
/// `grpc-message` 头名。
const GRPC_MESSAGE: HeaderName = HeaderName::from_static("grpc-message");

/// 按协议把网关错误渲染成响应，分派矩阵见模块文档。
pub fn render(protocol: Protocol, err: &GatewayError) -> Response<ProxyBody> {
    match protocol {
        Protocol::Http1 | Protocol::Http2 | Protocol::WebSocket | Protocol::RestJson => {
            render_http(err)
        }
        Protocol::Grpc => render_grpc(err),
        Protocol::GrpcWeb => render_grpc_web(err),
        Protocol::GrpcWebText => render_grpc_web_text(err),
    }
}

/// HTTP 家族渲染：`http_status()` + 固定形状 JSON body `{"error": "<文案>"}`。
fn render_http(err: &GatewayError) -> Response<ProxyBody> {
    let body = serde_json::json!({ "error": err.public_message() }).to_string();
    let mut resp = Response::new(full_body(Bytes::from(body)));
    *resp.status_mut() = err.http_status();
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// 原生 gRPC 渲染：trailers-only——错误语义全部走 `grpc-status` /
/// `grpc-message` 头，HTTP status 固定 200，body 为空。
fn render_grpc(err: &GatewayError) -> Response<ProxyBody> {
    let mut resp = grpc_response("application/grpc", empty_body());
    let headers = resp.headers_mut();
    headers.insert(GRPC_STATUS, header_value(&err.grpc_status().to_string()));
    headers.insert(GRPC_MESSAGE, header_value(&err.grpc_message()));
    resp
}

/// gRPC-Web 渲染：200 + body 末尾的二进制 trailer 帧。
fn render_grpc_web(err: &GatewayError) -> Response<ProxyBody> {
    let frame = encode_trailer_frame(err.grpc_status(), &err.grpc_message());
    grpc_response("application/grpc-web+proto", full_body(frame))
}

/// gRPC-Web-Text 渲染：200 + 整帧 base64 的文本 trailer 帧。
fn render_grpc_web_text(err: &GatewayError) -> Response<ProxyBody> {
    let text = encode_trailer_frame_text(err.grpc_status(), &err.grpc_message());
    grpc_response(
        "application/grpc-web-text+proto",
        full_body(Bytes::from(text)),
    )
}

/// gRPC 家族共用骨架：200 OK + 指定 content-type（错误语义在头或帧里表达）。
fn grpc_response(content_type: &'static str, body: ProxyBody) -> Response<ProxyBody> {
    let mut resp = Response::new(body);
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    resp
}

#[cfg(test)]
#[path = "error_render_tests.rs"]
mod tests;

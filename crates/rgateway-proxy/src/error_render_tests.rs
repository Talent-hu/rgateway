//! [`crate::error_render`] 的测试：九变体 × 七协议的完整分派矩阵、
//! `SECRET_DETAIL` 泄露检测、JSON body 形状精确断言、gRPC-Web golden 帧比对。

use http::{HeaderMap, Response, StatusCode};
use http_body_util::BodyExt;
use rgateway_core::{GatewayError, Protocol, ProxyBody};

use super::render;
use crate::framing::decode_trailer_frame;

/// 泄露检测哨兵：伪装成「内部细节」写进错误载荷，任何客户端可见通道
/// （响应头、响应体）都不允许出现它。
const SECRET_DETAIL: &str = "SECRET_DETAIL";

/// Unavailable 的 golden trailer 帧（与 framing_tests 的常量同一字节序列）。
const GOLDEN_UNAVAILABLE_FRAME: &[u8] =
    b"\x80\x00\x00\x00\x35grpc-status: 14\r\ngrpc-message: upstream unavailable\r\n";

/// [`GOLDEN_UNAVAILABLE_FRAME`] 的 gRPC-Web-Text 变体（整帧 base64）。
const GOLDEN_UNAVAILABLE_B64: &str =
    "gAAAADVncnBjLXN0YXR1czogMTQNCmdycGMtbWVzc2FnZTogdXBzdHJlYW0gdW5hdmFpbGFibGUNCg==";

// 列含义：(期望 HTTP status, 期望 grpc-status, 期望 JSON 文案, 期望 grpc-message 文案)
// 顺序与 all_errors() 一一对应，数值与 rgateway-core/src/error.rs 映射总表一致
// （含 CircuitOpen 唯一允许的措辞差异）。
const EXPECTED: [(u16, i32, &str, &str); 9] = [
    (503, 14, "upstream unavailable", "upstream unavailable"),
    (504, 4, "deadline exceeded", "deadline exceeded"),
    (429, 8, "rate limit exceeded", "rate limit exceeded"),
    (
        503,
        14,
        "circuit breaker open",
        "circuit breaker open: circuit is open",
    ),
    (401, 16, "unauthenticated", "unauthenticated"),
    (403, 7, "permission denied", "permission denied"),
    (404, 12, "not found", "not found"),
    (400, 3, "invalid argument", "invalid argument"),
    (500, 13, "internal error", "internal error"),
];

/// 覆盖全部九个变体的错误样本，载荷统一为 SECRET_DETAIL。
fn all_errors() -> Vec<GatewayError> {
    vec![
        GatewayError::Unavailable(SECRET_DETAIL.into()),
        GatewayError::DeadlineExceeded(SECRET_DETAIL.into()),
        GatewayError::RateLimited(SECRET_DETAIL.into()),
        GatewayError::CircuitOpen(SECRET_DETAIL.into()),
        GatewayError::Unauthenticated(SECRET_DETAIL.into()),
        GatewayError::PermissionDenied(SECRET_DETAIL.into()),
        GatewayError::NotFound(SECRET_DETAIL.into()),
        GatewayError::InvalidArgument(SECRET_DETAIL.into()),
        GatewayError::Internal(SECRET_DETAIL.into()),
    ]
}

/// 取出响应体字节（消费响应；测试辅助）。
async fn body_bytes(resp: Response<ProxyBody>) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

/// 双通道契约的泄露检测：响应头与响应体都不允许含 SECRET_DETAIL。
fn assert_no_leak(headers: &HeaderMap, body: &[u8]) {
    for (name, value) in headers.iter() {
        let text = value.to_str().unwrap_or("<non-utf8>");
        assert!(
            !text.contains(SECRET_DETAIL),
            "头 {name} 泄露内部细节: {text}"
        );
    }
    let text = String::from_utf8_lossy(body);
    assert!(!text.contains(SECRET_DETAIL), "响应体泄露内部细节: {text}");
}

/// 取头值文本（测试辅助；缺失或非 UTF-8 直接 panic，测试必须看到失败）。
fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .expect("缺少期望的响应头")
        .to_str()
        .expect("响应头不是合法 UTF-8")
}

// HTTP 家族（Http1/Http2/WebSocket/RestJson）矩阵：状态码取 http_status()，
// Content-Type 为 application/json，body 形状精确为 {"error":"<文案>"}，
// 并做泄露检测。
#[tokio::test]
async fn http_family_renders_status_and_exact_json_body() {
    let protocols = [
        Protocol::Http1,
        Protocol::Http2,
        Protocol::WebSocket,
        Protocol::RestJson,
    ];
    for protocol in protocols {
        for (err, (want_status, _, want_public, _)) in all_errors().into_iter().zip(EXPECTED) {
            let resp = render(protocol, &err);
            let label = protocol.as_label();
            assert_eq!(resp.status().as_u16(), want_status, "{label}");
            assert_eq!(
                header_text(resp.headers(), "content-type"),
                "application/json",
                "{label}"
            );
            let headers = resp.headers().clone();
            let body = body_bytes(resp).await;
            assert_eq!(
                body,
                format!("{{\"error\":\"{want_public}\"}}").as_bytes(),
                "{label} JSON 形状必须精确"
            );
            assert_no_leak(&headers, &body);
        }
    }
}

// 原生 gRPC 矩阵：trailers-only 语义——200 OK、application/grpc、
// grpc-status / grpc-message 头、空 body，逐变体断言并做泄露检测。
#[tokio::test]
async fn grpc_renders_trailers_only() {
    for (err, (_, want_grpc, _, want_msg)) in all_errors().into_iter().zip(EXPECTED) {
        let resp = render(Protocol::Grpc, &err);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            header_text(resp.headers(), "content-type"),
            "application/grpc"
        );
        assert_eq!(
            header_text(resp.headers(), "grpc-status"),
            want_grpc.to_string()
        );
        assert_eq!(header_text(resp.headers(), "grpc-message"), want_msg);
        let headers = resp.headers().clone();
        let body = body_bytes(resp).await;
        assert!(body.is_empty(), "trailers-only 应当为空 body");
        assert_no_leak(&headers, &body);
    }
}

// gRPC-Web 矩阵：200 + application/grpc-web+proto，body 是二进制 trailer 帧，
// 帧内 status/message 与期望一致；Unavailable 的 body 与 golden 帧逐字节相等。
#[tokio::test]
async fn grpc_web_renders_trailer_frame_body() {
    for (err, (_, want_grpc, _, want_msg)) in all_errors().into_iter().zip(EXPECTED) {
        let resp = render(Protocol::GrpcWeb, &err);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            header_text(resp.headers(), "content-type"),
            "application/grpc-web+proto"
        );
        let headers = resp.headers().clone();
        let body = body_bytes(resp).await;
        assert_no_leak(&headers, &body);
        assert_eq!(
            decode_trailer_frame(&body).unwrap(),
            (want_grpc, want_msg.to_string()),
            "帧内 status/message 必须取自 grpc_status()/grpc_message()"
        );
    }
    let resp = render(
        Protocol::GrpcWeb,
        &GatewayError::Unavailable(SECRET_DETAIL.into()),
    );
    let body = body_bytes(resp).await;
    assert_eq!(
        body, GOLDEN_UNAVAILABLE_FRAME,
        "grpc-web body 与 golden 帧字节相等"
    );
}

// gRPC-Web-Text 矩阵：200 + application/grpc-web-text+proto，body 是整帧
// base64 文本；Unavailable 的 body 与 base64 golden 完全相等。
#[tokio::test]
async fn grpc_web_text_renders_base64_frame_body() {
    use base64::Engine;
    for (err, (_, want_grpc, _, want_msg)) in all_errors().into_iter().zip(EXPECTED) {
        let resp = render(Protocol::GrpcWebText, &err);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            header_text(resp.headers(), "content-type"),
            "application/grpc-web-text+proto"
        );
        let headers = resp.headers().clone();
        let body = body_bytes(resp).await;
        assert_no_leak(&headers, &body);
        let text = String::from_utf8(body).unwrap();
        let frame = base64::engine::general_purpose::STANDARD
            .decode(&text)
            .unwrap();
        assert_eq!(
            decode_trailer_frame(&frame).unwrap(),
            (want_grpc, want_msg.to_string())
        );
    }
    let resp = render(
        Protocol::GrpcWebText,
        &GatewayError::Unavailable(SECRET_DETAIL.into()),
    );
    let body = body_bytes(resp).await;
    assert_eq!(
        String::from_utf8(body).unwrap(),
        GOLDEN_UNAVAILABLE_B64,
        "grpc-web-text body 与 base64 golden 完全相等"
    );
}

//! [`crate::protocol`] 的测试：`detect` 的全矩阵探测（content-type 参数、
//! gRPC-Web-Text 子串歧义、Upgrade 大小写、HTTP/2 版本）与 `resolve` 的
//! hint 覆写语义。

use http::request::Parts;

use rgateway_core::{Protocol, ProtocolHint};

use super::{detect, resolve};

/// 用给定 HTTP 版本与可选头构造请求 Parts（测试辅助）。
fn parts_with(version: http::Version, content_type: Option<&str>, upgrade: Option<&str>) -> Parts {
    let mut builder = http::Request::builder()
        .method("POST")
        .uri("/pkg.Svc/Method")
        .version(version);
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    if let Some(up) = upgrade {
        builder = builder.header("upgrade", up);
    }
    builder
        .body(())
        .expect("test request builds")
        .into_parts()
        .0
}

// `detect` 全矩阵。列含义：(HTTP 版本, Content-Type 头值, Upgrade 头值, 期望协议)
#[test]
fn detect_matrix() {
    let cases = [
        // 无 grpc content-type：按版本分派
        (http::Version::HTTP_11, None, None, Protocol::Http1),
        (http::Version::HTTP_2, None, None, Protocol::Http2),
        // 原生 gRPC：含带分号参数、+proto 后缀、大小写与前后空白
        (
            http::Version::HTTP_11,
            Some("application/grpc"),
            None,
            Protocol::Grpc,
        ),
        (
            http::Version::HTTP_11,
            Some("application/grpc; charset=utf-8"),
            None,
            Protocol::Grpc,
        ),
        (
            http::Version::HTTP_11,
            Some("application/grpc+proto"),
            None,
            Protocol::Grpc,
        ),
        (
            http::Version::HTTP_2,
            Some("application/grpc"),
            None,
            Protocol::Grpc,
        ),
        (
            http::Version::HTTP_11,
            Some("  application/grpc ; charset=utf-8"),
            None,
            Protocol::Grpc,
        ),
        // gRPC-Web：必须先于 grpc 前缀命中（grpc-web 是 grpc 的前缀超集）
        (
            http::Version::HTTP_11,
            Some("application/grpc-web"),
            None,
            Protocol::GrpcWeb,
        ),
        (
            http::Version::HTTP_11,
            Some("application/grpc-web+proto"),
            None,
            Protocol::GrpcWeb,
        ),
        // gRPC-Web-Text：必须先于 grpc-web 前缀命中（text 变体是 grpc-web 的超集）
        (
            http::Version::HTTP_11,
            Some("application/grpc-web-text"),
            None,
            Protocol::GrpcWebText,
        ),
        (
            http::Version::HTTP_11,
            Some("application/grpc-web-text+proto"),
            None,
            Protocol::GrpcWebText,
        ),
        (
            http::Version::HTTP_2,
            Some("APPLICATION/GRPC-WEB-TEXT+PROTO"),
            None,
            Protocol::GrpcWebText,
        ),
        // WebSocket：Upgrade 头大小写不敏感
        (
            http::Version::HTTP_11,
            None,
            Some("websocket"),
            Protocol::WebSocket,
        ),
        (
            http::Version::HTTP_11,
            None,
            Some("WebSocket"),
            Protocol::WebSocket,
        ),
        (
            http::Version::HTTP_11,
            Some("text/plain"),
            Some("websocket"),
            Protocol::WebSocket,
        ),
        // 其他 content-type / 非 websocket 升级不改变版本分派
        (
            http::Version::HTTP_2,
            Some("text/plain"),
            None,
            Protocol::Http2,
        ),
        (
            http::Version::HTTP_11,
            Some("text/plain"),
            Some("h2c"),
            Protocol::Http1,
        ),
    ];
    for (version, content_type, upgrade, want) in cases {
        let parts = parts_with(version, content_type, upgrade);
        let got = detect(&parts);
        assert_eq!(
            got, want,
            "version={version:?} content-type={content_type:?} upgrade={upgrade:?}"
        );
    }
}

// `resolve` 矩阵：Fixed 覆写探测结果，Auto 采纳探测结果。
// 列含义：(路由 hint, 探测协议, 期望最终协议)
#[test]
fn resolve_matrix() {
    let cases = [
        (ProtocolHint::Auto, Protocol::Grpc, Protocol::Grpc),
        (ProtocolHint::Auto, Protocol::Http1, Protocol::Http1),
        (
            ProtocolHint::Fixed(Protocol::Http1),
            Protocol::Grpc,
            Protocol::Http1,
        ),
        (
            ProtocolHint::Fixed(Protocol::RestJson),
            Protocol::Http2,
            Protocol::RestJson,
        ),
        (
            ProtocolHint::Fixed(Protocol::GrpcWeb),
            Protocol::Http1,
            Protocol::GrpcWeb,
        ),
    ];
    for (hint, detected, want) in cases {
        assert_eq!(
            resolve(hint, detected),
            want,
            "hint={hint:?} detected={detected:?}"
        );
    }
}

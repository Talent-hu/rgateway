//! 准入闸矩阵端到端：协议闸（gRPC / gRPC-Web / gRPC-Web-Text / WebSocket /
//! rest-json 固定提示）与 scheme 闸（https 直连路由）各自被拒绝时的
//! Unavailable 渲染形态、上游绝不被触达、客户端通道不含内部细节。
//!
//! 断言的生产代码：`handler` 的 `gate_scheme` / `gate_protocol`（准入闸，
//! 错误双通道——内部措辞只进日志）、`protocol` 探测与 hint 覆写、
//! `error_render` 分派矩阵、`framing` trailer 帧 golden、
//! `GatewayError` 映射（唯一权威）。

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;

/// gRPC-Web trailer 帧 golden（Unavailable：grpc-status 14 / "upstream
/// unavailable"）：手工推导字节 = `0x80`（trailer 标志）+ 4 字节大端长度
/// 53（0x35）+ payload `"grpc-status: 14\r\ngrpc-message: upstream
/// unavailable\r\n"`（17 + 36 字节）。
const GOLDEN_UNAVAILABLE: &[u8] =
    b"\x80\x00\x00\x00\x35grpc-status: 14\r\ngrpc-message: upstream unavailable\r\n";

/// 上述 golden 帧整体 base64（STANDARD 字母表，含 padding）——
/// gRPC-Web-Text 变体的期望响应体。
const GOLDEN_UNAVAILABLE_B64: &str =
    "gAAAADVncnBjLXN0YXR1czogMTQNCmdycGMtbWVzc2FnZTogdXBzdHJlYW0gdW5hdmFpbGFibGUNCg==";

/// Unavailable 的期望渲染形态（error_render 分派矩阵的四种格子）。
#[derive(Clone, Copy)]
enum Expect {
    /// HTTP 族（WebSocket / RestJson）：503 + JSON body。
    HttpJson,
    /// 原生 gRPC：200 + `grpc-status` / `grpc-message` 头（trailers-only）。
    GrpcTrailers,
    /// gRPC-Web：200 + 二进制 trailer 帧（golden 字节）。
    GrpcWebFrame,
    /// gRPC-Web-Text：200 + base64 文本帧（golden）。
    GrpcWebTextFrame,
}

/// 一个闸门用例：路由形态 + 请求头 + 期望渲染。
struct GateCase {
    /// 用例标签（断言失败定位）。
    label: &'static str,
    /// 路由 protocol 提示（`None` = "auto"）。
    protocol: Option<&'static str>,
    /// 目标是否为 `https://` 直连（scheme 闸用例）。
    https_target: bool,
    /// 请求头逐行原样发出。
    headers: &'static [(&'static str, &'static str)],
    /// 期望渲染形态。
    expect: Expect,
}

/// 协议闸 + scheme 闸全表。
/// 列含义：(标签, protocol 提示, https 目标, 请求头, 期望渲染)——
/// 五种协议形态各自落回 Unavailable 的对应渲染格子，https 直连走 scheme 闸。
const CASES: &[GateCase] = &[
    GateCase {
        label: "grpc content-type",
        protocol: None,
        https_target: false,
        headers: &[
            ("Host", "example.test"),
            ("Content-Type", "application/grpc"),
        ],
        expect: Expect::GrpcTrailers,
    },
    GateCase {
        label: "grpc-web content-type",
        protocol: None,
        https_target: false,
        headers: &[
            ("Host", "example.test"),
            ("Content-Type", "application/grpc-web"),
        ],
        expect: Expect::GrpcWebFrame,
    },
    GateCase {
        label: "grpc-web-text content-type",
        protocol: None,
        https_target: false,
        headers: &[
            ("Host", "example.test"),
            ("Content-Type", "application/grpc-web-text"),
        ],
        expect: Expect::GrpcWebTextFrame,
    },
    GateCase {
        label: "websocket upgrade",
        protocol: None,
        https_target: false,
        headers: &[("Host", "example.test"), ("Upgrade", "websocket")],
        expect: Expect::HttpJson,
    },
    GateCase {
        label: "rest-json fixed hint",
        protocol: Some("rest-json"),
        https_target: false,
        headers: &[("Host", "example.test")],
        expect: Expect::HttpJson,
    },
    GateCase {
        label: "https direct route",
        protocol: None,
        https_target: true,
        headers: &[("Host", "example.test")],
        expect: Expect::HttpJson,
    },
];

/// 跑一个闸门用例：起计数上游 + 网关，发请求，断言渲染形态、
/// 上游未被触达与泄露检测。
async fn run_case(case: &GateCase) {
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = start_upstream(Behavior::Counting {
        hits: Arc::clone(&hits),
        delay_millis: 0,
        body: "must-not-be-served".to_string(),
    })
    .await;
    let scheme = if case.https_target { "https" } else { "http" };
    let target = format!("{scheme}://127.0.0.1:{}", upstream.port);
    let entry = match case.protocol {
        Some(protocol) => route_entry_with_protocol("api", "/api/**", &target, protocol),
        None => route_entry("api", "/api/**", &target, None, None),
    };
    let dir = TestDir::new();
    let gw = start(dir.path(), &config_with_routes(&entry)).await;

    let resp = request(gw.addr, "GET", "/api/echo", case.headers, BodySpec::Empty).await;

    assert_render(case, &resp);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "{}: upstream must never be touched",
        case.label
    );
    // 泄露检测：闸门内部措辞（协议名、"planned for a later phase"、
    // 上游地址）与 SECRET_DETAIL 哨兵绝不进客户端可见通道
    assert_no_leak(
        &resp,
        &[
            SECRET_DETAIL,
            "planned for a later phase",
            "https upstream",
            "127.0.0.1",
            &format!(":{}", upstream.port),
        ],
    );
}

/// 按期望渲染形态分派到对应格子的断言。
fn assert_render(case: &GateCase, resp: &common::RawResponse) {
    match case.expect {
        Expect::HttpJson => assert_http_json(case.label, resp),
        Expect::GrpcTrailers => assert_grpc_trailers(case.label, resp),
        Expect::GrpcWebFrame => assert_grpc_web_frame(case.label, resp),
        Expect::GrpcWebTextFrame => assert_grpc_web_text_frame(case.label, resp),
    }
}

/// HTTP 族格子：503 + JSON body。
fn assert_http_json(label: &str, resp: &common::RawResponse) {
    assert_eq!(resp.status, 503, "{label}");
    assert_eq!(
        resp.header("content-type"),
        Some("application/json"),
        "{label}"
    );
    assert_eq!(
        resp.body_text(),
        "{\"error\":\"upstream unavailable\"}",
        "{label}"
    );
}

/// 原生 gRPC 格子：200 + trailers-only 头。
fn assert_grpc_trailers(label: &str, resp: &common::RawResponse) {
    assert_eq!(resp.status, 200, "{label}");
    assert_eq!(
        resp.header("content-type"),
        Some("application/grpc"),
        "{label}"
    );
    assert_eq!(resp.header("grpc-status"), Some("14"), "{label}");
    assert_eq!(
        resp.header("grpc-message"),
        Some("upstream unavailable"),
        "{label}"
    );
    assert!(resp.body.is_empty(), "{label}: trailers-only");
}

/// gRPC-Web 格子：200 + 二进制 trailer 帧（golden 字节）。
fn assert_grpc_web_frame(label: &str, resp: &common::RawResponse) {
    assert_eq!(resp.status, 200, "{label}");
    assert_eq!(
        resp.header("content-type"),
        Some("application/grpc-web+proto"),
        "{label}"
    );
    assert_eq!(
        resp.body, GOLDEN_UNAVAILABLE,
        "{label}: trailer frame must match golden bytes"
    );
}

/// gRPC-Web-Text 格子：200 + base64 文本帧（golden）。
fn assert_grpc_web_text_frame(label: &str, resp: &common::RawResponse) {
    assert_eq!(resp.status, 200, "{label}");
    assert_eq!(
        resp.header("content-type"),
        Some("application/grpc-web-text+proto"),
        "{label}"
    );
    assert_eq!(
        resp.body_text(),
        GOLDEN_UNAVAILABLE_B64,
        "{label}: text frame must match golden base64"
    );
}

/// 协议闸与 scheme 闸矩阵：每个用例独立起上游与网关，循环断言
/// 渲染形态 + 上游未触达 + 无泄露。
#[tokio::test]
async fn gates_render_unavailable_without_touching_upstream() {
    // golden 与生产编解码器互证：同一字节序列还原出 (14, "upstream unavailable")
    let (status, message) =
        rgateway_proxy::decode_trailer_frame(GOLDEN_UNAVAILABLE).expect("decode golden");
    assert_eq!((status, message.as_str()), (14, "upstream unavailable"));

    for case in CASES {
        run_case(case).await;
    }
}

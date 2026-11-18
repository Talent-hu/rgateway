//! 协议感知错误渲染端到端：无路由 404（HTTP / gRPC / gRPC-Web / 文本变体）、
//! 上游不可达 503 泄露检测、截止期 504、请求体超限 400（Content-Length
//! 预检与分块流限长两条路径）。
//!
//! 断言的生产代码：`dispatch`（协议探测 / 限长 / 渲染分派）、
//! `error_render`（分派矩阵）、`framing`（trailer 帧 golden）、
//! `GatewayError` 的状态码与文案映射（唯一权威）；转发走生产
//! `rgateway-proxy` 的 handler / upstream（`bootstrap::start` 装配）。

mod common;

use common::*;

/// gRPC-Web trailer 帧 golden（NotFound：grpc-status 12 / "not found"）：
/// 手工推导字节 = `0x80`（trailer 标志）+ 4 字节大端长度 42（0x2a）+
/// payload `"grpc-status: 12\r\ngrpc-message: not found\r\n"`。
const GOLDEN_TRAILER: &[u8] = b"\x80\x00\x00\x00\x2agrpc-status: 12\r\ngrpc-message: not found\r\n";

/// 上述 golden 帧整体 base64（STANDARD 字母表，含 padding）——
/// gRPC-Web-Text 变体的期望响应体。
const GOLDEN_TRAILER_B64: &str = "gAAAACpncnBjLXN0YXR1czogMTINCmdycGMtbWVzc2FnZTogbm90IGZvdW5kDQo=";

/// 起一个只配置 `/api/**` 路由的网关（其余请求落无路由）。
async fn gateway_without_match(dir: &TestDir) -> TestGateway {
    let upstream = start_upstream(Behavior::Fixed {
        status: 200,
        body: "unused".to_string(),
    })
    .await;
    let yaml = config_with_routes(&route_entry(
        "api",
        "/api/**",
        &format!("http://127.0.0.1:{}", upstream.port),
        None,
        None,
    ));
    start(dir.path(), &yaml).await
}

/// 无路由的普通 GET：404 + 固定形状 JSON + X-Request-ID。
#[tokio::test]
async fn no_route_renders_404_json() {
    let dir = TestDir::new();
    let gw = gateway_without_match(&dir).await;

    let resp = request(
        gw.addr,
        "GET",
        "/nope",
        &[("Host", "example.test")],
        BodySpec::Empty,
    )
    .await;

    assert_eq!(resp.status, 404);
    assert_eq!(resp.header("content-type"), Some("application/json"));
    assert_eq!(resp.body_text(), "{\"error\":\"not found\"}");
    let id = resp.header("x-request-id").expect("x-request-id required");
    assert!(
        uuid::Uuid::parse_str(id).is_ok(),
        "x-request-id must be uuid: {id}"
    );
}

/// `Content-Type: application/grpc` 的无路由请求：trailers-only——
/// 200 + `grpc-status: 12` + `grpc-message: not found`，body 空。
#[tokio::test]
async fn grpc_no_route_renders_trailers_only() {
    let dir = TestDir::new();
    let gw = gateway_without_match(&dir).await;

    let resp = request(
        gw.addr,
        "GET",
        "/nope",
        &[
            ("Host", "example.test"),
            ("Content-Type", "application/grpc"),
        ],
        BodySpec::Empty,
    )
    .await;

    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-type"), Some("application/grpc"));
    assert_eq!(resp.header("grpc-status"), Some("12"));
    assert_eq!(resp.header("grpc-message"), Some("not found"));
    assert!(resp.body.is_empty(), "trailers-only must have empty body");
}

/// `application/grpc-web+proto` 无路由：响应体必须逐字节等于 golden trailer 帧。
#[tokio::test]
async fn grpc_web_no_route_renders_golden_trailer_frame() {
    let dir = TestDir::new();
    let gw = gateway_without_match(&dir).await;

    let resp = request(
        gw.addr,
        "GET",
        "/nope",
        &[
            ("Host", "example.test"),
            ("Content-Type", "application/grpc-web+proto"),
        ],
        BodySpec::Empty,
    )
    .await;

    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("content-type"),
        Some("application/grpc-web+proto")
    );
    assert_eq!(
        resp.body, GOLDEN_TRAILER,
        "trailer frame must match golden bytes"
    );
    // golden 与生产解码器互证：同一字节序列还原出 (12, "not found")
    let (status, message) =
        rgateway_proxy::decode_trailer_frame(GOLDEN_TRAILER).expect("decode golden");
    assert_eq!((status, message.as_str()), (12, "not found"));
}

/// `application/grpc-web-text+proto` 无路由：响应体是 golden 帧的 base64 文本。
#[tokio::test]
async fn grpc_web_text_no_route_renders_base64_frame() {
    let dir = TestDir::new();
    let gw = gateway_without_match(&dir).await;

    let resp = request(
        gw.addr,
        "GET",
        "/nope",
        &[
            ("Host", "example.test"),
            ("Content-Type", "application/grpc-web-text+proto"),
        ],
        BodySpec::Empty,
    )
    .await;

    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("content-type"),
        Some("application/grpc-web-text+proto")
    );
    assert_eq!(
        resp.body_text(),
        GOLDEN_TRAILER_B64,
        "text frame must match golden base64"
    );
}

/// 上游连接被拒：503 + 泛化文案；客户端通道绝不含上游地址 / 端口 /
/// `SECRET_DETAIL`（错误双通道红线）。
#[tokio::test]
async fn upstream_refused_renders_503_without_leak() {
    // 取一个立刻释放的端口：连接必然被拒（connection refused）
    let closed_port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    };
    let dir = TestDir::new();
    let yaml = config_with_routes(&route_entry(
        "api",
        "/api/**",
        &format!("http://127.0.0.1:{closed_port}"),
        None,
        None,
    ));
    let gw = start(dir.path(), &yaml).await;

    let resp = request(
        gw.addr,
        "GET",
        "/api/echo",
        &[("Host", "example.test")],
        BodySpec::Empty,
    )
    .await;

    assert_eq!(resp.status, 503);
    assert_eq!(resp.body_text(), "{\"error\":\"upstream unavailable\"}");
    // 泄露检测：内部细节（生产的 Unavailable 内部消息带上游地址与底层
    // 错误原文）不得外泄到客户端可见通道
    assert_no_leak(
        &resp,
        &[
            SECRET_DETAIL,
            "127.0.0.1",
            &format!(":{closed_port}"),
            "upstream request failed",
            "Connect",
        ],
    );
}

/// 上游慢于路由截止预算：504 + 泛化文案；内部消息里的路由 id 不外泄
/// （真实时间：网络 I/O 用例）。
#[tokio::test]
async fn slow_upstream_renders_504_deadline_exceeded() {
    let upstream = start_upstream(Behavior::Sleep {
        millis: 2000,
        body: "too late".to_string(),
    })
    .await;
    let dir = TestDir::new();
    let yaml = config_with_routes(&route_entry(
        "slow-route",
        "/api/**",
        &format!("http://127.0.0.1:{}", upstream.port),
        None,
        Some("200ms"),
    ));
    let gw = start(dir.path(), &yaml).await;

    let resp = request(
        gw.addr,
        "GET",
        "/api/slow",
        &[("Host", "example.test")],
        BodySpec::Empty,
    )
    .await;

    assert_eq!(resp.status, 504);
    assert_eq!(resp.body_text(), "{\"error\":\"deadline exceeded\"}");
    // 内部消息是 "total deadline exceeded: slow-route"——路由 id 不得外泄
    assert_no_leak(&resp, &["slow-route", "total deadline", SECRET_DETAIL]);
}

/// Content-Length 预检超限：400 + `invalid argument`（不读 body 直接拒）。
#[tokio::test]
async fn content_length_over_limit_renders_400() {
    let upstream = start_upstream(Behavior::EchoBody).await;
    let dir = TestDir::new();
    let yaml = config_with_limits(
        &route_entry(
            "api",
            "/api/**",
            &format!("http://127.0.0.1:{}", upstream.port),
            None,
            None,
        ),
        "1KB",
    );
    let gw = start(dir.path(), &yaml).await;

    let resp = request(
        gw.addr,
        "POST",
        "/api/echo",
        &[("Host", "example.test")],
        BodySpec::Bytes(vec![b'a'; 4096]),
    )
    .await;

    assert_eq!(resp.status, 400);
    assert_eq!(resp.body_text(), "{\"error\":\"invalid argument\"}");
}

/// 分块（无 Content-Length）超大请求体：流经 `Limited` 时
/// `LengthLimitError` 被生产 `limited_body` 归一为 `InvalidArgument`，
/// 请求体错误随转发（hyper user body 错误）冒泡，生产的错误还原
/// （`ProxyError::into_gateway_error`）归一后由链返回、渲染 400。
#[tokio::test]
async fn chunked_over_limit_renders_400() {
    let upstream = start_upstream(Behavior::EchoBody).await;
    let dir = TestDir::new();
    let yaml = config_with_limits(
        &route_entry(
            "api",
            "/api/**",
            &format!("http://127.0.0.1:{}", upstream.port),
            None,
            None,
        ),
        "1KB",
    );
    let gw = start(dir.path(), &yaml).await;

    // 4 个 1KB chunk，总量 4KB > 1KB 上限
    let resp = request(
        gw.addr,
        "POST",
        "/api/echo",
        &[("Host", "example.test")],
        BodySpec::Chunked(vec![vec![b'b'; 1024]; 4]),
    )
    .await;

    assert_eq!(resp.status, 400);
    assert_eq!(resp.body_text(), "{\"error\":\"invalid argument\"}");
}

//! [`crate::upstream`] 的测试：真实回环（hyper server 起在 127.0.0.1:0）锁定
//! 转发语义（出站 URI、请求/响应头治理、体流式透传）、出站版本归一为
//! HTTP/1.1、hyper 错误链上的 `ProxyError` 还原与连接拒绝兜底。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, Version};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

use rgateway_core::body::stream_body;
use rgateway_core::{empty_body, full_body, ErrorKind, Protocol, ProxyError, RequestCtx};

use super::UpstreamClient;

/// 泄露检测哨兵：伪装成「内部细节」写进错误载荷，客户端可见通道
/// （`public_message()`）绝不允许出现它。
const SECRET_DETAIL: &str = "SECRET_DETAIL";

/// 上游服务器捕获到的请求快照（断言转发语义用）。
#[derive(Debug)]
struct Captured {
    method: Method,
    path_and_query: String,
    version: Version,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// 取头值文本（缺失直接 panic，测试必须看到失败）。
fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .expect("缺少期望的请求头")
        .to_str()
        .expect("头值不是合法 UTF-8")
}

/// 启动一个回环 echo 服务器：捕获请求快照，固定返回 pong 响应（响应头带
/// hop-by-hop 头与待覆写的 `X-Request-ID`）。返回监听地址与捕获槽。
async fn spawn_echo_server() -> (SocketAddr, Arc<Mutex<Option<Captured>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定回环端口");
    let addr = listener.local_addr().expect("取回环地址");
    let slot = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&slot);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req: Request<Incoming>| {
                    let captured = Arc::clone(&captured);
                    async move {
                        let (parts, body) = req.into_parts();
                        // 请求体收集失败（上游侧断流）时不回 pong，保持确定性。
                        let body = body
                            .collect()
                            .await
                            .map_err(|e| std::io::Error::other(format!("body: {e}")))?
                            .to_bytes()
                            .to_vec();
                        *captured.lock().expect("捕获锁") = Some(Captured {
                            method: parts.method,
                            path_and_query: parts
                                .uri
                                .path_and_query()
                                .map(|pq| pq.as_str().to_string())
                                .unwrap_or_default(),
                            version: parts.version,
                            headers: parts.headers,
                            body,
                        });
                        let mut resp = Response::new(Full::new(Bytes::from_static(b"pong-data")));
                        let headers = resp.headers_mut();
                        headers.insert("x-request-id", "server-old".parse().expect("头值"));
                        headers.insert("x-echo", "1".parse().expect("头值"));
                        headers.insert("keep-alive", "timeout=5".parse().expect("头值"));
                        headers.insert("proxy-connection", "keep-alive".parse().expect("头值"));
                        Ok::<_, std::io::Error>(resp)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
    (addr, slot)
}

/// 「bind 后立即 drop 的端口」：随后 connect 必然被拒（无监听者）。
fn dead_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定临时端口");
    let addr = listener.local_addr().expect("取端口");
    drop(listener);
    addr
}

// 转发全链路语义（真实回环）：出站 URI = http://{target}{path_and_query}；
// 请求头经 prepare_upstream 治理（X-Request-ID / X-Forwarded-* 覆写、XFF
// 追加、Forwarded 与 hop-by-hop 剥离、Host 保留）；请求体流式透传；响应头
// 经 finalize_response（hop-by-hop 剥离、X-Request-ID 覆写、业务头保留）；
// 响应体流式透传。
#[tokio::test]
async fn forward_governs_headers_and_streams_bodies() {
    let (addr, captured) = spawn_echo_server().await;
    let client = UpstreamClient::new(Duration::from_secs(5));
    let ctx = Arc::new(RequestCtx::new(
        Protocol::Http1,
        Some("9.9.9.9:4242".parse().expect("peer 地址")),
    ));

    let req = Request::builder()
        .method(Method::POST)
        .uri("http://gateway.example/ignored")
        .header("host", "public.example")
        .header("x-request-id", "evil-id")
        .header("x-forwarded-host", "evil.example")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "1.1.1.1")
        .header("forwarded", "host=evil.example")
        .header("te", "trailers")
        .header("x-keep", "1")
        .extension(Arc::clone(&ctx))
        .body(full_body(Bytes::from_static(b"hello-upstream")))
        .expect("构造请求");

    let resp = client
        .forward(req, addr, "/echo?x=1")
        .await
        .expect("转发成功");
    assert_eq!(resp.status(), http::StatusCode::OK);
    assert_eq!(
        header_text(resp.headers(), "x-request-id"),
        ctx.request_id.to_string()
    );
    assert_eq!(header_text(resp.headers(), "x-echo"), "1");
    assert!(
        !resp.headers().contains_key("keep-alive"),
        "hop-by-hop 必须剥离"
    );
    assert!(
        !resp.headers().contains_key("proxy-connection"),
        "hop-by-hop 必须剥离"
    );
    let body = resp
        .into_body()
        .collect()
        .await
        .expect("收集响应体")
        .to_bytes();
    assert_eq!(body.as_ref(), b"pong-data", "响应体必须原样透传");

    let got = captured
        .lock()
        .expect("捕获锁")
        .take()
        .expect("上游应捕获请求");
    assert_eq!(got.method, Method::POST);
    assert_eq!(got.path_and_query, "/echo?x=1", "path_and_query 必须透传");
    assert_eq!(got.version, Version::HTTP_11);
    assert_eq!(got.body.as_slice(), b"hello-upstream", "请求体必须原样透传");
    // 列含义（期望值）：关联 id 覆写 / 原始 Host 覆写 / proto 覆写为明文 /
    // XFF 追加链 / Host 保留 / Forwarded 剥离 / hop-by-hop 剥离 / 业务头保留。
    assert_eq!(
        header_text(&got.headers, "x-request-id"),
        ctx.request_id.to_string()
    );
    assert_eq!(
        header_text(&got.headers, "x-forwarded-host"),
        "public.example"
    );
    assert_eq!(header_text(&got.headers, "x-forwarded-proto"), "http");
    assert_eq!(
        header_text(&got.headers, "x-forwarded-for"),
        "1.1.1.1, 9.9.9.9"
    );
    assert_eq!(header_text(&got.headers, "host"), "public.example");
    assert!(!got.headers.contains_key("forwarded"), "Forwarded 必须剥离");
    assert!(!got.headers.contains_key("te"), "hop-by-hop 必须剥离");
    assert_eq!(header_text(&got.headers, "x-keep"), "1");
}

// 出站版本归一：HTTP/2 入站请求转发到 h1 池化连接前必须把 version 归一为
// HTTP/1.1，否则 hyper-util 直接报 UserUnsupportedVersion。锁定：HTTP/2
// 入站转发成功，且上游看到 HTTP/1.1。
#[tokio::test]
async fn forward_normalizes_outbound_version_to_http11() {
    let (addr, captured) = spawn_echo_server().await;
    let client = UpstreamClient::new(Duration::from_secs(5));
    let ctx = Arc::new(RequestCtx::new(Protocol::Http2, None));
    let req = Request::builder()
        .method(Method::GET)
        .uri("http://gateway.example/")
        .version(Version::HTTP_2)
        .extension(Arc::clone(&ctx))
        .body(empty_body())
        .expect("构造请求");
    let resp = client
        .forward(req, addr, "/v2")
        .await
        .expect("HTTP/2 入站必须能转发");
    assert_eq!(resp.status(), http::StatusCode::OK);
    let got = captured
        .lock()
        .expect("捕获锁")
        .take()
        .expect("上游应捕获请求");
    assert_eq!(got.version, Version::HTTP_11, "出站必须归一为 HTTP/1.1");
}

// 错误还原：请求体（数据面）抛出的 ProxyError 被 hyper 包装成 user body
// 错误后，必须沿 source() 链 downcast 还原（不是兜底），内部细节保留，
// 客户端只见泛化文案。
#[tokio::test]
async fn forward_recovers_proxy_error_wrapped_by_hyper() {
    let (addr, _captured) = spawn_echo_server().await;
    let client = UpstreamClient::new(Duration::from_secs(5));
    let ctx = Arc::new(RequestCtx::new(Protocol::Http1, None));
    let body = stream_body(futures_util::stream::iter([Err::<
        http_body::Frame<Bytes>,
        _,
    >(ProxyError::Upstream(
        SECRET_DETAIL.to_string(),
    ))]));
    let req = Request::builder()
        .method(Method::POST)
        .uri("http://gateway.example/")
        .extension(Arc::clone(&ctx))
        .body(body)
        .expect("构造请求");
    let err = client
        .forward(req, addr, "/")
        .await
        .expect_err("请求体错误必须冒泡");
    assert_eq!(err.kind(), ErrorKind::Unavailable);
    let internal = err.to_string();
    assert!(
        internal.contains(SECRET_DETAIL),
        "还原应保留内部细节: {internal}"
    );
    assert!(
        !internal.contains("upstream request failed"),
        "必须走 downcast 还原而不是兜底: {internal}"
    );
    assert_eq!(err.public_message(), "upstream unavailable");
    assert!(
        !err.public_message().contains(SECRET_DETAIL),
        "泛化文案不得泄露细节"
    );
}

// 连接拒绝（bind 后立即 drop 的端口）：错误链里没有 ProxyError 时走
// Unavailable 兜底，内部消息带 "upstream request failed" 与底层细节，
// 客户端仍只见泛化文案。
#[tokio::test]
async fn forward_falls_back_to_unavailable_when_connect_refused() {
    let dead = dead_port();
    let client = UpstreamClient::new(Duration::from_secs(5));
    let ctx = Arc::new(RequestCtx::new(Protocol::Http1, None));
    let req = Request::builder()
        .method(Method::GET)
        .uri("http://gateway.example/")
        .extension(Arc::clone(&ctx))
        .body(empty_body())
        .expect("构造请求");
    let err = client
        .forward(req, dead, "/x")
        .await
        .expect_err("连接拒绝必须报错");
    assert_eq!(err.kind(), ErrorKind::Unavailable);
    let internal = err.to_string();
    assert!(
        internal.contains("upstream request failed"),
        "兜底消息必须点名上游请求失败: {internal}"
    );
    assert_eq!(err.public_message(), "upstream unavailable");
}

// path_and_query 是系统边界输入：拼不出合法出站 URI 时快速失败
// （InvalidArgument），绝不把畸形 URI 发给上游。
// 列含义：(path_and_query, 用例说明)
#[tokio::test]
async fn forward_rejects_invalid_path_and_query() {
    let (addr, _captured) = spawn_echo_server().await;
    let client = UpstreamClient::new(Duration::from_secs(5));
    let ctx = Arc::new(RequestCtx::new(Protocol::Http1, None));
    let cases: [(&str, &str); 2] = [
        ("bad path", "含空格"),
        ("http://evil.example/abs", "绝对 URI 拼接后必然畸形"),
    ];
    for (pq, why) in cases {
        let req = Request::builder()
            .method(Method::GET)
            .uri("http://gateway.example/")
            .extension(Arc::clone(&ctx))
            .body(empty_body())
            .expect("构造请求");
        let err = client
            .forward(req, addr, pq)
            .await
            .expect_err("非法 path_and_query 必须拒绝");
        assert_eq!(err.kind(), ErrorKind::InvalidArgument, "{why}: {pq:?}");
    }
}

// 请求上下文缺失（分发层未注入）按内部错误处理，绝不静默放行。
#[tokio::test]
async fn forward_requires_request_ctx() {
    let (addr, _captured) = spawn_echo_server().await;
    let client = UpstreamClient::new(Duration::from_secs(5));
    let req = Request::builder()
        .method(Method::GET)
        .uri("http://gateway.example/")
        .body(empty_body())
        .expect("构造请求");
    let err = client
        .forward(req, addr, "/")
        .await
        .expect_err("缺 ctx 必须报错");
    assert_eq!(err.kind(), ErrorKind::Internal);
}

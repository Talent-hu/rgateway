//! [`crate::handler`] 的测试：两个准入闸（协议闸、scheme 闸）× 7 种协议的
//! 拒绝/放行矩阵、目标缺失闸、`SECRET_DETAIL` 泄露检测与真实回环转发。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tower::ServiceExt;

use rgateway_config::spec::{RouteCompileInfo, RouteHandlerFactory};
use rgateway_core::{
    empty_body, ErrorKind, GatewayError, Protocol, ProtocolHint, ProxyBody, RequestCtx, RouteState,
    RouteUri, UpstreamTarget,
};

use super::HttpHandlerFactory;

/// 泄露检测哨兵：既伪装 route id（内部细节），也用于断言客户端可见文案。
const SECRET_DETAIL: &str = "SECRET_DETAIL";

/// 「计划后续阶段」的内部细节关键词：允许出现在 `Display`（日志通道），
/// 绝不允许出现在客户端可见文案。
const PLANNED_DETAIL: &str = "planned for a later phase";

/// 双通道泄露检测：内部细节（route id 哨兵、后续阶段字样）绝不能出现在
/// 客户端可见通道（`public_message()` / `grpc_message()`）。
fn assert_no_leak(err: &GatewayError) {
    for visible in [err.public_message().to_string(), err.grpc_message()] {
        assert!(!visible.contains(SECRET_DETAIL), "泄露 route id: {visible}");
        assert!(!visible.contains(PLANNED_DETAIL), "泄露内部细节: {visible}");
    }
    assert_eq!(err.public_message(), "upstream unavailable");
}

/// 启动最小回环 pong 服务器：固定 200 + "pong"，并捕获请求 path_and_query。
async fn spawn_pong_server() -> (SocketAddr, Arc<Mutex<Option<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定回环端口");
    let addr = listener.local_addr().expect("取回环地址");
    let slot = Arc::new(Mutex::new(None));
    let path = Arc::clone(&slot);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let path = Arc::clone(&path);
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req: Request<Incoming>| {
                    let path = Arc::clone(&path);
                    async move {
                        *path.lock().expect("捕获锁") = Some(
                            req.uri()
                                .path_and_query()
                                .map(|pq| pq.as_str().to_string())
                                .unwrap_or_default(),
                        );
                        Ok::<_, std::io::Error>(Response::new(Full::new(Bytes::from_static(
                            b"pong",
                        ))))
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

/// 构造路由编译信息：route id 固定为 [`SECRET_DETAIL`] 哨兵（泄露检测）。
fn compile_info(uri: &str) -> RouteCompileInfo {
    RouteCompileInfo {
        id: Arc::from(SECRET_DETAIL),
        uri: RouteUri::parse(uri).expect("路由 URI"),
        protocol_hint: ProtocolHint::Auto,
        state: Arc::new(RouteState::new(SECRET_DETAIL)),
        transcoding: None,
    }
}

/// 构造带上下文的入站请求（ctx 经 extensions 注入，与分发层同语义）。
/// `target` 为 `None` 时模拟「目标选择层未选中实例」。
fn make_request(protocol: Protocol, target: Option<SocketAddr>) -> Request<ProxyBody> {
    let ctx = RequestCtx::new(protocol, Some("9.9.9.9:4242".parse().expect("peer")));
    if let Some(addr) = target {
        ctx.set_target(UpstreamTarget {
            instance_id: "inst-1".into(),
            addr,
        });
    }
    Request::builder()
        .method(Method::GET)
        .uri("http://gateway.example/echo?x=1")
        .header("host", "public.example")
        .extension(Arc::new(ctx))
        .body(empty_body())
        .expect("构造请求")
}

// 协议闸矩阵（7 种协议）：Http1/Http2 放行并转发到真实上游（200，路径透传），
// 其余 5 种一律 Unavailable（内部细节带 "planned for a later phase"），
// 客户端只见泛化文案（SECRET_DETAIL 泄露检测）。
// 列含义：(协议, 是否应放行转发)
#[tokio::test]
async fn protocol_gate_forwards_http_and_rejects_other_protocols() {
    let (upstream, path) = spawn_pong_server().await;
    let factory = HttpHandlerFactory::new(Duration::from_secs(5));
    let svc = factory.build(&compile_info("http://upstream.example"));
    let cases: [(Protocol, bool); 7] = [
        (Protocol::Http1, true),
        (Protocol::Http2, true),
        (Protocol::WebSocket, false),
        (Protocol::Grpc, false),
        (Protocol::GrpcWeb, false),
        (Protocol::GrpcWebText, false),
        (Protocol::RestJson, false),
    ];
    for (protocol, expect_ok) in cases {
        let req = make_request(protocol, Some(upstream));
        let result = svc.clone().oneshot(req).await;
        if expect_ok {
            let resp = result.unwrap_or_else(|e| panic!("{protocol:?} 应放行: {e}"));
            assert_eq!(resp.status(), StatusCode::OK, "{protocol:?}");
            assert_eq!(
                path.lock().expect("捕获锁").take().as_deref(),
                Some("/echo?x=1"),
                "{protocol:?}: path_and_query 必须透传"
            );
        } else {
            let err = result.expect_err("非 HTTP 协议必须拒绝");
            assert_eq!(err.kind(), ErrorKind::Unavailable, "{protocol:?}");
            assert!(
                err.to_string().contains(PLANNED_DETAIL),
                "{protocol:?}: 内部细节应点名后续阶段: {err}"
            );
            assert_no_leak(&err);
        }
    }
}

// scheme 闸矩阵（7 种协议）：直连 https 路由一律拒绝——严禁把 https 当明文
// 转发（防 TLS 静默降级）。scheme 闸先于协议闸执行，因此 7 种协议全部落在
// https 拒绝形态；内部细节带 "https upstream planned for a later phase"，
// 客户端只见泛化文案（SECRET_DETAIL 泄露检测）。
#[tokio::test]
async fn scheme_gate_rejects_https_direct_for_all_protocols() {
    let (upstream, _path) = spawn_pong_server().await;
    let factory = HttpHandlerFactory::new(Duration::from_secs(5));
    let svc = factory.build(&compile_info("https://upstream.example"));
    let protocols = [
        Protocol::Http1,
        Protocol::Http2,
        Protocol::WebSocket,
        Protocol::Grpc,
        Protocol::GrpcWeb,
        Protocol::GrpcWebText,
        Protocol::RestJson,
    ];
    for protocol in protocols {
        let req = make_request(protocol, Some(upstream));
        let err = svc
            .clone()
            .oneshot(req)
            .await
            .expect_err("https 直连必须拒绝");
        assert_eq!(err.kind(), ErrorKind::Unavailable, "{protocol:?}");
        assert!(
            err.to_string()
                .contains("https upstream planned for a later phase"),
            "{protocol:?}: 内部细节应点名 https: {err}"
        );
        assert_no_leak(&err);
    }
}

// scheme 闸放行形态：lb:// 路由（静态实例只有 ip:port 无 scheme）与明文
// http 直连路由都放行，转发到真实上游成功。
// 列含义：(路由 URI, 用例说明)
#[tokio::test]
async fn scheme_gate_allows_lb_and_http_routes() {
    let (upstream, _path) = spawn_pong_server().await;
    let factory = HttpHandlerFactory::new(Duration::from_secs(5));
    let cases: [(&str, &str); 2] = [
        ("lb://user-svc", "lb 路由无 scheme，放行"),
        ("http://upstream.example", "明文直连放行"),
    ];
    for (uri, why) in cases {
        let svc = factory.build(&compile_info(uri));
        let req = make_request(Protocol::Http1, Some(upstream));
        let resp = svc
            .oneshot(req)
            .await
            .unwrap_or_else(|e| panic!("{why}: {e}"));
        assert_eq!(resp.status(), StatusCode::OK, "{why}");
    }
}

// 目标缺失闸：ctx.target() 为空（目标选择层未选中实例）→ Unavailable；
// route id 是内部细节（哨兵 SECRET_DETAIL），绝不进客户端文案。
// 列含义：(协议, 用例说明)
#[tokio::test]
async fn missing_target_is_unavailable_and_hides_route_id() {
    let factory = HttpHandlerFactory::new(Duration::from_secs(5));
    let svc = factory.build(&compile_info("http://upstream.example"));
    let cases: [(Protocol, &str); 2] = [(Protocol::Http1, "http1"), (Protocol::Http2, "http2")];
    for (protocol, why) in cases {
        let req = make_request(protocol, None);
        let err = svc.clone().oneshot(req).await.expect_err("无目标必须拒绝");
        assert_eq!(err.kind(), ErrorKind::Unavailable, "{why}");
        assert!(
            err.to_string().contains(SECRET_DETAIL),
            "{why}: 内部消息应带 route id: {err}"
        );
        assert_no_leak(&err);
    }
}

//! dispatch 测试共用的测试替身与断言辅助：假链行为、手工路由/快照构造、
//! 请求构造、错误渲染断言（含 `SECRET_DETAIL` 泄露检测与 `X-Request-ID`
//! 治理检查）。由 `dispatch_tests` 经 `#[path]` 引入。

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use rgateway_config::compile::{CompiledConfig, ServerSettings, TlsSettings};
use rgateway_config::SnapshotStore;
use rgateway_core::{
    boxed_body, empty_body, full_body, BoxedService, GatewayError, ProtocolHint, ProxyBody,
    ProxyError, Route, RoutePredicate, RouteState, RouteUri, StateRegistry, StaticDiscovery,
};
use rgateway_proxy::framing::decode_trailer_frame;
use rgateway_router::{PathPredicate, RouteTable};
use tower::service_fn;
use tracing::field::{Field, Visit};
use tracing::{span, Event, Metadata, Subscriber};

use crate::dispatch::Dispatcher;

/// 泄露检测哨兵：伪装成「内部细节」写进错误载荷，任何客户端可见通道
/// （响应头、响应体）都不允许出现它。
pub(super) const SECRET_DETAIL: &str = "SECRET_DETAIL";

/// 假链行为：`Route.chain` 里装箱的测试替身。
#[derive(Clone, Copy)]
pub(super) enum ChainBehavior {
    /// 恒返回 200 + 标记体 "pong"。
    Ok200,
    /// 返回 `RateLimited`（载荷 SECRET_DETAIL）。
    RateLimited,
    /// 返回 `Unavailable`（载荷 SECRET_DETAIL）。
    Unavailable,
    /// 永不完成（并发闸测试用）。
    Pending,
    /// 收集请求体后原样回显；收集失败则把 `ProxyError` 归一成 `GatewayError`。
    EchoBody,
    /// 返回 200 + 永不结束的响应体（并发闸许可生命周期测试用）。
    HangingBody,
}

/// 按行为构造假链（BoxedService，克隆语义与真实链一致）。
pub(super) fn chain_for(behavior: ChainBehavior) -> BoxedService {
    BoxedService::new(service_fn(move |req: Request<ProxyBody>| async move {
        match behavior {
            ChainBehavior::Ok200 => Ok(Response::new(full_body(Bytes::from_static(b"pong")))),
            ChainBehavior::RateLimited => Err(GatewayError::RateLimited(SECRET_DETAIL.to_string())),
            ChainBehavior::Unavailable => Err(GatewayError::Unavailable(SECRET_DETAIL.to_string())),
            ChainBehavior::Pending => std::future::pending().await,
            ChainBehavior::HangingBody => Ok(Response::new(boxed_body(NeverBody))),
            ChainBehavior::EchoBody => match req.into_body().collect().await {
                Ok(collected) => Ok(Response::new(full_body(collected.to_bytes()))),
                Err(err) => Err(err.into_gateway_error()),
            },
        }
    }))
}

/// 永不结束的 body：`poll_frame` 恒 Pending（不注册 waker），drop 即走。
/// 并发闸许可生命周期测试用它模拟「响应体未耗尽」的悬挂状态。
struct NeverBody;

impl Body for NeverBody {
    type Data = Bytes;
    type Error = ProxyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ProxyError>>> {
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        false
    }
}

/// 客户端请求体的错误来源：验证 `limited_body` 的错误分类（downcast 还原）。
#[derive(Clone, Copy)]
pub(super) enum BodyFailure {
    /// `ProxyError::Upstream`（非 Gateway 变体）——归 InvalidArgument。
    Upstream,
    /// `ProxyError::Io`——归 InvalidArgument。
    Io,
    /// `ProxyError::Gateway(...)`——必须原样透传（保持原错误种类）。
    EmbeddedGateway,
}

/// 一被 poll 就抛出指定错误的请求体（首个 poll 报错后即结束）。
pub(super) fn failing_body(failure: BodyFailure) -> ProxyBody {
    let err = match failure {
        BodyFailure::Upstream => ProxyError::Upstream("client body stream failed".to_string()),
        BodyFailure::Io => ProxyError::Io(std::io::Error::other("client body io failed")),
        BodyFailure::EmbeddedGateway => {
            ProxyError::Gateway(GatewayError::RateLimited(SECRET_DETAIL.to_string()))
        }
    };
    boxed_body(FailingBody { err: Some(err) })
}

/// 持有一次性错误的 body：poll 时先报错，之后结束。
struct FailingBody {
    err: Option<ProxyError>,
}

impl Body for FailingBody {
    type Data = Bytes;
    type Error = ProxyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ProxyError>>> {
        let this = self.get_mut();
        Poll::Ready(this.err.take().map(Err))
    }
}

/// 手工构造匹配 `/api/**` 的路由（全字段 pub，直接装配假链）。
pub(super) fn route(id: &str, hint: ProtocolHint, behavior: ChainBehavior) -> Arc<Route> {
    let predicate = PathPredicate::new(vec!["/api/**".to_string()]).expect("static pattern");
    Arc::new(Route {
        id: Arc::from(id),
        order: 0,
        uri: RouteUri::parse("lb://test-svc").expect("static uri"),
        protocol_hint: hint,
        predicates: vec![Arc::new(predicate) as Arc<dyn RoutePredicate>],
        state: Arc::new(RouteState::new(id)),
        chain: std::sync::Mutex::new(chain_for(behavior)),
    })
}

/// 以手工快照构造 Dispatcher；limits 取自快照的 server.limits。
/// 访问日志默认开 + JSON 格式；需要换格式/开关时用 [`dispatcher_access`]。
pub(super) fn dispatcher(
    max_request_body: usize,
    max_concurrent_requests: u32,
    routes: Vec<Arc<Route>>,
) -> Arc<Dispatcher> {
    dispatcher_access(
        max_request_body,
        max_concurrent_requests,
        routes,
        true,
        true,
    )
}

/// 同 [`dispatcher`]，但可指定访问日志开关与格式（观测性测试用）。
/// 参数：(max_request_body, max_concurrent_requests, routes, access_log_enabled, access_log_json)。
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatcher_access(
    max_request_body: usize,
    max_concurrent_requests: u32,
    routes: Vec<Arc<Route>>,
    access_log_enabled: bool,
    access_log_json: bool,
) -> Arc<Dispatcher> {
    let server = ServerSettings {
        listen: "127.0.0.1:0".parse().expect("addr"),
        max_concurrent_streams: 1024,
        initial_stream_window_size: 1024 * 1024,
        initial_connection_window_size: 1024 * 1024,
        max_request_body,
        max_concurrent_requests,
        max_retry_buffer: 1024,
        connect_timeout: Duration::from_secs(3),
        request_default: Duration::from_secs(30),
        graceful_shutdown: Duration::from_secs(30),
        ws_handshake: Duration::from_secs(10),
        tls: TlsSettings::default(),
    };
    let compiled = CompiledConfig {
        server,
        routes: RouteTable::new(routes),
        discovery: Arc::new(StaticDiscovery::new(HashMap::new())),
        states: Arc::new(StateRegistry::new()),
        mgmt_listen: "127.0.0.1:9090".parse().expect("addr"),
        mgmt_grpc: false,
        access_log_enabled,
        access_log_json,
        metrics_enabled: false,
        metrics_path: "/metrics".to_string(),
        tracing_filter: None,
    };
    Arc::new(Dispatcher::new(
        Arc::new(SnapshotStore::new(compiled)),
        rgateway_core::ClientScheme::Http,
    ))
}

/// 构造请求（测试辅助）：method / uri / 版本 / 可选 content-type 与
/// content-length 头，body 由调用方给出。
pub(super) fn request_with(
    method: &str,
    uri: &str,
    version: http::Version,
    content_type: Option<&str>,
    content_length: Option<&str>,
    body: ProxyBody,
) -> Request<ProxyBody> {
    let mut builder = Request::builder().method(method).uri(uri).version(version);
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    if let Some(cl) = content_length {
        builder = builder.header("content-length", cl);
    }
    builder.body(body).expect("test request builds")
}

/// 空体 GET 请求（HTTP/1.1）。
pub(super) fn http_request(uri: &str) -> Request<ProxyBody> {
    request_with("GET", uri, http::Version::HTTP_11, None, None, empty_body())
}

/// 取出响应体字节（消费响应；测试辅助）。
pub(super) async fn body_bytes(resp: Response<ProxyBody>) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .expect("body collect")
        .to_bytes()
        .to_vec()
}

/// 取头值文本（测试辅助；缺失或非 UTF-8 直接 panic，测试必须看到失败）。
pub(super) fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .expect("缺少期望的响应头")
        .to_str()
        .expect("响应头不是合法 UTF-8")
}

/// 双通道契约的泄露检测：响应头与响应体都不允许含 SECRET_DETAIL。
pub(super) fn assert_no_leak(headers: &HeaderMap, body: &[u8]) {
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

/// 每个响应都必须带合法 uuid 形态的 X-Request-ID（finalize_response 治理）。
pub(super) fn assert_request_id(headers: &HeaderMap) {
    let id = header_text(headers, "x-request-id");
    assert!(
        uuid::Uuid::parse_str(id).is_ok(),
        "X-Request-ID 必须是合法 uuid: {id}"
    );
}

/// 期望的错误渲染形态（渲染协议由探测/hint 覆写决定，映射取自
/// `GatewayError` 的方法，此处不自建映射表）。
pub(super) enum Expect {
    /// HTTP 家族：状态码 + JSON `{"error": ...}` 文案。
    Http {
        status: u16,
        json_error: &'static str,
    },
    /// 原生 gRPC：200 + grpc-status / grpc-message 头（trailers-only）。
    Grpc {
        status: &'static str,
        message: &'static str,
    },
    /// gRPC-Web：200 + body 末尾 trailer 帧。
    GrpcWebFrame { status: i32, message: &'static str },
}

/// 按期望形态断言错误渲染（含泄露检测与 X-Request-ID）。
pub(super) async fn assert_error_shape(resp: Response<ProxyBody>, want: &Expect, label: &str) {
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = body_bytes(resp).await;
    assert_request_id(&headers);
    assert_no_leak(&headers, &body);
    match want {
        Expect::Http {
            status: want_status,
            json_error,
        } => {
            assert_eq!(status.as_u16(), *want_status, "{label}");
            assert_eq!(
                header_text(&headers, "content-type"),
                "application/json",
                "{label}"
            );
            assert_eq!(
                body,
                format!("{{\"error\":\"{json_error}\"}}").as_bytes(),
                "{label} JSON 形状必须精确"
            );
        }
        Expect::Grpc {
            status: want_status,
            message,
        } => {
            assert_eq!(status, StatusCode::OK, "{label}");
            assert_eq!(
                header_text(&headers, "grpc-status"),
                *want_status,
                "{label}"
            );
            assert_eq!(header_text(&headers, "grpc-message"), *message, "{label}");
            assert!(body.is_empty(), "{label} trailers-only body 应为空");
        }
        Expect::GrpcWebFrame {
            status: want_status,
            message,
        } => {
            assert_eq!(status, StatusCode::OK, "{label}");
            assert_eq!(
                decode_trailer_frame(&body).expect("trailer frame"),
                (*want_status, message.to_string()),
                "{label}"
            );
        }
    }
}

// ---- tracing 捕获（访问日志断言）----

/// 捕获 `target="access"` 事件消息的极简订阅器（克隆共享缓冲）。
///
/// 只关心 message 字段（访问日志渲染行），span/其余字段不参与断言——
/// 访问日志的 schema 由 rgateway-obs 的渲染测试锁定，这里只锁「派发侧
/// 会把渲染行发到 access 目标」这一装配事实。
#[derive(Clone, Default)]
pub(super) struct CaptureLog(Arc<Mutex<Vec<String>>>);

impl CaptureLog {
    /// 已捕获的消息行（时间序）。
    pub(super) fn lines(&self) -> Vec<String> {
        self.0.lock().expect("capture lock").clone()
    }
}

/// 抽取事件 message 字段的访问器（`fmt::Arguments` 走 `record_debug`）。
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        }
    }
}

impl Subscriber for CaptureLog {
    fn enabled(&self, meta: &Metadata<'_>) -> bool {
        meta.target() == "access"
    }

    fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        self.0.lock().expect("capture lock").push(visitor.0);
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

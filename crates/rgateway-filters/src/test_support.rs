//! 测试共用的假 inner 服务、请求构造器与执行辅助（仅测试编译）。
//!
//! 全部假服务都直接产出 `BoxedService`，行为用原子计数/脚本/延迟控制，
//! 不碰真实网络——行为测试只验证过滤器自身的决策与错误映射。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame};
use rgateway_config::{FilterBuildCtx, FilterSpec};
use rgateway_core::{
    boxed_body, empty_body, full_body, BoxedService, GatewayError, Protocol, ProxyBody, ProxyError,
    RequestCtx, RouteState, StaticDiscovery, UpstreamTarget,
};
use tower::service_fn;
use tower::util::BoxCloneService;

/// 构造一个带 `Arc<RequestCtx>` 的请求，body 为空。
///
/// `peer` 写入 `peer_addr`（限流 client_ip 维度用）；`target` 若给出则
/// `set_target`（熔断器按该实例判）。
pub(crate) fn request(
    peer: Option<SocketAddr>,
    target: Option<UpstreamTarget>,
) -> http::Request<ProxyBody> {
    let ctx = Arc::new(RequestCtx::new(Protocol::Http1, peer));
    if let Some(t) = target {
        ctx.set_target(t);
    }
    let mut req = http::Request::new(empty_body());
    req.extensions_mut().insert(ctx);
    req
}

/// 构造一个带指定请求体的请求（BodyLimit 测试用）。
pub(crate) fn request_with_body(
    peer: Option<SocketAddr>,
    body: ProxyBody,
    content_length: Option<usize>,
) -> http::Request<ProxyBody> {
    request_with_method("GET", peer, body, content_length)
}

/// 构造指定方法 + 请求体的请求（Retry 方法闸测试用）；`content_length`
/// 若给出则写入 Content-Length 头（已知长度语义）。
pub(crate) fn request_with_method(
    method: &str,
    peer: Option<SocketAddr>,
    body: ProxyBody,
    content_length: Option<usize>,
) -> http::Request<ProxyBody> {
    let ctx = Arc::new(RequestCtx::new(Protocol::Http1, peer));
    let mut req = http::Request::new(body);
    *req.method_mut() = method.parse().expect("valid http method");
    if let Some(len) = content_length {
        req.headers_mut().insert(
            http::header::CONTENT_LENGTH,
            http::HeaderValue::from_str(&len.to_string()).expect("ascii digits"),
        );
    }
    req.extensions_mut().insert(ctx);
    req
}

/// 一个 `UpstreamTarget` 便捷构造（实例 id = "inst-1"）。
pub(crate) fn target(id: &str) -> UpstreamTarget {
    UpstreamTarget {
        instance_id: Arc::from(id),
        addr: "127.0.0.1:9000".parse().expect("static addr"),
    }
}

/// 空的静态服务发现（过滤器构建上下文用，过滤器本身不查 discovery）。
pub(crate) fn empty_discovery() -> Arc<StaticDiscovery> {
    Arc::new(StaticDiscovery::new(HashMap::new()))
}

/// 测试用 [`FilterBuildCtx`]：给定路由 id 与治理状态（缓冲上限取默认 1MB）。
pub(crate) fn build_ctx(route_id: &str, state: Arc<RouteState>) -> FilterBuildCtx {
    build_ctx_with_buffer(route_id, state, 1024 * 1024)
}

/// 同 [`build_ctx`]，但可指定 `max_retry_buffer`（Retry 缓冲上限测试用）。
pub(crate) fn build_ctx_with_buffer(
    route_id: &str,
    state: Arc<RouteState>,
    max_retry_buffer: usize,
) -> FilterBuildCtx {
    FilterBuildCtx {
        route_id: Arc::from(route_id),
        route_state: state,
        discovery: empty_discovery(),
        max_retry_buffer,
        config_dir: Arc::new(std::path::PathBuf::from("conf")),
    }
}

/// 由紧凑 DSL 文本构造 [`FilterSpec`]（测试里的参数表格用一行表达）。
pub(crate) fn spec(compact: &str) -> FilterSpec {
    FilterSpec::parse_compact(compact).expect("compact spec must parse")
}

/// 由结构化 YAML 文本构造 [`FilterSpec`]（序列/map 参数——如 Retry 的
/// `methods` / `backoff`——紧凑 DSL 表达不了，走 YAML 原生类型）。
pub(crate) fn spec_yaml(yaml: &str) -> FilterSpec {
    let cfg: rgateway_config::schema::FilterConfig =
        serde_yaml::from_str(yaml).expect("structured spec must parse");
    FilterSpec::from_config(&cfg).expect("spec must normalize")
}

/// 无参数的 [`FilterSpec`]（紧凑 DSL 需要 `Name=k:v` 形态，空参走结构化）。
pub(crate) fn empty_spec(name: &str) -> FilterSpec {
    FilterSpec {
        name: name.to_string(),
        params: std::collections::BTreeMap::new(),
        source: name.to_string(),
    }
}

/// 调用次数可查的假 inner：返回 200 空 body。
pub(crate) fn counting_ok(counter: Arc<AtomicUsize>) -> BoxedService {
    BoxCloneService::new(service_fn(move |_req: http::Request<ProxyBody>| {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
        }
    }))
}

/// 延迟 `latency` 后返回 200 的假 inner（超时类测试用）。
pub(crate) fn slow_ok(latency: Duration) -> BoxedService {
    BoxCloneService::new(service_fn(
        move |_req: http::Request<ProxyBody>| async move {
            tokio::time::sleep(latency).await;
            Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
        },
    ))
}

/// 按脚本逐次产出结果的假 inner：脚本每项是一次调用的结果
/// （`"ok"` = 200，`"status:500"` = 指定状态码，`"err:unavailable"` 等 =
/// 对应 [`GatewayError`] 变体），脚本耗尽后重复最后一项。
pub(crate) fn scripted(script: Vec<Outcome>) -> (BoxedService, Arc<Mutex<Vec<String>>>) {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let cursor = Arc::new(AtomicUsize::new(0));
    let script = Arc::new(script);
    let log2 = Arc::clone(&log);
    let svc = BoxCloneService::new(service_fn(move |_req: http::Request<ProxyBody>| {
        let script = Arc::clone(&script);
        let cursor = Arc::clone(&cursor);
        let log = Arc::clone(&log2);
        async move {
            let i = cursor.fetch_add(1, Ordering::SeqCst);
            let outcome = *script
                .get(i)
                .or_else(|| script.last())
                .expect("script must not be empty");
            log.lock().expect("lock").push(outcome.label().to_string());
            outcome.into_response()
        }
    }));
    (svc, log)
}

/// 按调用序号产出结果的假 inner：`f(第几次调用，0 起)` 决定本次结果——
/// 每次调用都能新建响应（自定义头/体），返回调用计数器供断言精确次数。
pub(crate) fn scripted_fn<F>(f: F) -> (BoxedService, Arc<AtomicUsize>)
where
    F: Fn(usize) -> Result<http::Response<ProxyBody>, GatewayError> + Send + Sync + 'static,
{
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&counter);
    let f = Arc::new(f);
    let svc = BoxCloneService::new(service_fn(move |_req: http::Request<ProxyBody>| {
        let counter = Arc::clone(&seen);
        let f = Arc::clone(&f);
        async move {
            let i = counter.fetch_add(1, Ordering::SeqCst);
            f.as_ref()(i)
        }
    }));
    (svc, counter)
}

/// [`scripted`] 脚本里的一次调用结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ok200,
    Status(u16),
    Unavailable,
    Deadline,
    CircuitOpen,
    Internal,
    RateLimited,
    InvalidArgument,
}

impl Outcome {
    /// 执行日志标签（表格断言用）。
    fn label(self) -> &'static str {
        match self {
            Self::Ok200 => "ok",
            Self::Status(n) => match n {
                500 => "status:500",
                503 => "status:503",
                404 => "status:404",
                _ => "status:other",
            },
            Self::Unavailable => "err:unavailable",
            Self::Deadline => "err:deadline",
            Self::CircuitOpen => "err:circuit_open",
            Self::Internal => "err:internal",
            Self::RateLimited => "err:rate_limited",
            Self::InvalidArgument => "err:invalid_argument",
        }
    }

    /// 转成响应结果：错误变体走 `Err`，状态码变体走 `Ok(Response)`。
    fn into_response(self) -> Result<http::Response<ProxyBody>, GatewayError> {
        match self {
            Self::Ok200 => Ok(http::Response::new(empty_body())),
            Self::Status(code) => {
                let mut resp = http::Response::new(empty_body());
                *resp.status_mut() =
                    http::StatusCode::from_u16(code).expect("valid status code in test");
                Ok(resp)
            }
            Self::Unavailable => Err(GatewayError::Unavailable("test upstream".into())),
            Self::Deadline => Err(GatewayError::DeadlineExceeded("test deadline".into())),
            Self::CircuitOpen => Err(GatewayError::CircuitOpen("test circuit".into())),
            Self::Internal => Err(GatewayError::Internal("test internal".into())),
            Self::RateLimited => Err(GatewayError::RateLimited("test rate".into())),
            Self::InvalidArgument => Err(GatewayError::InvalidArgument("test argument".into())),
        }
    }
}

/// 收集请求体的假 inner：完整读出请求体后回显为响应体；读失败把
/// body 错误归一成 [`GatewayError`]（经 [`ProxyError::into_gateway_error`]）。
pub(crate) fn echo_body() -> BoxedService {
    BoxCloneService::new(service_fn(|req: http::Request<ProxyBody>| async move {
        use http_body_util::BodyExt;
        match req.into_body().collect().await {
            Ok(collected) => Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(
                full_body(collected.to_bytes()),
            )),
            Err(err) => Err(err.into_gateway_error()),
        }
    }))
}

/// 数据路径立刻报错的请求体（非限长 body 错误的客户端故障测试用）。
pub(crate) fn failing_body(message: &'static str) -> ProxyBody {
    let frames =
        futures_util::stream::once(async move { Err(ProxyError::Upstream(message.to_string())) });
    boxed_body(http_body_util::StreamBody::new(frames))
}

/// 单帧请求体（BodyLimit 正常/超限测试用）。
pub(crate) fn chunk_body(data: &'static [u8]) -> ProxyBody {
    let frames = futures_util::stream::once(async move {
        Ok::<Frame<Bytes>, ProxyError>(Frame::data(Bytes::from_static(data)))
    });
    boxed_body(http_body_util::StreamBody::new(frames))
}

/// 响应头立刻到达、第 2 帧延迟 `delay` 的响应体（超时只约束响应头的
/// 契约测试用）。帧 1 → 睡眠 → 帧 2 → 结束。
pub(crate) struct DelayedResponseBody {
    delay: Duration,
    state: DelayedState,
}

enum DelayedState {
    FirstFrame,
    Gap(std::pin::Pin<Box<tokio::time::Sleep>>),
    SecondFrame,
    Done,
}

impl DelayedResponseBody {
    pub(crate) fn new(delay: Duration) -> Self {
        Self {
            delay,
            state: DelayedState::FirstFrame,
        }
    }
}

impl Body for DelayedResponseBody {
    type Data = Bytes;
    type Error = ProxyError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, ProxyError>>> {
        use std::future::Future;
        use std::task::Poll;
        let this = self.get_mut();
        let delay = this.delay;
        loop {
            // 状态先「拿走」置 Done 占位：睡眠要 move 出来 poll，未完成分支
            // 放回去；漏放回只会提前结束流，不会卡死。
            match std::mem::replace(&mut this.state, DelayedState::Done) {
                DelayedState::FirstFrame => {
                    this.state = DelayedState::Gap(Box::pin(tokio::time::sleep(delay)));
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"frame-1")))));
                }
                DelayedState::Gap(mut gap) => match gap.as_mut().poll(cx) {
                    Poll::Ready(()) => this.state = DelayedState::SecondFrame,
                    Poll::Pending => {
                        this.state = DelayedState::Gap(gap);
                        return Poll::Pending;
                    }
                },
                DelayedState::SecondFrame => {
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"frame-2")))));
                }
                DelayedState::Done => return Poll::Ready(None),
            }
        }
    }
}

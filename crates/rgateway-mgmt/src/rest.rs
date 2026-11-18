//! 管理面 REST 处理：请求级纯函数（无 accept 循环），输出统一信封
//! `{"success": bool, "data": <object|null>, "error": <string|null>}`。
//!
//! # 设计要点（为什么这样设计）
//!
//! * **请求级纯函数**：[`handle`] 只消费一个请求、返回一个响应，装配方
//!   （rgateway-server）负责监听与 gRPC/REST 同端口多路复用；第二阶段把
//!   gRPC 复用进来时按 content-type 分流即可，不必再拆 accept 循环。
//! * **边界校验集中在入口**：未知路径 404、方法不符 405（带 `Allow` 头）、
//!   reset body 非法 400，文案全部是可行动的泛化信息——绝不回显文件系统
//!   路径、内部地址或配置原文（响应只含 id/计数/URI 字符串形态）。
//! * **熔断器读输出确定性**：实例状态来自 DashMap，迭代无序，输出前按
//!   实例 id 排序，保证同一状态两次查询得到同一份 JSON，便于运维比对。
//! * **重置幂等**：未知路由/未知实例返回 `{"reset":0}` 而不是报错——重放
//!   重置请求无害，运维脚本不需要先查后改。
//! * **`/metrics` 是唯一的非信封端点**：路径取自快照
//!   （`observability.metrics.path`，可热重载），输出 Prometheus 文本。
//!   指标未启用（无渲染闭包）时 404——端点不存在比「存在但 503」更诚实，
//!   也避免向未授权扫描者暴露「指标曾经开启」的痕迹；闭包存在但返回
//!   `None`（导出器未就绪）才 503。

use std::sync::Arc;

use bytes::Bytes;
use http::header::{ALLOW, CONTENT_TYPE};
use http::{HeaderValue, StatusCode};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use rgateway_config::CompiledConfig;
use rgateway_core::{BreakerState, ProtocolHint, Route};

use crate::state::MgmtState;

/// reset 请求体上限（字节）。
///
/// 管理面是低频运维接口，64 KiB 足够任意合法重置请求；不限长的全量
/// `collect` 是内存放大攻击面（`mgmt.listen` 被配成非回环地址时尤其危险），
/// 超限直接 413 拒绝、不进 JSON 解析。
const MAX_RESET_BODY: usize = 64 * 1024;

/// reset 单字段（`route_id` / `instance_id`）长度上限。
///
/// 与 [`MAX_RESET_BODY`] 同属防放大边界：字段合法值是路由 id 与
/// `host:port` 实例 id，远低于 256；gRPC 侧（`grpc.rs`）复用同一常量，
/// 保证两条通道的边界语义对称。
pub(crate) const MAX_RESET_ID_LEN: usize = 256;

/// 管理面 REST 响应体类型：完整 JSON 载荷（管理面数据量小，无需流式）。
pub type MgmtBody = Full<Bytes>;

/// 内部错误的兜底信封（手写字面量，避免序列化失败路径递归）。
const INTERNAL_ERROR_BODY: &[u8] = br#"{"success":false,"data":null,"error":"internal error"}"#;

/// 管理面内部错误（库级，thiserror）；不对外回显细节。
#[derive(Debug, thiserror::Error)]
enum MgmtError {
    /// 信封 JSON 序列化失败（结构固定、无浮点，实际不可达；防御性保留）。
    #[error("response envelope serialization failed")]
    Serialize(#[from] serde_json::Error),
}

/// 项目约定的 API 响应信封。
#[derive(Debug, Serialize)]
struct Envelope<T: Serialize> {
    success: bool,
    data: Option<T>,
    error: Option<String>,
}

// ---- 响应 data 形态（DTO，只序列化、绝不携带内部路径/密钥） ----

/// GET /health 的 data。
#[derive(Debug, Serialize)]
struct HealthData {
    status: &'static str,
}

/// 路由摘要（snapshot / routes 端点共用）：uri 只输出 [`RouteUri`]
/// 的字符串形态（`lb://name` 或 http(s) URI）。
///
/// [`RouteUri`]: rgateway_core::RouteUri
#[derive(Debug, Serialize)]
struct RouteSummary {
    id: String,
    order: i32,
    uri: String,
    protocol: &'static str,
}

/// GET /mgmt/v1/snapshot 的 data。
#[derive(Debug, Serialize)]
struct SnapshotData {
    version: u64,
    failed_reloads: u64,
    listen: String,
    route_count: usize,
    routes: Vec<RouteSummary>,
}

/// GET /mgmt/v1/routes 的 data。
#[derive(Debug, Serialize)]
struct RoutesData {
    routes: Vec<RouteSummary>,
}

/// 单个实例的熔断状态（closed/open/half_open）。
#[derive(Debug, Serialize)]
struct BreakerEntry {
    instance_id: String,
    state: &'static str,
}

/// 单条路由的熔断状态列表。
#[derive(Debug, Serialize)]
struct RouteBreakers {
    route_id: String,
    breakers: Vec<BreakerEntry>,
}

/// GET /mgmt/v1/breakers 的 data。
#[derive(Debug, Serialize)]
struct BreakersData {
    routes: Vec<RouteBreakers>,
}

/// POST /mgmt/v1/breakers/reset 的请求体（严格校验，未知字段拒绝）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetRequest {
    route_id: String,
    /// 指定实例；缺省或 `null` 表示重置该路由全部实例。
    #[serde(default)]
    instance_id: Option<String>,
}

/// POST /mgmt/v1/breakers/reset 的 data。
#[derive(Debug, Serialize)]
struct ResetData {
    reset: usize,
}

/// GET /mgmt/v1/stats 的 data（聚合视图）。
#[derive(Debug, Serialize)]
struct StatsData {
    version: u64,
    failed_reloads: u64,
    route_count: usize,
    breaker_count: usize,
    breaker_open_count: usize,
}

/// 处理一个管理面 HTTP 请求（请求级纯函数；accept/多路复用由装配方负责）。
///
/// 路由语义：指标路径（快照 `observability.metrics.path`）优先；其余未知
/// 路径 404；已知路径方法不符 405（带 `Allow` 头）；`GET` 只读端点与
/// `POST /mgmt/v1/breakers/reset` 见各 handler。
pub async fn handle<B>(state: Arc<MgmtState>, req: http::Request<B>) -> http::Response<MgmtBody>
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let (parts, body) = req.into_parts();
    let path = parts.uri.path();
    // 指标路径跟随快照（可热重载），因此不能进静态方法表，单独短路。
    if path == state.store().load().metrics_path {
        return match parts.method.as_str() {
            "GET" => metrics_endpoint(&state),
            _ => method_not_allowed("GET"),
        };
    }
    match allowed_method(path) {
        None => fail(StatusCode::NOT_FOUND, "not found"),
        Some(allow) if parts.method.as_str() != allow => method_not_allowed(allow),
        Some(_) => match path {
            "/health" => ok(HealthData { status: "ok" }),
            "/mgmt/v1/snapshot" => snapshot(&state),
            "/mgmt/v1/routes" => routes(&state),
            "/mgmt/v1/breakers" => breakers(&state),
            "/mgmt/v1/stats" => stats(&state),
            _ => reset(state, body).await,
        },
    }
}

/// 指标端点：端点存在性 = 快照 `metrics.enabled`（热重载即时生效，与
/// `metrics.path` 同语义）∧ 渲染闭包就绪（启动期装上 recorder 才注入），
/// 任一不满足 404；闭包返回 `None`（导出器未就绪）503；否则输出
/// Prometheus 文本（`text/plain`，exposition format）。
fn metrics_endpoint(state: &Arc<MgmtState>) -> http::Response<MgmtBody> {
    if !state.store().load().metrics_enabled {
        return fail(StatusCode::NOT_FOUND, "not found");
    }
    let Some(renderer) = state.metrics() else {
        return fail(StatusCode::NOT_FOUND, "not found");
    };
    match renderer() {
        Some(text) => text_response(StatusCode::OK, text),
        None => fail(StatusCode::SERVICE_UNAVAILABLE, "metrics unavailable"),
    }
}

/// 组一个纯文本响应（指标文本里可能有任意 UTF-8 内容，不做转义、原样输出）。
fn text_response(status: StatusCode, payload: String) -> http::Response<MgmtBody> {
    let mut resp = http::Response::new(Full::new(Bytes::from(payload)));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    resp
}

/// 每个已知端点允许的方法（405 的 `Allow` 头据此回显）；未知路径返回 `None`。
fn allowed_method(path: &str) -> Option<&'static str> {
    match path {
        "/health" | "/mgmt/v1/snapshot" | "/mgmt/v1/routes" | "/mgmt/v1/breakers"
        | "/mgmt/v1/stats" => Some("GET"),
        "/mgmt/v1/breakers/reset" => Some("POST"),
        _ => None,
    }
}

/// GET /mgmt/v1/snapshot：版本、失败重载数、数据面监听、路由摘要。
fn snapshot(state: &Arc<MgmtState>) -> http::Response<MgmtBody> {
    let snap = state.store().load();
    let routes = route_summaries(&snap);
    ok(SnapshotData {
        version: state.store().version(),
        failed_reloads: state.store().failed_reloads(),
        listen: snap.server.listen.to_string(),
        route_count: routes.len(),
        routes,
    })
}

/// GET /mgmt/v1/routes：与 snapshot 同形态的路由摘要（按匹配顺序）。
fn routes(state: &Arc<MgmtState>) -> http::Response<MgmtBody> {
    let snap = state.store().load();
    ok(RoutesData {
        routes: route_summaries(&snap),
    })
}

/// GET /mgmt/v1/breakers：逐路由逐实例的熔断状态；无状态输出空数组。
fn breakers(state: &Arc<MgmtState>) -> http::Response<MgmtBody> {
    let snap = state.store().load();
    let mut routes = Vec::with_capacity(snap.routes.len());
    for route in snap.routes.routes() {
        let mut entries: Vec<BreakerEntry> = state
            .states()
            .route_state(&route.id)
            .breaker_states()
            .into_iter()
            .map(|(instance_id, state)| BreakerEntry {
                instance_id: instance_id.to_string(),
                state: state.as_label(),
            })
            .collect();
        // DashMap 迭代无序：排序后输出，保证同一状态两次查询得到同一份 JSON。
        entries.sort_unstable_by(|a, b| a.instance_id.cmp(&b.instance_id));
        routes.push(RouteBreakers {
            route_id: route.id.to_string(),
            breakers: entries,
        });
    }
    ok(BreakersData { routes })
}

/// GET /mgmt/v1/stats：版本/失败重载/路由数/熔断器（总数、打开数）聚合。
fn stats(state: &Arc<MgmtState>) -> http::Response<MgmtBody> {
    let snap = state.store().load();
    let mut breaker_count = 0usize;
    let mut breaker_open_count = 0usize;
    for route in snap.routes.routes() {
        for (_, breaker) in state.states().route_state(&route.id).breaker_states() {
            breaker_count += 1;
            // 打开数只统计 Open 态；HalfOpen 是恢复探测，不算「拒绝流量」。
            if breaker == BreakerState::Open {
                breaker_open_count += 1;
            }
        }
    }
    ok(StatsData {
        version: state.store().version(),
        failed_reloads: state.store().failed_reloads(),
        route_count: snap.routes.len(),
        breaker_count,
        breaker_open_count,
    })
}

/// POST /mgmt/v1/breakers/reset：body 非法/缺 route_id 一律 400
/// （文案固定 `invalid request`）；未知路由/实例返回 `{"reset":0}`（幂等）。
async fn reset<B>(state: Arc<MgmtState>, body: B) -> http::Response<MgmtBody>
where
    B: http_body::Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    // 限长再收集：body 错误先归一成 io::Error（Limited 只接受 std Error），
    // 超限触发 LengthLimitError → 413；其余读体失败 → 400（泛化文案）。
    let body = body.map_err(|err| std::io::Error::other(err.to_string()));
    let raw = match Limited::new(body, MAX_RESET_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(err) if err.downcast_ref::<LengthLimitError>().is_some() => {
            return fail(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
        }
        Err(err) => {
            // 读体失败只进服务端日志，客户端拿到泛化文案。
            tracing::debug!(error = %err, "mgmt reset request body read failed");
            return fail(StatusCode::BAD_REQUEST, "invalid request");
        }
    };
    let parsed: ResetRequest = match parse_json(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::debug!(error = %err, "mgmt reset request body rejected");
            return fail(StatusCode::BAD_REQUEST, "invalid request");
        }
    };
    if parsed.route_id.is_empty()
        || parsed.route_id.len() > MAX_RESET_ID_LEN
        || parsed
            .instance_id
            .as_deref()
            .is_some_and(|id| id.len() > MAX_RESET_ID_LEN)
    {
        return fail(StatusCode::BAD_REQUEST, "invalid request");
    }
    let reset = state
        .states()
        .reset_breaker(&parsed.route_id, parsed.instance_id.as_deref());
    ok(ResetData { reset })
}

/// 路由摘要列表（按路由表匹配顺序）。
fn route_summaries(snap: &CompiledConfig) -> Vec<RouteSummary> {
    snap.routes
        .routes()
        .iter()
        .map(|r| route_summary(r))
        .collect()
}

/// 单条路由 → 摘要。
fn route_summary(route: &Route) -> RouteSummary {
    RouteSummary {
        id: route.id.to_string(),
        order: route.order,
        uri: route.uri.as_label(),
        protocol: hint_label(route.protocol_hint),
    }
}

/// 协议提示 → 稳定输出标签（`auto` 或协议标签）。
fn hint_label(hint: ProtocolHint) -> &'static str {
    match hint {
        ProtocolHint::Auto => "auto",
        ProtocolHint::Fixed(protocol) => protocol.as_label(),
    }
}

/// 解析请求体 JSON（严格模式由 DTO 的 `deny_unknown_fields` 承担）。
fn parse_json<T: DeserializeOwned>(raw: &[u8]) -> Result<T, MgmtError> {
    Ok(serde_json::from_slice(raw)?)
}

/// 成功信封（200）。
fn ok<T: Serialize>(data: T) -> http::Response<MgmtBody> {
    let envelope = Envelope {
        success: true,
        data: Some(data),
        error: None,
    };
    render(StatusCode::OK, &envelope)
}

/// 失败信封（4xx/5xx）：文案是可行动的泛化信息，不含内部细节。
fn fail(status: StatusCode, message: &str) -> http::Response<MgmtBody> {
    let envelope = Envelope::<()> {
        success: false,
        data: None,
        error: Some(message.to_string()),
    };
    render(status, &envelope)
}

/// 405 响应：失败信封 + `Allow` 头告知正确方法。
fn method_not_allowed(allow: &'static str) -> http::Response<MgmtBody> {
    let mut resp = fail(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    resp.headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
    resp
}

/// 序列化信封并落成 JSON 响应；序列化失败降级为兜底 500（细节只进日志）。
fn render<T: Serialize>(status: StatusCode, envelope: &Envelope<T>) -> http::Response<MgmtBody> {
    match serde_json::to_vec(envelope) {
        Ok(payload) => json_response(status, Bytes::from(payload)),
        Err(err) => {
            tracing::error!(error = %err, "mgmt envelope serialization failed");
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                Bytes::from_static(INTERNAL_ERROR_BODY),
            )
        }
    }
}

/// 组一个 JSON 响应（header 取常量，构造无失败路径）。
fn json_response(status: StatusCode, payload: Bytes) -> http::Response<MgmtBody> {
    let mut resp = http::Response::new(Full::new(payload));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp
}

#[cfg(test)]
#[path = "rest_tests.rs"]
mod tests;

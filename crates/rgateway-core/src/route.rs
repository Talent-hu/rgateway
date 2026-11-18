//! 路由领域模型：协议标识（[`Protocol`]）、请求上下文（[`RequestCtx`]）
//! 与路由记录（[`Route`]）。
//!
//! # 设计要点（为什么这样设计）
//!
//! * [`RequestCtx::start`] / [`RequestCtx::deadline`] 用 `tokio::time::Instant`
//!   而非 `std::time::Instant`：tokio 的时钟在测试中可用
//!   `#[tokio::test(start_paused = true)]` + `tokio::time::advance` 精确推进，
//!   使截止期（Deadline）数学可以确定性验证；生产环境下它走系统时钟，
//!   行为与 `std` 版一致。
//! * `auth` 字段用 `OnceLock` 实现 **set-once**：认证过滤器对一个请求只应
//!   执行一次，重复写入返回 `Err`，调用方按内部的重复认证错误处理。
//! * `target` 字段用 `Mutex<Option<UpstreamTarget>>` 而**不是** `OnceLock`：
//!   重试（retry）每一轮都要重新跑负载均衡、重新选实例（并跳过熔断打开的
//!   实例），因此目标必须**可覆写**。`OnceLock` 的一次性语义会把第一次的
//!   选择永久锁死，重试无法换实例。
//! * [`Route::chain`] 放进 `Mutex`：[`BoxedService`] 是 `Send` 但**非**
//!   `Sync`，而 `Route` 要以 `Arc<Route>` 在多线程间共享；`Mutex` 把非
//!   `Sync` 的服务包装成 `Sync` 字段，锁只在 [`Route::chain_clone`] 克隆
//!   服务的一瞬间持有，绝不跨 await。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::Instant;

use crate::body::BoxedService;
use crate::error::GatewayError;
use crate::governance::RouteState;
use crate::predicate::RoutePredicate;

/// 网关视角下一次请求/响应对的线上协议（wire protocol）。
///
/// 由分发层从 content-type / upgrade 头探测得出，或由路由的
/// [`ProtocolHint::Fixed`] 强制指定；后续的协议处理器与错误渲染都以此为准。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// 普通 HTTP/1.x。
    Http1,
    /// HTTP/2（非 gRPC 的普通请求/流）。
    Http2,
    /// WebSocket：upgrade 之后的双向帧通道。
    WebSocket,
    /// 原生 gRPC over HTTP/2（`application/grpc`）。
    Grpc,
    /// gRPC-Web 二进制帧封装（`application/grpc-web+proto` 等）。
    GrpcWeb,
    /// gRPC-Web 文本帧封装（`application/grpc-web-text+proto`，帧整体 base64）。
    GrpcWebText,
    /// 以 REST/JSON 形式进入、经 gRPC-JSON 转码（transcoding）转发的请求。
    RestJson,
}

impl Protocol {
    /// 是否属于 gRPC 家族：`Grpc`、`GrpcWeb` 或 `GrpcWebText`。
    ///
    /// gRPC 家族的错误渲染走 `grpc-status` / trailer 语义。
    pub fn is_grpc_family(self) -> bool {
        matches!(self, Self::Grpc | Self::GrpcWeb | Self::GrpcWebText)
    }

    /// 是否为 gRPC-Web 变体：`GrpcWeb` 或 `GrpcWebText`。
    ///
    /// gRPC-Web 需要把 trailer 编码成 body 末尾的特殊帧（文本变体还要
    /// base64），与原生 gRPC 的 HTTP/2 trailer 处理路径不同。
    pub fn is_grpc_web(self) -> bool {
        matches!(self, Self::GrpcWeb | Self::GrpcWebText)
    }

    /// 指标（metrics）用的稳定标签值——固定枚举保证基数（cardinality）有界，
    /// 永远不要把请求里的动态值用作协议标签。
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Http1 => "http1",
            Self::Http2 => "http2",
            Self::WebSocket => "ws",
            Self::Grpc => "grpc",
            Self::GrpcWeb => "grpc-web",
            Self::GrpcWebText => "grpc-web-text",
            Self::RestJson => "rest-json",
        }
    }
}

/// 路由声明的协议行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtocolHint {
    /// 默认：每个请求从 content-type / upgrade 头探测协议。
    #[default]
    Auto,
    /// 强制该路由使用固定协议（例如转码路由固定 `RestJson`）。
    Fixed(Protocol),
}

/// 路由的上游寻址方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteUri {
    /// `lb://service-name`：先经 [`crate::ServiceDiscovery`] 解析实例列表，
    /// 每次尝试（attempt）再用 [`crate::LoadBalancer`] 选一个实例——重试时
    /// 会重新解析、重新选择。
    Lb(Arc<str>),
    /// `http(s)://host:port`：直接转发到这个精确的 authority，不走服务发现。
    Direct(http::Uri),
}

impl RouteUri {
    /// 解析 `lb://name` 或绝对 http(s) URI。
    ///
    /// # 错误
    ///
    /// 返回 `Err` 的情况：`lb://` 后名称为空或含 `/`；scheme 不是
    /// http/https；缺少 authority。错误文本是内部细节，仅进日志。
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(name) = s.strip_prefix("lb://") {
            // lb 名称必须是非空的纯服务名：服务发现以名字为键，不允许带路径。
            if name.is_empty() || name.contains('/') {
                return Err(format!("invalid lb uri: {s:?}"));
            }
            return Ok(Self::Lb(Arc::from(name)));
        }
        let uri: http::Uri = s.parse().map_err(|e| format!("invalid uri {s:?}: {e}"))?;
        match uri.scheme_str() {
            Some("http") | Some("https") => {}
            other => return Err(format!("unsupported scheme {other:?} in {s:?}")),
        }
        // 直连地址必须带 authority（host 或 host:port）；端口可省略，
        // 发起连接时按 scheme 缺省取 80/443（见 rgateway-config 的
        // select_direct）。此处只校验 authority 存在。
        if uri.authority().is_none() {
            return Err(format!("uri missing authority: {s:?}"));
        }
        Ok(Self::Direct(uri))
    }

    /// 指标标签值（`lb://name` 或完整 URI 字符串）。
    pub fn as_label(&self) -> String {
        match self {
            Self::Lb(name) => format!("lb://{name}"),
            Self::Direct(uri) => uri.to_string(),
        }
    }
}

/// 调用方通过认证的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// JWT 令牌认证。
    Jwt,
    /// API Key 认证。
    ApiKey,
}

/// 客户端接入本网关的 URL scheme——`X-Forwarded-Proto` 的取值依据。
///
/// 由监听形态决定（数据面 TLS 监听 → [`Self::Https`]），不是请求头可
/// 影响的量：客户端伪造 `X-Forwarded-Proto` 会被逐跳覆写掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientScheme {
    /// 明文 HTTP 接入。
    #[default]
    Http,
    /// TLS 接入（`server.tls.enabled = true`）。
    Https,
}

impl ClientScheme {
    /// 头值文本（`http` / `https`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// 一次转发尝试（attempt）选定的上游实例。
///
/// 由目标选择（target-select）层写入 [`RequestCtx::set_target`]，
/// 供熔断 / 超时 / 处理器读取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamTarget {
    /// 熔断器 / 治理状态键用的稳定实例标识（通常是 `ip:port`）。
    pub instance_id: Arc<str>,
    /// 实际连接地址。
    pub addr: SocketAddr,
}

/// 认证过滤器建立的身份，附加到请求上下文上。
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// 主体标识（JWT 的 `sub` claim 或 API Key 名称）。
    pub subject: String,
    /// 认证方式。
    pub kind: AuthKind,
    /// 精选的 claim 名值对（仅字符串、数量有界，防止指标/日志基数爆炸）。
    pub claims: HashMap<String, String>,
}

/// 贯穿整条过滤器链的单请求上下文。
///
/// 由分发层在链运行前构造（含路由匹配结果），以 `Arc<RequestCtx>` 放进
/// request extensions 共享。晚绑定字段（auth、target）用内部可变性：
/// auth 是 set-once（`OnceLock`），target 可覆写（`Mutex`）——原因见模块
/// 文档"设计要点"。
pub struct RequestCtx {
    /// 关联 id（correlation id），对外以 `X-Request-ID` 传播。
    /// 用 uuid v7：时间有序，日志可按时间高效索引。
    pub request_id: uuid::Uuid,
    /// 探测得出（或路由强制）的协议。
    pub protocol: Protocol,
    /// 请求进入网关的时刻，`elapsed()` 的基准点。
    pub start: Instant,
    /// 由有效超时预算（timeout budget）推出的绝对截止期，可选。
    pub deadline: Option<Instant>,
    /// 下游对端地址（IP 过滤器与访问日志用）。
    pub peer_addr: Option<SocketAddr>,
    /// 客户端接入 scheme（X-Forwarded-Proto 依据）；分发层按监听形态覆写。
    pub client_scheme: ClientScheme,
    /// 匹配到的路由 id。
    pub route_id: Option<Arc<str>>,
    /// 路由匹配 / 转码时捕获的路径模板变量。
    pub path_params: HashMap<String, String>,
    // 认证结果：set-once（`OnceLock`），见模块文档"设计要点"。
    auth: OnceLock<AuthContext>,
    // 最新上游目标：可覆写（`Mutex`），因为每次重试都要重新选实例。
    target: std::sync::Mutex<Option<UpstreamTarget>>,
}

impl RequestCtx {
    /// 为刚接入的请求创建上下文：`request_id` 取 uuid v7（时间有序），
    /// `start` 取当前 tokio 时钟时刻，其余字段为空/未设。
    pub fn new(protocol: Protocol, peer_addr: Option<SocketAddr>) -> Self {
        Self {
            request_id: uuid::Uuid::now_v7(),
            protocol,
            start: Instant::now(),
            deadline: None,
            peer_addr,
            client_scheme: ClientScheme::Http,
            route_id: None,
            path_params: HashMap::new(),
            auth: OnceLock::new(),
            target: std::sync::Mutex::new(None),
        }
    }

    /// 当前尝试选定的上游目标（每次重试会被替换）。
    ///
    /// 返回 `None` 表示尚未选择。锁中毒（poisoned）时保守地视为"未选"。
    pub fn target(&self) -> Option<UpstreamTarget> {
        self.target.lock().map(|g| g.clone()).unwrap_or(None)
    }

    /// 写入本次尝试的上游目标，**覆盖**任何先前的选择——重试必须能重新跑
    /// 负载均衡，所以这里是有意的覆写语义，不是 set-once。
    pub fn set_target(&self, target: UpstreamTarget) {
        if let Ok(mut guard) = self.target.lock() {
            *guard = Some(target);
        }
    }

    /// 认证过滤器建立的身份（如有）；`None` 表示未认证。
    pub fn auth(&self) -> Option<&AuthContext> {
        self.auth.get()
    }

    /// 附加认证结果（set-once）。已设置过则返回 `Err(原值)`，调用方按
    /// 内部的重复认证（double-auth）错误处理。
    pub fn set_auth(&self, auth: AuthContext) -> Result<(), AuthContext> {
        self.auth.set(auth)
    }

    /// 自请求进入网关以来经过的时间。
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// 距绝对截止期的剩余时间（若已设置）。
    ///
    /// 用 `saturating_duration_since`：过了截止期返回 `Some(零)` 而不是
    /// 回绕/panic，调用方把零视为"已超时"即可。
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// 以相对 `start` 的预算设置绝对截止期：`deadline = start + budget`。
    pub fn set_deadline_after(&mut self, budget: Duration) {
        self.deadline = Some(self.start + budget);
    }
}

/// 编译后的路由：一组匹配谓词 + 预编译的过滤器链（终止于协议处理器）。
pub struct Route {
    /// 路由 id（配置里的标识，日志与指标用）。
    pub id: Arc<str>,
    /// 匹配优先级：数值越小越先匹配（Spring Cloud Gateway 语义）。
    pub order: i32,
    /// 上游寻址方式（`lb://` 或直连）。
    pub uri: RouteUri,
    /// 路由声明的协议行为（Auto 探测 / Fixed 强制）。
    pub protocol_hint: ProtocolHint,
    /// 匹配谓词：**全部**命中路由才适用（逻辑与）。
    pub predicates: Vec<Arc<dyn RoutePredicate>>,
    /// 治理状态（LB 计数器、熔断器、保留的过滤器状态），配置热重载时
    /// 随 `Arc` 存活延续——避免一次 reload 就把熔断器和负载均衡计数清零。
    pub state: Arc<RouteState>,
    /// 预编译链：路由过滤器 → 截止期 → 重试 → 每次尝试（熔断/超时/LB）
    /// → 协议处理器。`BoxedService` 是 `Send` 但**非** `Sync`，所以链放进
    /// `Mutex`，让 `Route`（以及 `Arc<Route>`）可跨线程共享；锁只在
    /// [`Route::chain_clone`] 克隆服务时短暂持有。
    pub chain: Mutex<BoxedService>,
}

impl Route {
    /// 所有谓词都命中时路由才适用。
    pub fn matches(&self, ctx: &RequestCtx, parts: &http::request::Parts) -> bool {
        self.predicates.iter().all(|p| p.matches(ctx, parts))
    }

    /// 为一次分发克隆出预编译链（廉价的 box 克隆）。
    ///
    /// `Mutex` 只在克隆瞬间持有，绝不跨 await；锁中毒视为内部错误。
    pub fn chain_clone(&self) -> Result<BoxedService, GatewayError> {
        self.chain
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| GatewayError::Internal("route chain lock poisoned".into()))
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;

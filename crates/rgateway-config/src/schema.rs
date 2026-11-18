//! `rgateway.yaml` 的 Serde 数据模型（原始态，位于校验之前）。
//!
//! 本模块只负责把 YAML 映射成 Rust 类型，不做语义校验（取值合法性、跨
//! 字段约束由 [`crate::validate`] 负责），也不编译成运行时结构（由
//! [`crate::compile`] 负责）。把 YAML 表面隔离在这里的目的：解析后端
//! （`serde_yaml`，上游已归档）将来可能整体替换，届时只需改本模块，
//! 编译逻辑一行都不用动。
//!
//! 字段形态约定：
//! * 时长写紧凑字符串（"300ms"、"3s"、"2m"、"1h"、"1d"），大小写紧凑
//!   字符串（"10MB"、"64KB"），直到编译阶段才由
//!   [`crate::dsl::parse_duration`] / [`crate::dsl::parse_size`] 解析——
//!   保持字符串形态，解析失败时才能报出精确的配置坐标
//!   （[`crate::error::Location`]）；
//! * 谓词/过滤器是双形态：紧凑 DSL 字符串或结构化 YAML 映射，见
//!   [`PredicateConfig`] / [`FilterConfig`]；
//! * 除 [`StructuredFilter`]（`flatten` 本来就要收集未知键）外，所有
//!   结构体都带 `deny_unknown_fields`：拼错的键立即报错，而不是被静默
//!   忽略后带着错误配置上线；配合 `#[serde(default)]`，缺失的键取内置
//!   默认值，配置文件只需写与默认不同的部分；
//! * `Default` 实现手写而非 derive——derive 会给出空串 / 0 之类的无意义
//!   出厂值，这里需要 "0.0.0.0:8080"、1024 这类可直接使用的默认值。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 配置文件的根节点，对应 `rgateway.yaml` 顶层映射。
///
/// 六个配置段全部可省略（整体取 [`Default`]）；出现未知顶层键时报错。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigFile {
    /// 数据面监听与 HTTP/2、限额、超时、TLS 设置（`server:`）。
    pub server: ServerConfig,
    /// 服务发现提供者列表（`discovery:`）。
    pub discovery: DiscoveryConfig,
    /// 全局默认过滤器 / 超时 / 负载均衡（`defaults:`）。
    pub defaults: DefaultsConfig,
    /// 路由表（`routes:`），数值小的 `order` 先匹配。
    pub routes: Vec<RouteConfig>,
    /// 访问日志、指标、tracing（`observability:`）。
    pub observability: ObservabilityConfig,
    /// 管理面监听设置（`mgmt:`）。
    pub mgmt: MgmtConfig,
}

/// 数据面监听配置（`server:`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// 数据面监听地址，`ip:port` 形式，默认 `0.0.0.0:8080`。
    pub listen: String,
    /// HTTP/2 协议参数（`server.http2:`）。
    pub http2: Http2Config,
    /// 请求体 / 并发 / 重试缓冲上限（`server.limits:`）。
    pub limits: LimitsConfig,
    /// 各阶段超时（`server.timeouts:`）。
    pub timeouts: TimeoutsConfig,
    /// TLS 监听设置（`server.tls:`）。
    pub tls: TlsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".to_string(),
            http2: Http2Config::default(),
            limits: LimitsConfig::default(),
            timeouts: TimeoutsConfig::default(),
            tls: TlsConfig::default(),
        }
    }
}

/// HTTP/2 协议参数（`server.http2:`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Http2Config {
    /// 单连接最大并发 stream 数，默认 1024。
    pub max_concurrent_streams: u32,
    /// 每个 stream 的初始流控窗口，紧凑大小写法（如 "1MB"），默认 "1MB"。
    pub initial_stream_window_size: String,
    /// 连接级初始流控窗口，紧凑大小写法（如 "64KB"），默认 "64KB"。
    pub initial_connection_window_size: String,
}

impl Default for Http2Config {
    fn default() -> Self {
        Self {
            max_concurrent_streams: 1024,
            initial_stream_window_size: "1MB".to_string(),
            initial_connection_window_size: "64KB".to_string(),
        }
    }
}

/// 请求限制（`server.limits:`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// 请求体大小上限，紧凑大小写法（如 "10MB"），默认 "10MB"。
    pub max_request_body: String,
    /// 网关同时处理的最大请求数，默认 4096。
    pub max_concurrent_requests: u32,
    /// 重试重放（retry replay）时缓冲请求体的上限，紧凑大小写法，默认 "1MB"。
    pub max_retry_buffer: String,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_request_body: "10MB".to_string(),
            max_concurrent_requests: 4096,
            max_retry_buffer: "1MB".to_string(),
        }
    }
}

/// 超时设置（`server.timeouts:`），各字段均为紧凑时长写法（如 "3s"）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutsConfig {
    /// 与上游建连超时，默认 "3s"。
    pub connect: String,
    /// 路由与 `defaults` 都未指定时的默认请求超时预算（作为请求 Deadline），
    /// 默认 "30s"。
    pub request_default: String,
    /// 优雅关闭（graceful shutdown）的等待时长，默认 "30s"。
    pub graceful_shutdown: String,
    /// WebSocket 握手超时，默认 "10s"。
    pub ws_handshake: String,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            connect: "3s".to_string(),
            request_default: "30s".to_string(),
            graceful_shutdown: "30s".to_string(),
            ws_handshake: "10s".to_string(),
        }
    }
}

/// TLS 客户端证书认证模式（`server.tls.client_auth`）。
///
/// YAML 中写小写 `none` / `optional` / `required`（`rename_all`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientAuth {
    /// 不要求客户端证书（默认）。
    #[default]
    None,
    /// 可选：允许不提供客户端证书。
    Optional,
    /// 必需：必须提供客户端证书。
    Required,
}

/// TLS 配置（`server.tls:`）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// 是否启用 TLS，默认 false。
    pub enabled: bool,
    /// PEM 证书链路径（文件变更时热重载）。
    pub cert: String,
    /// PEM 私钥路径（文件变更时热重载）。
    pub key: String,
    /// 客户端证书认证模式，默认 [`ClientAuth::None`]。
    pub client_auth: ClientAuth,
    /// 可选的 PEM CA 包路径，用于校验客户端证书。
    pub client_ca: String,
}

/// 服务发现配置（`discovery:`）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// 服务发现提供者列表，按 `type` 标签分派（`tag = "type"`）。
    pub providers: Vec<DiscoveryProvider>,
}

/// 服务发现提供者：YAML 中用 `type` 字段做内部标签分派
/// （如 `type: static`），新增提供者只需加一个变体。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DiscoveryProvider {
    /// 静态服务列表（`type: static`）。
    Static(StaticProvider),
}

/// 静态服务发现（`type: static`）：手工维护的服务名 → 实例列表映射。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticProvider {
    /// service name → instance list（服务名到实例列表的映射）。
    /// 用 [`BTreeMap`] 保证遍历顺序稳定，校验与快照输出可复现。
    pub services: BTreeMap<String, Vec<InstanceConfig>>,
}

/// 上游实例（服务发现列表中的一项）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    /// 实例地址，`ip:port` 形式。
    pub address: String,
    /// 负载均衡权重，默认 100。
    #[serde(default = "default_weight")]
    pub weight: u32,
}

/// [`InstanceConfig::weight`] 缺省时的权重 100。
fn default_weight() -> u32 {
    100
}

/// 全局默认段（`defaults:`）：先于路由级设置套用到每条路由，路由级同名
/// 设置优先（覆盖）全局默认。
///
/// 例外是 `filters`：按 Spring Cloud Gateway 语义，`defaults.filters`
/// 追加到每条路由已声明过滤器**之后**（组装顺序见 [`crate::compile`]）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    /// 全局过滤器列表，追加到每条路由过滤链末尾（[`FilterConfig`] 双形态）。
    pub filters: Vec<FilterConfig>,
    /// 作为请求 Deadline 的默认超时预算（紧凑时长，如 "30s"），可省略。
    pub request_timeout: Option<String>,
    /// 默认负载均衡算法："round_robin" | "random" | "smooth_wrr"，可省略。
    pub load_balancer: Option<String>,
}

/// 路由条目（`routes:` 列表中的一项）。
///
/// `id` 与 `uri` 必填，其余字段缺省时取默认值。本层只描述形状：
/// id 唯一性、uri 合法性、协议白名单等由 [`crate::validate`] 检查。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// 路由唯一标识，同时是错误定位里的坐标名（`routes[<id>]`）。
    pub id: String,
    /// 匹配优先级，数值小者先匹配，默认 0。
    #[serde(default)]
    pub order: i32,
    /// 上游目标：`lb://<service>`（走服务发现 + 负载均衡）或
    /// `http(s)://host:port`（直连）。
    pub uri: String,
    /// 协议：`auto`（按请求自动探测）或固定协议名，默认 "auto"。
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// 谓词列表：紧凑 DSL 字符串（`"Path=/api/**"`）或结构化映射，
    /// 决定请求是否匹配本路由。
    #[serde(default)]
    pub predicates: Vec<PredicateConfig>,
    /// 过滤器列表：紧凑 DSL 字符串或结构化映射，对请求 / 响应做处理。
    #[serde(default)]
    pub filters: Vec<FilterConfig>,
    /// gRPC-JSON 转码设置（仅 REST 路由使用）。
    #[serde(default)]
    pub transcoding: Option<TranscodingConfig>,
    /// 路由级超时预算（紧凑时长，如 "30s"），覆盖 `defaults.request_timeout`。
    #[serde(default)]
    pub request_timeout: Option<String>,
    /// 路由级负载均衡算法覆盖，覆盖 `defaults.load_balancer`。
    #[serde(default)]
    pub load_balancer: Option<String>,
}

/// [`RouteConfig::protocol`] 缺省时的协议 "auto"。
fn default_protocol() -> String {
    "auto".to_string()
}

/// gRPC-JSON 转码设置（`routes[].transcoding:`），仅 REST 路由有意义。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscodingConfig {
    /// `FileDescriptorSet` 文件路径，由 `protoc --include_imports
    /// --descriptor_set_out` 生成。
    pub descriptor: String,
    /// 显式 HTTP 映射规则，优先于 `google.api.http` 注解。
    #[serde(default)]
    pub rules: Vec<HttpRuleConfig>,
}

/// 单条 HTTP 映射规则：把一个 gRPC 方法映射到 HTTP 谓词 + URI 模板
/// （对应 `google.api.http` 的 HttpRule）。
///
/// `get`/`post`/`put`/`delete`/`patch` 五个方法字段：顶层规则必须恰好
/// 设置一个；`additional_bindings` 里的规则至少设置一个（由
/// [`crate::validate`] 检查）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpRuleConfig {
    /// 目标 RPC 方法，`package.Service/Method` 全限定形式。
    pub selector: String,
    /// HTTP GET 映射的 URI 模板。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get: Option<String>,
    /// HTTP POST 映射的 URI 模板。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<String>,
    /// HTTP PUT 映射的 URI 模板。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put: Option<String>,
    /// HTTP DELETE 映射的 URI 模板。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<String>,
    /// HTTP PATCH 映射的 URI 模板。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// `*` 或一个请求字段名，映射到 HTTP body。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// 可选：响应消息中映射到 HTTP body 的字段名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    /// 同一方法的附加映射规则（`additional_bindings`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_bindings: Vec<HttpRuleConfig>,
}

/// 谓词配置，双形态（`#[serde(untagged)]`，按声明顺序尝试匹配）：
/// * 紧凑 DSL 字符串：`"Method=GET,POST"`——适合参数少的谓词，一行写完；
/// * 结构化映射：`{ name: Method, args: [GET, POST] }`——适合参数多、
///   需要 YAML 多行 / 引号转义的场景。
///
/// 紧凑字符串在编译阶段由 [`crate::dsl::parse_named_args`] 拆成
/// `(name, args)`，此后两种形态完全等价。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PredicateConfig {
    /// 紧凑 DSL 形态：`"Name=arg1,arg2"`。
    Compact(String),
    /// 结构化形态：`name` + `args` 列表。
    Structured(StructuredPredicate),
}

/// 结构化谓词：`name` + 位置参数列表。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredPredicate {
    /// 谓词名（如 `Method`、`Path`、`Header`）。
    pub name: String,
    /// 位置参数列表，默认空。
    #[serde(default)]
    pub args: Vec<String>,
}

/// 过滤器配置，双形态（`#[serde(untagged)]`）：
/// * 紧凑 DSL 字符串：`"RateLimit=key:client_ip,rate:100,period:1s"`——
///   `k:v` 逗号分隔，值一律是字符串；
/// * 结构化映射：`{ name: Retry, retries: 3, methods: [GET], ... }`——
///   参数值保留 YAML 原生类型（数字、布尔、嵌套 map 如 `backoff`），
///   适合参数多、类型丰富的过滤器。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum FilterConfig {
    /// 紧凑 DSL 形态：`"Name=k:v,k:v"`。
    Compact(String),
    /// 结构化形态：`name` + 自由参数表。
    Structured(StructuredFilter),
}

/// 结构化过滤器：`name` + 其余键值参数。
///
/// `#[serde(flatten)]` 把未知键收进 `params`，因此本结构体不能（serde
/// 层面）也不需要 `deny_unknown_fields`——参数语义由
/// [`crate::spec::FilterSpec`] 在编译阶段按名解释。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StructuredFilter {
    /// 过滤器名（如 `Retry`、`RateLimit`、`RewritePath`）。
    pub name: String,
    /// 自由参数表：除 `name` 外的所有键值。
    #[serde(flatten)]
    pub params: BTreeMap<String, ParamValue>,
}

/// 结构化过滤器参数携带的 YAML 值（字符串 / 数字 / 布尔 / 序列 / 映射）。
pub type ParamValue = serde_yaml::Value;

/// 访问日志输出格式（`observability.access_log.format`）。
///
/// YAML 中写小写 `json` / `text`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// JSON 行格式（默认）。
    #[default]
    Json,
    /// 纯文本格式。
    Text,
}

/// 访问日志配置（`observability.access_log:`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessLogConfig {
    /// 是否开启访问日志，默认 true。
    pub enabled: bool,
    /// 日志格式，默认 [`LogFormat::Json`]。
    pub format: LogFormat,
}

impl Default for AccessLogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            format: LogFormat::Json,
        }
    }
}

/// 指标配置（`observability.metrics:`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// 是否开启 Prometheus 指标，默认 true。
    pub enabled: bool,
    /// 管理面上暴露指标的路径，默认 "/metrics"。
    pub path: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/metrics".to_string(),
        }
    }
}

/// 可观测性配置（`observability:`）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// 访问日志。
    pub access_log: AccessLogConfig,
    /// Prometheus 指标。
    pub metrics: MetricsConfig,
    /// tracing 过滤表达式（RUST_LOG 语法），可省略。
    pub tracing_filter: Option<String>,
}

/// 管理面配置（`mgmt:`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MgmtConfig {
    /// 管理面监听地址，`ip:port`（REST + 可选 gRPC），默认 `127.0.0.1:9090`。
    pub listen: String,
    /// 是否同时提供 gRPC 管理服务，默认 true。
    pub grpc: bool,
}

impl Default for MgmtConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9090".to_string(),
            grpc: true,
        }
    }
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;

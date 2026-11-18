//! 编译管线：已校验的 [`ConfigFile`] → 不可变的 [`CompiledConfig`]（可运行快照）。
//!
//! 本模块是「配置 → 运行期产物」的总入口，职责：
//! - 把 `server:` 段解析为 typed 的 [`ServerSettings`]（compact 时长/大小已解析）；
//! - 由 `discovery.providers` 块构建静态服务发现表；
//! - 逐路由组装谓词与过滤器链（子模块 `chain`），汇总为 [`RouteTable`]；
//! - 原样汇总 mgmt / observability 设置，产出一次性编译的 [`CompiledConfig`]。
//!
//! 过滤器链组装顺序（外 → 内），每条路由在编译期一次性完成：
//!   Compression → CORS → RateLimit → Auth → Route 过滤器（config 顺序）→
//!   Deadline → Retry → TargetSelect（LB / direct，跳过熔断打开的实例）→
//!   CircuitBreaker（实例级）→ Timeout（单次尝试）→ Handler。
//!
//! 顺序设计的关键点：TargetSelect 放在实例级 Circuit Breaker **之前**，
//! 这样 Retry 每次重试都能重新选实例并跳过熔断打开的实例（设计要求）。
//! 所有过滤器在编译期一次性组装成 `BoxedService` 链，请求处理期零动态
//! 过滤器查找、零配置解析。
//!
//! 产物整体不可变并包在 `Arc` 中，热重载时原子替换，请求路径始终看到
//! 完整一致的配置视图；治理状态（LB 计数器、熔断器、限流桶等）经
//! [`StateRegistry`] 按路由 id 跨重载存活。管线中 [`crate::validate`]
//! 先行校验，compile 对坏输入仍须干净地失败（带 [`Location`] 的错误）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rgateway_core::{Instance, ServiceDiscovery, StateRegistry, StaticDiscovery};
use rgateway_router::RouteTable;

use crate::error::{ConfigError, Location};
use crate::schema::{ClientAuth, ConfigFile, DiscoveryConfig, DiscoveryProvider, LogFormat};
use crate::spec::{FilterRegistry, RouteHandlerFactory};

#[path = "compile_chain.rs"]
mod chain;
#[path = "compile_settings.rs"]
mod settings;
#[cfg(test)]
#[path = "compile_tests.rs"]
mod tests;

/// TLS 设置，证书/私钥/CA 路径已解析为绝对路径。
///
/// `cert`/`key` 为空 `PathBuf` 表示未配置（`enabled = false` 时的常态）；
/// `client_ca` 仅在配置了客户端证书校验材料时为 `Some`。
#[derive(Debug, Clone, Default)]
pub struct TlsSettings {
    /// 是否启用 TLS。
    pub enabled: bool,
    /// 服务端证书（PEM）路径；未配置时为空。
    pub cert: PathBuf,
    /// 服务端私钥（PEM）路径；未配置时为空。
    pub key: PathBuf,
    /// 客户端证书校验模式（不校验/可选/强制）。
    pub client_auth: ClientAuth,
    /// 校验客户端证书的 CA 证书路径；未配置时为 `None`。
    pub client_ca: Option<PathBuf>,
}

/// 完整解析后的 server 设置：所有 compact 大小/时长都已转换为 typed 值。
#[derive(Debug, Clone)]
pub struct ServerSettings {
    /// 监听地址。
    pub listen: SocketAddr,
    /// HTTP/2 单连接最大并发流数。
    pub max_concurrent_streams: u32,
    /// HTTP/2 流级初始窗口大小。
    pub initial_stream_window_size: usize,
    /// HTTP/2 连接级初始窗口大小。
    pub initial_connection_window_size: usize,
    /// 单个请求体的大小上限。
    pub max_request_body: usize,
    /// 网关全局并发请求数上限。
    pub max_concurrent_requests: u32,
    /// Retry 重放请求体时允许缓冲的最大字节数。
    pub max_retry_buffer: usize,
    /// 与上游建连的超时。
    pub connect_timeout: Duration,
    /// 总 Deadline 默认预算：路由与 defaults 都未设 `request_timeout` 时使用。
    pub request_default: Duration,
    /// 优雅关闭的等待时长。
    pub graceful_shutdown: Duration,
    /// WebSocket 握手超时。
    pub ws_handshake: Duration,
    /// TLS 设置（路径已解析）。
    pub tls: TlsSettings,
}

/// 一次编译产出的全部内容（可运行快照）。
///
/// 整体包在 `Arc` 中，热重载时原子替换（见 `snapshot` 模块），因此任何
/// 时刻的请求都在同一个一致的快照上工作。
pub struct CompiledConfig {
    /// 解析后的 server / TLS 设置。
    pub server: ServerSettings,
    /// 编译好的路由表（含逐路由预组装的过滤器链）。
    pub routes: RouteTable,
    /// 服务发现（当前实现为静态表）。
    pub discovery: Arc<dyn ServiceDiscovery>,
    /// 治理状态注册表，与旧快照共享，跨热重载存活。
    pub states: Arc<StateRegistry>,
    /// 管理面监听地址。
    pub mgmt_listen: SocketAddr,
    /// 管理面是否启用 gRPC 接口。
    pub mgmt_grpc: bool,
    /// 是否开启访问日志。
    pub access_log_enabled: bool,
    /// 访问日志是否为 JSON 格式（`false` 为 text）。
    pub access_log_json: bool,
    /// 是否开启 metrics 端点。
    pub metrics_enabled: bool,
    /// metrics 端点路径。
    pub metrics_path: String,
    /// tracing filter 指令（如 `rgateway=debug`）。
    pub tracing_filter: Option<String>,
}

/// [`compile`] 的输入。
pub struct CompileInput<'a> {
    /// 已通过 [`crate::validate::validate`] 的原始配置。
    pub file: &'a ConfigFile,
    /// 过滤器工厂注册表（canonical 名字 → 构建器）。
    pub registry: &'a FilterRegistry,
    /// 治理状态注册表，按路由 id 跨热重载复用。
    pub states: &'a Arc<StateRegistry>,
    /// 最内层协议处理器工厂（由 rgateway-proxy 注入，使本 crate 不依赖代理实现）。
    pub handler_factory: &'a dyn RouteHandlerFactory,
    /// 配置文件所在目录；相对路径（TLS 材料、transcoding 描述符）以此解析。
    pub config_dir: &'a std::path::Path,
}

/// 校验并编译一份配置为过滤器链与路由表。
///
/// 流程：解析 [`ServerSettings`] → 构建静态服务发现 → 逐路由编译
/// （谓词 + 按槽位组装的过滤器链 + 目标选择器 + 总 Deadline 预算）→
/// 汇总 [`RouteTable`] 与 mgmt / observability 设置。
///
/// # 参数
/// 见 [`CompileInput`]：`file` 为待编译配置，`registry` 提供过滤器工厂，
/// `states` 提供跨重载的治理状态，`handler_factory` 产出最内层服务，
/// `config_dir` 是所有相对路径的解析基准。
///
/// # 返回
/// 成功时返回 [`CompiledConfig`]，其中 `states` 与输入共享同一 `Arc`。
///
/// # 错误
/// 任一字段解析失败（listen 地址、compact 时长/大小、TLS 路径、协议名、
/// 谓词/过滤器名、transcoding 描述符路径等）都返回携带精确 [`Location`]
/// 的 [`ConfigError`]，不会 panic。
pub fn compile(input: CompileInput<'_>) -> Result<CompiledConfig, ConfigError> {
    let server = settings::parse_server(&input.file.server, input.config_dir)?;
    let discovery = build_discovery(&input.file.discovery)?;
    let mut routes = Vec::with_capacity(input.file.routes.len());
    for route in &input.file.routes {
        let compiled = chain::compile_route(route, &input, &discovery, &server)?;
        routes.push(Arc::new(compiled));
    }
    Ok(CompiledConfig {
        server,
        routes: RouteTable::new(routes),
        discovery,
        states: Arc::clone(input.states),
        mgmt_listen: parse_listen(
            &Location::section("mgmt").field("listen"),
            &input.file.mgmt.listen,
        )?,
        mgmt_grpc: input.file.mgmt.grpc,
        access_log_enabled: input.file.observability.access_log.enabled,
        access_log_json: input.file.observability.access_log.format == LogFormat::Json,
        metrics_enabled: input.file.observability.metrics.enabled,
        metrics_path: input.file.observability.metrics.path.clone(),
        tracing_filter: input.file.observability.tracing_filter.clone(),
    })
}

/// 由 `discovery.providers[type=static]` 块构建静态服务发现表。
///
/// 实例 id 默认取 `ip:port` 字符串（见 [`Instance::new`]）。同一个服务名
/// 只允许由一个 provider 声明：重复声明返回带 [`Location`] 的错误，而不是
/// 静默地只保留最后一个 provider 的实例列表（后写覆盖会掩盖配置笔误）。
///
/// # 错误
/// 地址解析失败或服务名重复时返回 [`ConfigError`]，位置分别精确到
/// `discovery.providers[i].services[name][j].address` 与
/// `discovery.providers[i].services[name]`。
fn build_discovery(config: &DiscoveryConfig) -> Result<Arc<dyn ServiceDiscovery>, ConfigError> {
    let mut services: HashMap<Arc<str>, Vec<Instance>> = HashMap::new();
    for (i, provider) in config.providers.iter().enumerate() {
        let DiscoveryProvider::Static(statics) = provider;
        for (name, instances) in &statics.services {
            let mut list = Vec::with_capacity(instances.len());
            for (j, inst) in instances.iter().enumerate() {
                let loc = Location::indexed("discovery.providers", i)
                    .field(&format!("services[{name}][{j}].address"));
                let addr = parse_listen(&loc, &inst.address)?;
                list.push(Instance::new(addr, inst.weight));
            }
            let key = Arc::from(name.as_str());
            if services.contains_key(&key) {
                return Err(ConfigError::value(
                    Location::indexed("discovery.providers", i).field(&format!("services[{name}]")),
                    format!(
                        "duplicate service name {name:?}: declared by more than one \
                         provider, merge the entries into a single provider"
                    ),
                ));
            }
            services.insert(key, list);
        }
    }
    Ok(Arc::new(StaticDiscovery::new(services)))
}

/// 解析 `ip:port` 形式的 socket 地址；失败时错误携带给定的 [`Location`]。
pub(super) fn parse_listen(loc: &Location, raw: &str) -> Result<SocketAddr, ConfigError> {
    raw.parse::<SocketAddr>().map_err(|e| {
        ConfigError::value(loc.clone(), format!("invalid listen address {raw:?}: {e}"))
    })
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    fn providers(yaml: &str) -> DiscoveryConfig {
        serde_yaml::from_str(yaml).expect("yaml must parse")
    }

    /// 验证：两个 provider 声明同名 service 必须报错（而不是后者静默覆盖前者），
    /// 且错误位置精确到第二个 provider 的该服务名。
    #[test]
    fn duplicate_service_name_across_providers_is_rejected() {
        let config = providers(
            r#"providers:
  - type: static
    services: { user-svc: [{ address: "127.0.0.1:50051" }] }
  - type: static
    services: { user-svc: [{ address: "127.0.0.1:50052" }] }"#,
        );
        let err = match build_discovery(&config) {
            Err(err) => err,
            Ok(_) => panic!("duplicate service name must fail"),
        };
        match err {
            ConfigError::Value { location, message } => {
                assert_eq!(
                    location.to_string(),
                    "discovery.providers[1].services[user-svc]"
                );
                assert!(message.contains("duplicate"), "{message}");
            }
            other => panic!("expected ConfigError::Value, got {other:?}"),
        }
    }

    /// 验证：不同 provider 声明的不同服务名全部注册成功，未知服务查得空列表。
    #[test]
    fn distinct_services_across_providers_all_register() {
        let config = providers(
            r#"providers:
  - type: static
    services: { svc-a: [{ address: "127.0.0.1:50051" }] }
  - type: static
    services: { svc-b: [{ address: "127.0.0.1:50052" }] }"#,
        );
        let discovery = build_discovery(&config).expect("distinct services compile");
        assert_eq!(discovery.get("svc-a").len(), 1);
        assert_eq!(discovery.get("svc-b").len(), 1);
        assert_eq!(discovery.get("unknown").len(), 0);
    }
}

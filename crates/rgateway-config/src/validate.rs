//! 语义校验（带精确定位）。
//!
//! 分层位置：schema 反序列化之后、compile 之前——只看解析出的结构，不
//! 触碰运行时。所有问题一次性收集进 Vec 而不是遇错即停：一次配置重载
//! 要报告全部错误，避免「改一处、报一处」的来回折腾。空结果表示可以
//! 安全编译。

use std::collections::HashSet;

use rgateway_core::predicate::full_match_regex;
use rgateway_core::RouteUri;

use crate::dsl;
use crate::error::{ConfigError, Location};
use crate::schema::{ClientAuth, ConfigFile, RouteConfig};
use crate::spec::PredicateSpec;

mod filter_params;

use filter_params::validate_filter_list;

/// 管理面 REST 的保留路径（与 `rgateway-mgmt` 的 `rest::handle` 静态路由
/// 一致）。`observability.metrics.path` 不得与它们冲突——冲突会让指标端点
/// 或管理端点之一永远不可达。mgmt crate 侧有对拍测试保证双向一致。
pub const MGMT_RESERVED_PATHS: [&str; 6] = [
    "/health",
    "/mgmt/v1/snapshot",
    "/mgmt/v1/routes",
    "/mgmt/v1/breakers",
    "/mgmt/v1/stats",
    "/mgmt/v1/breakers/reset",
];

/// 校验已解析的配置；返回空 Vec 表示可以安全编译。
pub fn validate(file: &ConfigFile) -> Vec<ConfigError> {
    let mut errors = Vec::new();

    validate_server(file, &mut errors);
    validate_observability(file, &mut errors);
    validate_discovery(file, &mut errors);
    validate_defaults(file, &mut errors);
    validate_routes(file, &mut errors);

    errors
}

/// 校验并在第一个错误处失败（只取第一个错误；批量报告请用 [`validate`]）。
pub fn ensure_valid(file: &ConfigFile) -> Result<(), ConfigError> {
    let errors = validate(file);
    match errors.into_iter().next() {
        None => Ok(()),
        Some(err) => Err(err),
    }
}

/// observability 段：`metrics.path` 必须是绝对路径（`/` 开头）、不含空白 /
/// 查询 / 片段分隔符，且不与 [`MGMT_RESERVED_PATHS`] 冲突。
///
/// 指标路径是 Prometheus scrape 的输入，也是管理面 URL 空间的一部分：
/// 留白或 `?`/`#` 会拼出畸形抓取地址，撞上保留路径则让指标端点或某个
/// 管理端点永远 404——两类问题都必须在加载期拒绝，而不是运行期静默。
fn validate_observability(file: &ConfigFile, errors: &mut Vec<ConfigError>) {
    let loc = Location::section("observability");
    let path = file.observability.metrics.path.as_str();
    let syntax_ok = path.starts_with('/')
        && path.len() > 1
        && !path
            .chars()
            .any(|c| c.is_whitespace() || c == '?' || c == '#');
    if !syntax_ok {
        errors.push(ConfigError::value(
            loc.field("metrics.path"),
            format!("metrics path must be an absolute path without spaces or query: {path:?}"),
        ));
        return;
    }
    if MGMT_RESERVED_PATHS.contains(&path) {
        errors.push(ConfigError::value(
            loc.field("metrics.path"),
            format!("metrics path collides with a management endpoint: {path:?}"),
        ));
    }
}

/// server 段：listen 地址可解析、size/duration 字段语法合法（duration
/// 且 > 0）、`tls.enabled` 时 cert/key 必填、并发上限 > 0。
fn validate_server(file: &ConfigFile, errors: &mut Vec<ConfigError>) {
    let loc = Location::section("server");
    if file.server.listen.parse::<std::net::SocketAddr>().is_err() {
        errors.push(ConfigError::value(
            loc.field("listen"),
            format!("invalid listen address {:?}", file.server.listen),
        ));
    }
    for (field, value) in [
        (
            "http2.initial_stream_window_size",
            file.server.http2.initial_stream_window_size.as_str(),
        ),
        (
            "http2.initial_connection_window_size",
            file.server.http2.initial_connection_window_size.as_str(),
        ),
        (
            "limits.max_request_body",
            file.server.limits.max_request_body.as_str(),
        ),
        (
            "limits.max_retry_buffer",
            file.server.limits.max_retry_buffer.as_str(),
        ),
    ] {
        if let Err(m) = dsl::parse_size(value) {
            errors.push(ConfigError::value(
                Location::section("server").field(field),
                m,
            ));
        }
    }
    for (field, value) in [
        ("timeouts.connect", file.server.timeouts.connect.as_str()),
        (
            "timeouts.request_default",
            file.server.timeouts.request_default.as_str(),
        ),
        (
            "timeouts.graceful_shutdown",
            file.server.timeouts.graceful_shutdown.as_str(),
        ),
        (
            "timeouts.ws_handshake",
            file.server.timeouts.ws_handshake.as_str(),
        ),
    ] {
        match dsl::parse_duration(value) {
            Err(m) => errors.push(ConfigError::value(
                Location::section("server").field(field),
                m,
            )),
            Ok(d) if d.is_zero() => errors.push(ConfigError::value(
                Location::section("server").field(field),
                "duration must be > 0",
            )),
            Ok(_) => {}
        }
    }
    if file.server.tls.enabled {
        for (field, value) in [
            ("tls.cert", file.server.tls.cert.as_str()),
            ("tls.key", file.server.tls.key.as_str()),
        ] {
            if value.is_empty() {
                errors.push(ConfigError::value(
                    Location::section("server").field(field),
                    "required when tls.enabled",
                ));
            }
        }
        // client_auth != none 时必须提供 client_ca：没有 CA 就无法验证
        // 客户端证书，mTLS 配置在加载期即拒绝而非起一个形同虚设的监听。
        if file.server.tls.client_auth != ClientAuth::None && file.server.tls.client_ca.is_empty() {
            errors.push(ConfigError::value(
                Location::section("server").field("tls.client_ca"),
                "required when tls.client_auth is not none",
            ));
        }
    }
    if file.server.limits.max_concurrent_requests == 0 {
        errors.push(ConfigError::value(
            Location::section("server").field("limits.max_concurrent_requests"),
            "must be > 0",
        ));
    }
}

/// discovery 段（目前只有 Static 提供方）：服务至少一个实例、实例
/// address 可解析为 SocketAddr、weight > 0——摘流走 discovery removal，
/// 不允许用 weight=0 表达下线。
fn validate_discovery(file: &ConfigFile, errors: &mut Vec<ConfigError>) {
    for (i, provider) in file.discovery.providers.iter().enumerate() {
        let loc = Location::indexed("discovery.providers", i);
        match provider {
            crate::schema::DiscoveryProvider::Static(p) => {
                for (svc, instances) in &p.services {
                    let svc_loc = loc.field(&format!("services[{svc}]"));
                    if instances.is_empty() {
                        errors.push(ConfigError::value(
                            svc_loc.clone(),
                            "service has no instances",
                        ));
                    }
                    for (j, inst) in instances.iter().enumerate() {
                        let iloc = Location::indexed(&svc_loc.to_string(), j);
                        if inst.address.parse::<std::net::SocketAddr>().is_err() {
                            errors.push(ConfigError::value(
                                iloc.field("address"),
                                format!("invalid socket address {:?}", inst.address),
                            ));
                        }
                        if inst.weight == 0 {
                            errors.push(ConfigError::value(
                                iloc.field("weight"),
                                "weight must be > 0 (use discovery removal to drain)",
                            ));
                        }
                    }
                }
            }
        }
    }
}

/// 路由逐条校验。分段意图（函数超 50 行是既有债务，本次不重构）：
/// id 非空且去重 → uri（lb 服务存在性）→ protocol 白名单 → transcoding
/// 前置条件与 rules → predicates → filters。
fn validate_routes(file: &ConfigFile, errors: &mut Vec<ConfigError>) {
    let mut ids = HashSet::new();
    // 已知服务名集合：lb:// 路由的 uri 必须指向存在的服务
    let known_services: HashSet<&str> = file
        .discovery
        .providers
        .iter()
        .flat_map(|p| match p {
            crate::schema::DiscoveryProvider::Static(s) => s.services.keys().map(|s| s.as_str()),
        })
        .collect();

    for route in &file.routes {
        let loc = Location::route(&route.id);
        if route.id.trim().is_empty() {
            errors.push(ConfigError::value(loc, "route id must not be empty"));
            continue;
        }
        if !ids.insert(route.id.as_str()) {
            errors.push(ConfigError::value(loc.clone(), "duplicate route id"));
        }
        match RouteUri::parse(&route.uri) {
            Ok(RouteUri::Lb(name)) => {
                if !known_services.contains(name.as_ref()) {
                    errors.push(ConfigError::value(
                        loc.field("uri"),
                        format!("unknown discovery service {name:?}"),
                    ));
                }
            }
            // 直连地址不依赖 discovery，无需存在性检查
            Ok(RouteUri::Direct(_)) => {}
            Err(m) => errors.push(ConfigError::value(loc.field("uri"), m)),
        }
        match route.protocol.as_str() {
            "auto" | "http1" | "http2" | "grpc" | "grpc-web" | "grpc-web-text" | "rest-json"
            | "websocket" => {}
            other => errors.push(ConfigError::value(
                loc.field("protocol"),
                format!("unknown protocol {other:?}"),
            )),
        }
        // transcoding 只在 auto/rest-json 两种 protocol 下有意义
        if route.transcoding.is_some() && route.protocol != "auto" && route.protocol != "rest-json"
        {
            errors.push(ConfigError::value(
                loc.field("transcoding"),
                "transcoding requires protocol auto or rest-json",
            ));
        }
        if let Some(t) = &route.transcoding {
            if t.descriptor.trim().is_empty() {
                errors.push(ConfigError::value(
                    loc.field("transcoding.descriptor"),
                    "descriptor path must not be empty",
                ));
            }
            for (i, rule) in t.rules.iter().enumerate() {
                let rloc = Location::route_item(&route.id, "transcoding.rules", i);
                // selector 必须是 package.Service/Method 全限定形式（含 `/`）
                if !rule.selector.contains('/') {
                    errors.push(ConfigError::value(
                        rloc.field("selector"),
                        format!(
                            "selector must be package.Service/Method, got {:?}",
                            rule.selector
                        ),
                    ));
                }
                // 每条 rule 必须恰好绑定一个 HTTP 方法
                let methods = [
                    rule.get.as_deref(),
                    rule.post.as_deref(),
                    rule.put.as_deref(),
                    rule.delete.as_deref(),
                    rule.patch.as_deref(),
                ]
                .iter()
                .filter(|m| m.is_some())
                .count();
                if methods != 1 {
                    errors.push(ConfigError::value(
                        rloc.clone(),
                        format!("exactly one of get/post/put/delete/patch required, got {methods}"),
                    ));
                }
                // additional_bindings 同样必须至少带一个 HTTP 方法
                for (j, add) in rule.additional_bindings.iter().enumerate() {
                    if add.get.is_none()
                        && add.post.is_none()
                        && add.put.is_none()
                        && add.delete.is_none()
                        && add.patch.is_none()
                    {
                        errors.push(ConfigError::value(
                            Location::indexed(&format!("{rloc}.additional_bindings"), j),
                            "binding requires an http method",
                        ));
                    }
                }
            }
        }
        validate_predicates(route, errors);
        validate_filters(route, errors);
    }
}

/// 谓词按名校验：`Path`/`Method`/`Header`/`Host`/`Query` 至少一个参数；
/// `Path` 的每个模式过 ant 语法校验；`Header`/`Query` 恰好 2 个参数
/// （name, regex）且正则必须能编译。未知名字报 `UnknownPredicate`。
fn validate_predicates(route: &RouteConfig, errors: &mut Vec<ConfigError>) {
    for (i, p) in route.predicates.iter().enumerate() {
        let ploc = Location::route_item(&route.id, "predicates", i);
        let spec = match PredicateSpec::from_config(p) {
            Ok(s) => s,
            Err(mut e) => {
                // 解析失败的粗定位改写到该谓词条目
                e = relocate(e, &ploc);
                errors.push(e);
                continue;
            }
        };
        match spec.name.as_str() {
            "Path" | "Method" | "Header" | "Host" | "Query" => {
                if spec.args.is_empty() {
                    errors.push(ConfigError::value(
                        ploc.clone(),
                        format!("{} needs at least one argument", spec.name),
                    ));
                }
                if spec.name == "Path" {
                    for arg in &spec.args {
                        if let Err(m) = dsl::validate_ant_pattern(arg) {
                            errors.push(ConfigError::value(ploc.clone(), m));
                        }
                    }
                }
                if matches!(spec.name.as_str(), "Header" | "Query") {
                    if spec.args.len() != 2 {
                        errors.push(ConfigError::value(
                            ploc.clone(),
                            format!("{} expects exactly 2 arguments (name, regex)", spec.name),
                        ));
                    } else if let Err(m) = full_match_regex(&spec.args[1]) {
                        // 按 compile 实际构建的全匹配锚定形式（`^(?:P)$`）
                        // 校验正则，让非法正则提前在这里报错而不是编译期。
                        errors.push(ConfigError::value(
                            ploc.field("regex"),
                            format!("invalid regex: {m}"),
                        ));
                    }
                }
            }
            other => errors.push(ConfigError::UnknownPredicate {
                location: ploc,
                name: other.to_string(),
            }),
        }
    }
}

/// 路由声明的过滤器逐条校验，定位 `routes[id].filters[i]`。
fn validate_filters(route: &RouteConfig, errors: &mut Vec<ConfigError>) {
    validate_filter_list(
        &route.filters,
        &|i| Location::route_item(&route.id, "filters", i),
        errors,
    );
}

/// `defaults.filters` 与路由过滤器走完全相同的按名参数校验（默认过滤器
/// 会追加到每条路由上，缺参同样致命），定位 `defaults.filters[<i>]`。
fn validate_defaults(file: &ConfigFile, errors: &mut Vec<ConfigError>) {
    validate_filter_list(
        &file.defaults.filters,
        &|i| Location::indexed("defaults.filters", i),
        errors,
    );
}

/// 把错误的 [`Location`] 替换为更精确的条目定位——解析器只产生粗粒度
/// 定位（如 `filter` 段），调用方知道具体坐标。
fn relocate(mut err: ConfigError, loc: &Location) -> ConfigError {
    match &mut err {
        ConfigError::Io { location, .. }
        | ConfigError::Yaml { location, .. }
        | ConfigError::Value { location, .. }
        | ConfigError::Invalid { location, .. }
        | ConfigError::Compile { location, .. }
        | ConfigError::UnknownFilter { location, .. }
        | ConfigError::UnknownPredicate { location, .. } => *location = loc.clone(),
    }
    err
}

/// compile 使用的便捷包装：把谓词/过滤器解析错误的定位改写到 `loc`。
pub fn at<T>(loc: &Location, result: Result<T, ConfigError>) -> Result<T, ConfigError> {
    result.map_err(|e| relocate(e, loc))
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;

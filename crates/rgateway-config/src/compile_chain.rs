//! 单路由链组装：谓词、过滤器槽位、目标选择与预编译的 `BoxedService` 栈。
//!
//! 包装顺序「由内向外」依次进行，最终请求穿过的链为（外 → 内）：
//!   Compression → CORS → RateLimit → Auth → Route（config 顺序）→ Deadline →
//!   Retry → Target → Breaker → Timeout → Handler。
//!
//! 两个关键次序决策：
//! - Target 位于 Retry 与 Breaker 之间：每次重试都要重新执行目标选择
//!   （重新 LB 选实例，并跳过熔断打开的实例），把本次选中的实例写入
//!   `ctx.target`；而实例级 Circuit Breaker 判定的是「本次选中的这个实例」
//!   能否放行，所以必须在选择之后、随每次尝试重新执行。
//! - Deadline 在 Retry 之外：总截止预算约束整个请求（含所有重试），
//!   而单次尝试的超时由内侧的 Timeout 槽负责。Deadline 只约束拿到响应头
//!   的时刻，已通过截止期的响应体会继续流式传输。
//!
//! 一个路由的「生效过滤器列表」= 路由自身声明的 filters 之后**追加**
//! `defaults.filters`（Spring Cloud Gateway 语义：default filters 追加到
//! 每个路由之后）。所有条目按规范槽位 [`FilterSlot`] 分组，槽内保持
//! config 顺序，因此同一槽位内路由声明的过滤器包在默认过滤器**外侧**
//! （先于默认过滤器执行）。

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{HeaderName, Method};
use rgateway_core::{
    BoxedService, HeaderPredicate, HostPredicate, LoadBalancer, MethodPredicate, Protocol,
    ProtocolHint, QueryPredicate, RandomBalancer, RoundRobin, Route, RoutePredicate, RouteState,
    RouteUri, ServiceDiscovery, SmoothWeightedRoundRobin,
};
use rgateway_router::PathPredicate;

use crate::deadline::DeadlineLayer;
use crate::error::{ConfigError, Location};
use crate::schema::{DefaultsConfig, FilterConfig, RouteConfig, TranscodingConfig};
use crate::spec::{
    slot_for, FilterBuildCtx, FilterRegistry, FilterSlot, FilterSpec, PredicateSpec,
    RouteCompileInfo,
};
use crate::target::TargetSelector;
use crate::validate;

use super::settings::{parse_duration_at, resolve_in_dir};
use super::{CompileInput, ServerSettings};

/// 路由负载均衡器 holder 在 [`RouteState::preserved`] 中的槽位键。
const BALANCER_SLOT: &str = "lb";
/// 路由与 defaults 都未指定 `load_balancer` 时使用的默认算法。
const DEFAULT_BALANCER: &str = "round_robin";

/// 归一化后的单个过滤器，附带它在配置中的精确位置
/// （`routes[id].filters[i]` 或 `defaults.filters[i]`），错误定位用。
struct IndexedFilter {
    loc: Location,
    spec: FilterSpec,
}

/// 按规范槽位分组的过滤器集合；同一槽位内保持 config 顺序。
struct Slotted {
    by_slot: BTreeMap<FilterSlot, Vec<IndexedFilter>>,
}

impl Slotted {
    fn push(&mut self, slot: FilterSlot, filter: IndexedFilter) {
        self.by_slot.entry(slot).or_default().push(filter);
    }

    /// 取某一槽位的过滤器列表（config 顺序）；无条目时返回空切片。
    fn slot(&self, slot: FilterSlot) -> &[IndexedFilter] {
        self.by_slot.get(&slot).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// 槽位包装共用的过滤器注册表 + 构建上下文。
struct ChainCtx<'a> {
    registry: &'a FilterRegistry,
    slotted: &'a Slotted,
    build: &'a FilterBuildCtx,
}

impl ChainCtx<'_> {
    /// 把某一槽位的过滤器层包到 `chain` 外面，按 config **逆序**包装，
    /// 使先列出的过滤器最终位于槽内最外层（路由声明的过滤器排在
    /// `defaults.filters` 之前，因此先于默认过滤器执行）。
    fn wrap(&self, slot: FilterSlot, mut chain: BoxedService) -> Result<BoxedService, ConfigError> {
        for f in self.slotted.slot(slot).iter().rev() {
            let loc = f.loc.clone();
            let factory = self.registry.get(f.spec.name.as_str()).ok_or_else(|| {
                ConfigError::UnknownFilter {
                    location: loc.clone(),
                    name: f.spec.name.clone(),
                }
            })?;
            let wrapper = factory.build(&f.spec, self.build).map_err(|e| {
                ConfigError::compile(
                    loc,
                    format!("filter {:?} failed to build: {e}", f.spec.name),
                )
            })?;
            chain = wrapper(chain);
        }
        Ok(chain)
    }
}

/// 把一个路由配置编译为带预组装链的 [`Route`]。
///
/// 流程：解析 [`RouteUri`] 与协议提示 → 构建谓词列表 → 过滤器按槽位
/// 归类 → 取（或复用）该路由的治理状态 → 组装 [`RouteCompileInfo`] 并
/// 交给 handler 工厂产出最内层服务 → 构建目标选择器与总 Deadline 预算 →
/// 由内向外组装过滤器链 → 返回 [`Route`]。
///
/// # 错误
/// uri / protocol / 谓词 / 过滤器 / transcoding 路径 / 时长解析任一失败，
/// 返回携带精确 [`Location`] 的 [`ConfigError`]。
pub(super) fn compile_route(
    route: &RouteConfig,
    input: &CompileInput<'_>,
    discovery: &Arc<dyn ServiceDiscovery>,
    server: &ServerSettings,
) -> Result<Route, ConfigError> {
    let loc = Location::route(&route.id);
    let uri = RouteUri::parse(&route.uri).map_err(|m| ConfigError::value(loc.field("uri"), m))?;
    let protocol_hint = protocol_hint(&route.protocol, &loc.field("protocol"))?;
    let predicates = build_predicates(route)?;
    let slotted = classify_filters(route, &input.file.defaults)?;
    // 治理状态按路由 id 获取或创建，跨热重载存活（LB 计数器、熔断器等）。
    let state = input.states.route_state(&route.id);
    let info = RouteCompileInfo {
        id: Arc::from(route.id.as_str()),
        uri: uri.clone(),
        protocol_hint,
        state: Arc::clone(&state),
        transcoding: transcoding_info(route, input.config_dir)?,
    };
    let handler = input.handler_factory.build(&info);
    let target = build_target(route, input, discovery, &uri, &state, &loc)?;
    let budget = total_budget(&input.file.defaults, route, server)?;
    let fctx = FilterBuildCtx {
        route_id: Arc::clone(&info.id),
        route_state: Arc::clone(&state),
        discovery: Arc::clone(discovery),
        max_retry_buffer: server.max_retry_buffer,
        config_dir: Arc::new(input.config_dir.to_path_buf()),
    };
    let chain = assemble_chain(
        &ChainCtx {
            registry: input.registry,
            slotted: &slotted,
            build: &fctx,
        },
        target,
        budget,
        handler,
    )?;
    Ok(Route {
        id: info.id,
        order: route.order,
        uri,
        protocol_hint,
        predicates,
        state,
        // BoxedService 是 Send 但非 Sync，放进 Mutex 以保持 Route 可跨线程共享。
        chain: Mutex::new(chain),
    })
}

/// 把路由的协议名映射为 [`ProtocolHint`]；未知名称返回带位置的错误。
pub(super) fn protocol_hint(raw: &str, loc: &Location) -> Result<ProtocolHint, ConfigError> {
    let hint = match raw {
        "auto" => ProtocolHint::Auto,
        "http1" => ProtocolHint::Fixed(Protocol::Http1),
        "http2" => ProtocolHint::Fixed(Protocol::Http2),
        "grpc" => ProtocolHint::Fixed(Protocol::Grpc),
        "grpc-web" => ProtocolHint::Fixed(Protocol::GrpcWeb),
        "grpc-web-text" => ProtocolHint::Fixed(Protocol::GrpcWebText),
        "rest-json" => ProtocolHint::Fixed(Protocol::RestJson),
        "websocket" => ProtocolHint::Fixed(Protocol::WebSocket),
        other => {
            return Err(ConfigError::value(
                loc.clone(),
                format!("unknown protocol {other:?}"),
            ))
        }
    };
    Ok(hint)
}

/// 构建传给 handler 的 transcoding 配置：描述符路径相对配置目录解析为
/// 绝对路径；逃出配置目录的引用在编译期即被拒绝。
fn transcoding_info(
    route: &RouteConfig,
    config_dir: &Path,
) -> Result<Option<Arc<TranscodingConfig>>, ConfigError> {
    route
        .transcoding
        .as_ref()
        .map(|t| {
            let mut resolved = t.clone();
            resolved.descriptor = resolve_in_dir(config_dir, &t.descriptor)?
                .to_string_lossy()
                .into_owned();
            Ok(Arc::new(resolved))
        })
        .transpose()
}

/// 按声明顺序构建谓词列表（路由对它们取 AND：全部命中才匹配该路由）。
fn build_predicates(route: &RouteConfig) -> Result<Vec<Arc<dyn RoutePredicate>>, ConfigError> {
    let mut predicates: Vec<Arc<dyn RoutePredicate>> = Vec::with_capacity(route.predicates.len());
    for (i, cfg) in route.predicates.iter().enumerate() {
        let loc = Location::route_item(&route.id, "predicates", i);
        let spec = validate::at(&loc, PredicateSpec::from_config(cfg))?;
        predicates.push(build_predicate(&spec, &loc)?);
    }
    Ok(predicates)
}

/// 由归一化 spec 构建单个谓词。
///
/// Header/Host/Query 的正则参数都是**全值匹配**：模式在谓词构造器内被
/// 编译为 `^(?:P)$`（共享辅助函数 `rgateway_core::predicate::full_match_regex`），
/// 因此 `Host=api\.example\.com` 只精确匹配该 authority，绝不会匹配其子串。
///
/// # 错误
/// 参数个数不对、正则编译失败、header/method 名非法或谓词名未知时返回
/// 携带 [`Location`] 的 [`ConfigError`]。
fn build_predicate(
    spec: &PredicateSpec,
    loc: &Location,
) -> Result<Arc<dyn RoutePredicate>, ConfigError> {
    let args = &spec.args;
    if args.is_empty() {
        return Err(ConfigError::value(
            loc.clone(),
            format!("{} needs at least one argument", spec.name),
        ));
    }
    let predicate: Arc<dyn RoutePredicate> = match spec.name.as_str() {
        "Path" => Arc::new(
            PathPredicate::new(args.clone()).map_err(|m| ConfigError::value(loc.clone(), m))?,
        ),
        "Method" => Arc::new(MethodPredicate::new(parse_methods(args, loc)?)),
        "Header" => {
            require_two(spec, loc)?;
            Arc::new(
                HeaderPredicate::new(parse_header_name(&args[0], loc)?, &args[1])
                    .map_err(|e| invalid_regex(&args[1], loc, e))?,
            )
        }
        "Host" => {
            // 参数按「或」拼接：`Host=a,b`（或单参数 "a|b"）匹配全值 a 或 b
            // —— 锚定细节见 `full_match_regex`。
            let joined = args.join("|");
            Arc::new(HostPredicate::new(&joined).map_err(|e| invalid_regex(&joined, loc, e))?)
        }
        "Query" => {
            require_two(spec, loc)?;
            Arc::new(
                QueryPredicate::new(args[0].clone(), &args[1])
                    .map_err(|e| invalid_regex(&args[1], loc, e))?,
            )
        }
        other => {
            return Err(ConfigError::UnknownPredicate {
                location: loc.clone(),
                name: other.to_string(),
            })
        }
    };
    Ok(predicate)
}

/// Header/Query 谓词恰好接受 `(name, regex)` 两个参数。
fn require_two(spec: &PredicateSpec, loc: &Location) -> Result<(), ConfigError> {
    if spec.args.len() != 2 {
        return Err(ConfigError::value(
            loc.clone(),
            format!("{} expects exactly 2 arguments (name, regex)", spec.name),
        ));
    }
    Ok(())
}

/// 把 Method 谓词的参数逐个解析为 [`Method`]；非法名称定位到该谓词条目。
fn parse_methods(args: &[String], loc: &Location) -> Result<Vec<Method>, ConfigError> {
    args.iter()
        .map(|m| {
            m.parse::<Method>()
                .map_err(|e| ConfigError::value(loc.clone(), format!("invalid method {m:?}: {e}")))
        })
        .collect()
}

/// 把 Header 谓词的第一个参数解析为合法的 [`HeaderName`]。
fn parse_header_name(raw: &str, loc: &Location) -> Result<HeaderName, ConfigError> {
    HeaderName::from_bytes(raw.as_bytes())
        .map_err(|e| ConfigError::value(loc.clone(), format!("invalid header name {raw:?}: {e}")))
}

/// 把谓词模式编译失败定位到其配置条目。模式由谓词构造器经
/// `rgateway_core::predicate::full_match_regex` 编译（锚定为 `^(?:P)$`）。
fn invalid_regex(raw: &str, loc: &Location, cause: regex::Error) -> ConfigError {
    ConfigError::value(loc.clone(), format!("invalid regex {raw:?}: {cause}"))
}

/// 归一化「生效过滤器列表」——路由声明的 filters 之后追加
/// `defaults.filters`——并把每个条目归入其规范槽位。未知过滤器名在该
/// 条目自身的位置报错：`routes[id].filters[i]` / `defaults.filters[i]`。
fn classify_filters(
    route: &RouteConfig,
    defaults: &DefaultsConfig,
) -> Result<Slotted, ConfigError> {
    let mut slotted = Slotted {
        by_slot: BTreeMap::new(),
    };
    push_filters(&mut slotted, &route.filters, &|i| {
        Location::route_item(&route.id, "filters", i)
    })?;
    push_filters(&mut slotted, &defaults.filters, &|i| {
        Location::indexed("defaults.filters", i)
    })?;
    Ok(slotted)
}

/// 把一组过滤器配置经 [`FilterSpec`] 归一化 + [`slot_for`] 分类后推入
/// 对应槽位；`at` 生成条目的配置位置。
fn push_filters(
    slotted: &mut Slotted,
    filters: &[FilterConfig],
    at: &dyn Fn(usize) -> Location,
) -> Result<(), ConfigError> {
    for (i, cfg) in filters.iter().enumerate() {
        let loc = at(i);
        let spec = validate::at(&loc, FilterSpec::from_config(cfg))?;
        let slot = slot_for(&spec.name).ok_or_else(|| ConfigError::UnknownFilter {
            location: loc.clone(),
            name: spec.name.clone(),
        })?;
        slotted.push(slot, IndexedFilter { loc, spec });
    }
    Ok(())
}

/// 解析生效的 LB 算法（路由 → defaults → 内置默认）并构建单次尝试的
/// 目标选择器。算法实例经 [`preserved_balancer`] 复用，使轮询计数器跨
/// 热重载存活。
fn build_target(
    route: &RouteConfig,
    input: &CompileInput<'_>,
    discovery: &Arc<dyn ServiceDiscovery>,
    uri: &RouteUri,
    state: &Arc<RouteState>,
    loc: &Location,
) -> Result<TargetSelector, ConfigError> {
    let name = route
        .load_balancer
        .as_deref()
        .or(input.file.defaults.load_balancer.as_deref())
        .unwrap_or(DEFAULT_BALANCER);
    let balancer = preserved_balancer(state, name)
        .map_err(|m| ConfigError::value(loc.field("load_balancer"), m))?;
    Ok(TargetSelector {
        uri: uri.clone(),
        discovery: Arc::clone(discovery),
        balancer,
        state: Arc::clone(state),
    })
}

/// 总 Deadline 预算，优先级：路由 `request_timeout` → defaults
/// `request_timeout` → server 的 `timeouts.request_default`。
fn total_budget(
    defaults: &DefaultsConfig,
    route: &RouteConfig,
    server: &ServerSettings,
) -> Result<Duration, ConfigError> {
    if let Some(raw) = &route.request_timeout {
        return parse_duration_at(&Location::route(&route.id).field("request_timeout"), raw);
    }
    if let Some(raw) = &defaults.request_timeout {
        return parse_duration_at(&Location::section("defaults").field("request_timeout"), raw);
    }
    Ok(server.request_default)
}

/// 由内向外堆叠整条链：handler → Timeout → Breaker → Target → Retry →
/// Deadline → Route → Auth → RateLimit → Cors → Compression，最终外 → 内
/// 次序为 Compression → CORS → RateLimit → Auth → Route → Deadline →
/// Retry → Target → Breaker → Timeout → Handler。
///
/// 次序原因：Target 在 Breaker 之前，每次重试重新 LB 选实例并跳过熔断
/// 打开的实例；Deadline 在 Retry 之外，总预算约束整个请求（含重试）。
fn assemble_chain(
    ctx: &ChainCtx<'_>,
    target: TargetSelector,
    budget: Duration,
    handler: BoxedService,
) -> Result<BoxedService, ConfigError> {
    let mut chain = handler;
    // 以下每一步都把该槽位/层包在既有链的外面（wrap 即「向外长一层」）。
    // Timeout：单次尝试超时。
    chain = ctx.wrap(FilterSlot::Timeout, chain)?;
    // Breaker：实例级熔断器。
    chain = ctx.wrap(FilterSlot::Breaker, chain)?;
    // 目标选择在熔断内侧、随每次尝试执行：选出实例写入 ctx.target。
    chain = target.wrap(chain);
    // Retry 在目标选择外侧：每次重试都会重新选实例（跳过熔断打开的）。
    chain = ctx.wrap(FilterSlot::Retry, chain)?;
    // 总 Deadline 在 Retry 外侧：预算约束整个请求（含全部重试），只限响应头。
    chain = DeadlineLayer::new(budget).wrap(chain);
    // Route：路由类过滤器（RewritePath 等）。
    chain = ctx.wrap(FilterSlot::Route, chain)?;
    // Auth：认证/鉴权/IP 过滤。
    chain = ctx.wrap(FilterSlot::Auth, chain)?;
    // RateLimit：限流。
    chain = ctx.wrap(FilterSlot::RateLimit, chain)?;
    // CORS：跨域。
    chain = ctx.wrap(FilterSlot::Cors, chain)?;
    // Compression：响应压缩（最外层）。
    chain = ctx.wrap(FilterSlot::Compression, chain)?;
    Ok(chain)
}

/// 负载均衡器的 preserved holder。类型标记 `M` 编码算法种类：热重载把
/// 算法换成不同名字时，[`RouteState::preserved`] 里的 downcast 会失配，
/// 于是槽位被替换为新 holder；算法名不变的重载则复用同一 balancer 实例
/// （轮询计数器跨热重载存活）。
pub(super) struct BalancerSlot<M: 'static> {
    pub(super) name: String,
    pub(super) balancer: Arc<dyn LoadBalancer>,
    /// 仅作类型标记（零大小），让不同算法的 holder 类型互不相同。
    _algorithm: PhantomData<fn() -> M>,
}

impl<M: 'static> std::fmt::Debug for BalancerSlot<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BalancerSlot")
            .field("name", &self.name)
            .field("balancer", &self.balancer.name())
            .finish()
    }
}

/// 取（或创建）`name` 对应的 preserved balancer
/// （`"round_robin" | "random" | "smooth_wrr"`）；同一 [`RouteState`] 让
/// 计数器跨多次编译延续。未知算法名返回错误串。
pub(super) fn preserved_balancer(
    state: &RouteState,
    name: &str,
) -> Result<Arc<dyn LoadBalancer>, String> {
    match name {
        "round_robin" => Ok(slot_balancer::<RoundRobin>(state, name, || {
            Arc::new(RoundRobin::default())
        })),
        "random" => Ok(slot_balancer::<RandomBalancer>(state, name, || {
            Arc::new(RandomBalancer::default())
        })),
        "smooth_wrr" => Ok(slot_balancer::<SmoothWeightedRoundRobin>(
            state,
            name,
            || Arc::new(SmoothWeightedRoundRobin::default()),
        )),
        other => Err(format!(
            "unknown load balancer {other:?} (expected round_robin|random|smooth_wrr)"
        )),
    }
}

/// 算法标记 `M` 仍匹配时复用 preserved holder（拿到原 balancer 实例），
/// 不匹配则由 [`RouteState::preserved`] 换新。
fn slot_balancer<M: 'static>(
    state: &RouteState,
    name: &str,
    make: fn() -> Arc<dyn LoadBalancer>,
) -> Arc<dyn LoadBalancer> {
    let holder: Arc<BalancerSlot<M>> = state.preserved(BALANCER_SLOT, || {
        Arc::new(BalancerSlot {
            name: name.to_string(),
            balancer: make(),
            _algorithm: PhantomData,
        })
    });
    Arc::clone(&holder.balancer)
}

#[cfg(test)]
#[path = "compile_chain_tests.rs"]
mod tests;

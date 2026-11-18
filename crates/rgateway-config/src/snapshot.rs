//! 热重载状态：原子快照存储（snapshot store）+ 治理状态注册表。
//!
//! # 设计要点（为什么这样设计）
//!
//! * **快照整体替换、原子换入。** 每次重载编译出一份全新的不可变
//!   [`CompiledConfig`]，经 `arc-swap` 原子换入 [`SnapshotStore`]；
//!   在途请求继续持有旧的 `Arc` 服务到结束——换快照从不打断进行中的
//!   请求，也不需要锁：读者（[`SnapshotStore::load`]）只是原子指针读。
//! * **失败保旧。** 编译失败时旧快照继续生效，失败计数
//!   （`failed_reloads`）加一，错误原样返回给调用方去记录/上报；不会
//!   出现「半新半旧」的中间状态，因为快照只在编译**完全成功**后才换。
//! * **治理状态跨重载保留。** 熔断器等运行时状态放在
//!   [`RuntimeStateRegistry`]，与配置快照分离：重载只是换配置，不重置
//!   统计。重载成功后再裁剪（[`prune_stale_state`]）新配置不再引用的
//!   孤儿状态，避免内存泄漏。

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::compile::{compile, CompileInput, CompiledConfig};
use crate::error::ConfigError;
use crate::spec::{FilterRegistry, RouteHandlerFactory};
use rgateway_core::StateRegistry;

/// 跨重载保留的治理状态注册表（熔断器、按路由/实例的运行时统计）。
///
/// 这里把 `rgateway_core::StateRegistry` 提升为别名，让配置层调用方
/// 不必直接依赖 core 的命名。
pub use rgateway_core::StateRegistry as RuntimeStateRegistry;

/// 持有生效中的 [`CompiledConfig`]；读者调用 [`Self::load`]，那是原子
/// 指针读（无锁）。
///
/// 三个字段各司其职：
/// * `inner` —— `ArcSwap` 守护的当前快照，换入换出都是原子的；
/// * `version` —— 单调递增的快照版本号（从 1 起），换快照即加一，
///   供日志/管理面观测「配置生效到了第几版」；
/// * `failed_reloads` —— 失败的重载次数（每次失败保留旧快照）。
pub struct SnapshotStore {
    inner: ArcSwap<CompiledConfig>,
    version: AtomicU64,
    failed_reloads: AtomicU64,
}

impl SnapshotStore {
    /// 以初始快照建 store；版本号从 1 开始。
    pub fn new(initial: CompiledConfig) -> Self {
        Self {
            inner: ArcSwap::from_pointee(initial),
            version: AtomicU64::new(1),
            failed_reloads: AtomicU64::new(0),
        }
    }

    /// 借出当前快照（廉价的原子读，拿到的是 `Arc` 克隆）。
    pub fn load(&self) -> Arc<CompiledConfig> {
        self.inner.load_full()
    }

    /// 换入新快照并返回旧的。版本号在此加一。
    ///
    /// 只应在编译完全成功后调用（见 [`reload`]）；换入是原子的，读者
    /// 要么看到旧快照、要么看到新快照，绝无中间态。
    pub fn swap(&self, next: CompiledConfig) -> Arc<CompiledConfig> {
        self.version.fetch_add(1, Ordering::Relaxed);
        self.inner.swap(Arc::new(next))
    }

    /// 单调递增的快照版本号（从 1 起）。
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// 失败的重载次数（这些重载都保留了旧快照）。
    pub fn failed_reloads(&self) -> u64 {
        self.failed_reloads.load(Ordering::Relaxed)
    }

    /// 记一次重载失败（快照未变，仅计数）。
    pub fn record_reload_failure(&self) {
        self.failed_reloads.fetch_add(1, Ordering::Relaxed);
    }
}

/// 从 YAML 重建快照所需的依赖（文件监听器与管理面的手动 reload 共用）。
///
/// * `registry` —— 过滤器注册表（编译过滤器链时查工厂）。
/// * `states` —— 治理状态注册表，跨重载保留、成功后被裁剪。
/// * `handler_factory` —— 构造各路由 handler 的工厂。
#[derive(Clone)]
pub struct ReloadContext {
    pub registry: FilterRegistry,
    pub states: Arc<StateRegistry>,
    pub handler_factory: Arc<dyn RouteHandlerFactory>,
}

/// 从 YAML 文本重建快照，成功才换入 [`SnapshotStore`]；失败保持当前
/// 快照生效并返回错误（失败计数由调用方记录）。
///
/// 重载成功还会裁剪新配置不再引用的保留治理状态（[`prune_stale_state`]）；
/// 编译失败则旧快照、旧状态都原样保留。
///
/// # 错误
///
/// YAML 解析失败、校验失败（返回第一个错误）、编译失败时返回
/// [`ConfigError`]，此时 store 未被改动。
pub fn reload(
    store: &SnapshotStore,
    ctx: &ReloadContext,
    yaml: &str,
    config_dir: &Path,
) -> Result<Arc<CompiledConfig>, ConfigError> {
    let file = crate::loader::load_from_str(yaml)?;
    // 校验失败快速失败：只报第一个错误，store 不动。
    let errors = crate::validate::validate(&file);
    if let Some(first) = errors.into_iter().next() {
        return Err(first);
    }
    // 先完整编译、后裁剪、最后才换快照——顺序保证失败路径什么都不动。
    let compiled = compile(CompileInput {
        file: &file,
        registry: &ctx.registry,
        states: &ctx.states,
        handler_factory: ctx.handler_factory.as_ref(),
        config_dir,
    })?;
    prune_stale_state(&ctx.states, &compiled);
    store.swap(compiled);
    Ok(store.load())
}

/// 重载后的清理（[`StateRegistry::retain_routes`]）：丢掉新配置已删除/
/// 改名的路由的保留状态，以及实例已离开服务发现的按实例熔断器。
///
/// 在途请求持有的是自己的 `Arc`，裁剪注册表绝不会把它们脚下的状态
/// 抽走，只会阻止后来者再取到孤儿状态。
fn prune_stale_state(states: &StateRegistry, compiled: &CompiledConfig) {
    let live: Vec<Arc<str>> = compiled
        .routes
        .routes()
        .iter()
        .map(|route| route.id.clone())
        .collect();
    // 路由级：只保留新配置里还存在的路由 id
    states.retain_routes(&live);
    for route in compiled.routes.routes() {
        // 实例级：lb:// 路由按新发现列表裁剪熔断器
        if let rgateway_core::RouteUri::Lb(name) = &route.uri {
            route.state.retain_instances(&compiled.discovery.get(name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::RouteCompileInfo;
    use rgateway_core::{empty_body, BoxedService, BreakerConfig, GatewayError, ProxyBody};

    /// 路由 handler stub：watcher/snapshot 测试从不真正调用它。
    struct StubHandler;

    impl RouteHandlerFactory for StubHandler {
        fn build(&self, _route: &RouteCompileInfo) -> BoxedService {
            BoxedService::new(tower::service_fn(
                |_req: http::Request<ProxyBody>| async move {
                    Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
                },
            ))
        }
    }

    /// 带独立 StateRegistry 的 ReloadContext，返回值第二项是状态注册表
    /// 本体，供测试断言裁剪结果。
    fn stub_ctx() -> (Arc<ReloadContext>, Arc<StateRegistry>) {
        let states = Arc::new(StateRegistry::new());
        let ctx = ReloadContext {
            registry: FilterRegistry::new(),
            states: Arc::clone(&states),
            handler_factory: Arc::new(StubHandler),
        };
        (Arc::new(ctx), states)
    }

    /// 用空配置 `{}`（必须能编译）初始化的 SnapshotStore。
    fn stub_store(ctx: &ReloadContext) -> SnapshotStore {
        let file = crate::loader::load_from_str("{}").expect("empty yaml must parse");
        let compiled = compile(CompileInput {
            file: &file,
            registry: &ctx.registry,
            states: &ctx.states,
            handler_factory: ctx.handler_factory.as_ref(),
            config_dir: Path::new("."),
        })
        .expect("empty config must compile");
        SnapshotStore::new(compiled)
    }

    /// 执行一次必须成功的 reload（失败即 panic）。
    fn reload_yaml(store: &SnapshotStore, ctx: &ReloadContext, yaml: &str) {
        reload(store, ctx, yaml, Path::new(".")).expect("reload must succeed");
    }

    /// 注册表里当前的路由 id 排序列表，便于断言。
    fn sorted_ids(states: &StateRegistry) -> Vec<String> {
        let mut ids: Vec<String> = states.route_ids().iter().map(|id| id.to_string()).collect();
        ids.sort();
        ids
    }

    // 以下 YAML 常量是重载测试的输入：ONE_ROUTE 只有 user-http；
    // RENAMED_ROUTE 把它改名为 user-http-v2；LB_TWO_INSTANCES /
    // LB_ONE_INSTANCE 是同一 lb:// 路由在服务发现里两个实例与一个实例
    // 的两种形态（用来观察按实例熔断器的裁剪）。
    const ONE_ROUTE: &str = r#"
routes:
  - id: user-http
    uri: "http://127.0.0.1:9000"
    predicates: ["Path=/api/**"]
"#;
    const RENAMED_ROUTE: &str = r#"
routes:
  - id: user-http-v2
    uri: "http://127.0.0.1:9000"
    predicates: ["Path=/api/**"]
"#;
    const LB_TWO_INSTANCES: &str = r#"
discovery:
  providers:
    - type: static
      services:
        user-svc:
          - { address: "127.0.0.1:50051" }
          - { address: "127.0.0.1:50052" }
routes:
  - id: user-http
    uri: "lb://user-svc"
    predicates: ["Path=/api/**"]
"#;
    const LB_ONE_INSTANCE: &str = r#"
discovery:
  providers:
    - type: static
      services:
        user-svc:
          - { address: "127.0.0.1:50052" }
routes:
  - id: user-http
    uri: "lb://user-svc"
    predicates: ["Path=/api/**"]
"#;

    /// 验证：重载后，新配置删除/改名的路由的治理状态被裁剪，幸存路由
    /// 的状态保留。
    #[test]
    fn reload_prunes_state_for_routes_dropped_by_the_new_config() {
        let (ctx, states) = stub_ctx();
        let store = stub_store(&ctx);
        // 先造出旧配置积累的状态，其中 user-http 会活到下一轮
        states.route_state("old-route");
        states.route_state("user-http");

        reload_yaml(&store, &ctx, ONE_ROUTE);
        assert_eq!(sorted_ids(&states), vec!["user-http"]);

        reload_yaml(&store, &ctx, RENAMED_ROUTE);
        assert_eq!(sorted_ids(&states), vec!["user-http-v2"]);
    }

    /// 验证：重载后，实例已离开服务发现的按实例熔断器被裁剪，仍在的
    /// 实例的熔断器保留。
    #[test]
    fn reload_prunes_breakers_for_instances_that_left_discovery() {
        let (ctx, states) = stub_ctx();
        let store = stub_store(&ctx);
        reload_yaml(&store, &ctx, LB_TWO_INSTANCES);

        // 两个实例各建一个熔断器
        let state = states.route_state("user-http");
        state.breaker_for("127.0.0.1:50051", BreakerConfig::default());
        state.breaker_for("127.0.0.1:50052", BreakerConfig::default());
        assert_eq!(state.breaker_states().len(), 2);

        // 发现列表缩到一个实例后，离开的那个熔断器应被裁掉
        reload_yaml(&store, &ctx, LB_ONE_INSTANCE);
        let mut live: Vec<String> = state
            .breaker_states()
            .iter()
            .map(|(id, _)| id.to_string())
            .collect();
        live.sort();
        assert_eq!(live, vec!["127.0.0.1:50052"]);
    }
}

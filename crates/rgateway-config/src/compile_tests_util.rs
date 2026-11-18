//! 编译测试共用的测试替身与辅助函数：执行日志、可录音的过滤器/handler
//! 工厂、总是构建失败的工厂，以及 `compile` 的成功/失败快捷入口。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rgateway_core::{empty_body, BoxedService, Protocol, ProxyBody, RequestCtx, StateRegistry};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

use crate::error::{ConfigError, Location};
use crate::schema::ConfigFile;
use crate::spec::{
    FilterBuildCtx, FilterFactory, FilterRegistry, FilterSpec, RouteCompileInfo,
    RouteHandlerFactory, ServiceWrapper,
};

use super::super::{compile, CompileInput, CompiledConfig};

/// mock 层共享的执行日志：按调用顺序记录「过滤器名/handler」，测试据此
/// 断言链的外→内执行次序。
pub(super) type Log = Arc<Mutex<Vec<String>>>;

/// 新建一个空的共享执行日志。
pub(super) fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

/// 取出日志当前的全部记录（不克隆清空）。
pub(super) fn drain(log: &Log) -> Vec<String> {
    log.lock().expect("lock").clone()
}

/// 可录音的过滤器工厂：服务先把名字追加进共享日志，再委托给内层链。
/// spec 里的 `tag` 参数会以 `name:tag` 形式记录，让测试能区分同一槽位里
/// 叠放的同名实例（路由过滤器 vs `defaults.filters` 条目）。
struct RecordingFilter {
    name: &'static str,
    log: Log,
}

impl FilterFactory for RecordingFilter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn build(
        &self,
        spec: &FilterSpec,
        _ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        let label = match spec.param_str_opt("tag") {
            Ok(Some(tag)) => format!("{}:{tag}", self.name),
            _ => self.name.to_string(),
        };
        let log = Arc::clone(&self.log);
        Ok(Box::new(move |inner: BoxedService| {
            let log = Arc::clone(&log);
            let label = label.clone();
            BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
                let log = Arc::clone(&log);
                let label = label.clone();
                let inner = inner.clone();
                async move {
                    log.lock().expect("lock").push(label);
                    inner.oneshot(req).await
                }
            }))
        }))
    }
}

/// 构建总是失败的过滤器工厂（用于「构建错误位置」测试）。
struct FailingFilter {
    name: &'static str,
}

impl FilterFactory for FailingFilter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn build(
        &self,
        _spec: &FilterSpec,
        _ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        Err(ConfigError::value(
            Location::section("factory"),
            "factory refused to build",
        ))
    }
}

/// 录音的 handler 工厂：产出返回 200 的服务，执行时把 "handler" 追加进
/// 日志，同时记录路由编译信息与本次选中的上游目标（用于验证 LB 计数器
/// 跨重载延续）。
pub(super) struct RecordingHandler {
    log: Log,
    seen: Arc<Mutex<Vec<RouteCompileInfo>>>,
    targets: Arc<Mutex<Vec<String>>>,
}

impl RecordingHandler {
    /// 新建工厂，复用给定的共享日志。
    pub(super) fn new(log: Log) -> Self {
        Self {
            log,
            seen: Arc::new(Mutex::new(Vec::new())),
            targets: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 各次请求选中的上游实例 id（按请求顺序）。
    pub(super) fn targets(&self) -> Vec<String> {
        self.targets.lock().expect("lock").clone()
    }

    /// 各路由传给 handler 工厂的 [`RouteCompileInfo`]（按构建顺序）。
    pub(super) fn infos(&self) -> Vec<RouteCompileInfo> {
        self.seen.lock().expect("lock").clone()
    }
}

impl RouteHandlerFactory for RecordingHandler {
    fn build(&self, route: &RouteCompileInfo) -> BoxedService {
        self.seen.lock().expect("lock").push(route.clone());
        let log = Arc::clone(&self.log);
        let targets = Arc::clone(&self.targets);
        BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
            let log = Arc::clone(&log);
            let targets = Arc::clone(&targets);
            async move {
                log.lock().expect("lock").push("handler".to_string());
                if let Some(target) = crate::target::request_ctx(&req).and_then(|c| c.target()) {
                    targets
                        .lock()
                        .expect("lock")
                        .push(target.instance_id.to_string());
                }
                Ok::<http::Response<ProxyBody>, rgateway_core::GatewayError>(http::Response::new(
                    empty_body(),
                ))
            }
        }))
    }
}

/// 延迟 `latency` 后才应答 200 的 handler 工厂（Deadline 预算测试用）。
pub(super) struct SlowHandler {
    pub(super) latency: Duration,
}

impl RouteHandlerFactory for SlowHandler {
    fn build(&self, _route: &RouteCompileInfo) -> BoxedService {
        let latency = self.latency;
        BoxCloneService::new(service_fn(
            move |_req: http::Request<ProxyBody>| async move {
                tokio::time::sleep(latency).await;
                Ok::<http::Response<ProxyBody>, rgateway_core::GatewayError>(http::Response::new(
                    empty_body(),
                ))
            },
        ))
    }
}

/// 注册表：为每个名字注册一个可录音的过滤器工厂。
pub(super) fn registry_with(log: &Log, names: &[&'static str]) -> FilterRegistry {
    let mut registry = FilterRegistry::new();
    for name in names {
        registry.register(Arc::new(RecordingFilter {
            name,
            log: Arc::clone(log),
        }));
    }
    registry
}

/// 注册表：每个名字的工厂构建总是失败。
pub(super) fn registry_failing(names: &[&'static str]) -> FilterRegistry {
    let mut registry = FilterRegistry::new();
    for name in names {
        registry.register(Arc::new(FailingFilter { name }));
    }
    registry
}

/// 解析测试内嵌的 YAML 为 [`ConfigFile`]（走正式 loader，仅 schema 层）。
pub(super) fn parse_yaml(yaml: &str) -> ConfigFile {
    crate::loader::load_from_str(yaml).expect("yaml must parse")
}

/// 在系统临时目录下新建一个带唯一后缀的绝对目录（测试隔离用）。
pub(super) fn test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rgateway-compile-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// 以 [`CompileInput`] 调用 [`compile`] 并断言成功。
pub(super) fn compile_ok(
    file: &ConfigFile,
    registry: &FilterRegistry,
    states: &Arc<StateRegistry>,
    handler: &dyn RouteHandlerFactory,
    dir: &Path,
) -> CompiledConfig {
    compile(CompileInput {
        file,
        registry,
        states,
        handler_factory: handler,
        config_dir: dir,
    })
    .expect("compile must succeed")
}

/// 以 [`CompileInput`] 调用 [`compile`] 并断言失败，返回错误本体。
pub(super) fn compile_err(
    file: &ConfigFile,
    registry: &FilterRegistry,
    states: &Arc<StateRegistry>,
    handler: &dyn RouteHandlerFactory,
    dir: &Path,
) -> ConfigError {
    match compile(CompileInput {
        file,
        registry,
        states,
        handler_factory: handler,
        config_dir: dir,
    }) {
        Err(err) => err,
        Ok(_) => panic!("compile must fail"),
    }
}

/// 携带全新 [`RequestCtx`] 向链发送一个请求（断言调用成功）。
pub(super) async fn send(chain: &BoxedService) {
    let ctx = Arc::new(RequestCtx::new(Protocol::Http1, None));
    let mut req = http::Request::new(empty_body());
    req.extensions_mut().insert(ctx);
    chain.clone().oneshot(req).await.expect("chain call");
}

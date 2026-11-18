//! 启动装配（bootstrap）：配置文件 → 可运行的网关（P3 交付的二进制装配层）。
//!
//! 启动序列（顺序即取舍）：
//!
//! 1. **加载 + 校验（取第一个错误）**——坏配置在触碰任何运行时资源前快速
//!    失败，错误自带 [`rgateway_config::Location`] 坐标；
//! 2. **TLS 拒绝闸**——TLS 监听属后续阶段，`tls.enabled = true` 必须启动
//!    失败，绝不静默降级为明文监听（降级会让「以为加密」的流量裸奔上线）；
//! 3. **tracing 初始化**——过滤表达式三级优先级
//!    （[`resolve_tracing_filter`]：config → `RUST_LOG` → `info`），
//!    `try_init` 幂等（重复装配/测试并发只跳过、不报错）；
//! 4. **编译快照 + 热重载**——`SnapshotStore` 原子换入、失败保旧；
//!    过滤器用 [`runtime_registry`]（[`noop_filters`] 占位 + `rgateway-filters`
//!    真实工厂覆盖），handler 用 `rgateway_proxy::HttpHandlerFactory`
//!    （`connect_timeout` 取 `server.timeouts.connect`）；
//! 5. **指标 recorder**——`observability.metrics.enabled` 时取进程级共享句柄
//!    （[`shared_metrics_handle`]，全局 recorder 只装一次）并起周期
//!    `run_upkeep` 维护任务；`/metrics` 渲染闭包注入管理面状态；
//! 6. **监听 + accept 循环**——数据面 `server.listen` 与管理面 `mgmt.listen`
//!    各自绑定（绑定失败快速启动失败，绝不悄悄少一个面）；网络调度（auto
//!    Builder 同时服务 HTTP/1.1 与 HTTP/2、连接经 GracefulShutdown 管理、
//!    管理面 REST 与 gRPC 按 content-type 同端口复用）收在 [`net`]；
//! 7. **优雅关闭**（[`RunningGateway::shutdown`]）——停止 accept → 等在途
//!    连接结束（至多 `server.timeouts.graceful_shutdown` 窗口）→ 超时 abort
//!    剩余连接任务，同时 abort 指标维护任务。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use rgateway_config::dsl;
use rgateway_config::{
    compile, load_from_file, spawn_watcher, validate, CompileInput, ConfigFile, ConfigWatcher,
    FilterRegistry, ReloadContext, RouteHandlerFactory, SnapshotStore,
};
use rgateway_core::StateRegistry;
use rgateway_mgmt::MgmtState;
use rgateway_obs::MetricsHandle;
use rgateway_proxy::HttpHandlerFactory;

use crate::{noop_filters, Dispatcher};

mod net;
#[path = "bootstrap/tls.rs"]
mod tls;

use net::{accept_loop, mgmt_loop};

/// 指标直方图的周期维护间隔（`MetricsHandle` 上游维护契约的建议值）。
const UPKEEP_INTERVAL: Duration = Duration::from_secs(10);

/// 编译期守护：`RunningGateway` 必须能整体移进 `tokio::spawn` 的关闭任务。
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<RunningGateway>();
};

/// 在跑的网关实例。
///
/// [`Self::shutdown`] 消耗自身做优雅关闭；直接 drop 也等价于发起关闭——
/// 触发信号（watch 通道关闭）随字段 drop 发出，两个 accept 循环照常走到收尾。
pub struct RunningGateway {
    /// 数据面实际监听地址（配置端口为 0 时由此取实际端口）。
    pub local_addr: SocketAddr,
    /// 管理面实际监听地址（REST + gRPC 同端口复用）。
    pub mgmt_addr: SocketAddr,
    /// 保活配置文件监听器（drop 即停止热重载）。
    _watcher: Option<ConfigWatcher>,
    /// 指标周期维护任务；关闭时 abort。
    _upkeep: Option<JoinHandle<()>>,
    /// 关闭触发信号（watch）；drop 即触发（见类型文档）。
    trigger: Option<watch::Sender<bool>>,
    /// 收尾完成通知（数据面 + 管理面 accept 循环都结束后发来）。
    done: Option<oneshot::Receiver<()>>,
}

impl RunningGateway {
    /// 优雅关闭（消耗自身）：停止 accept → 等在途连接在
    /// `server.timeouts.graceful_shutdown` 窗口内结束 → 超时 abort 剩余
    /// 连接任务，并 abort 指标维护任务。收尾完成后返回。
    pub async fn shutdown(mut self) {
        if let Some(trigger) = self.trigger.take() {
            let _ = trigger.send(true);
        }
        if let Some(done) = self.done.take() {
            // accept 循环任务若提前结束（panic 等），通道随之关闭，视为收尾完成。
            let _ = done.await;
        }
        if let Some(upkeep) = self._upkeep.take() {
            upkeep.abort();
        }
    }
}

impl std::fmt::Debug for RunningGateway {
    /// 只暴露监听地址：内部句柄（watcher / 通道）没有可读的调试形态。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningGateway")
            .field("local_addr", &self.local_addr)
            .field("mgmt_addr", &self.mgmt_addr)
            .finish()
    }
}

/// 启动网关：按模块文档的序列装配，在 `server.listen`（数据面）与
/// `mgmt.listen`（管理面）上开始服务。
///
/// # 错误
///
/// 配置加载/校验/编译失败（[`rgateway_config::ConfigError`]，带
/// [`rgateway_config::Location`]）、TLS 材料非法（`tls.enabled = true` 而
/// 证书/私钥/CA 缺失或坏——**绝不降级明文**）、任一监听端口绑定失败
/// （数据面/管理面都必须真的在听，缺面启动失败）。
pub async fn start(config_path: &Path) -> anyhow::Result<RunningGateway> {
    let file = load_validated(config_path)?;
    init_tracing(&resolve_tracing_filter(
        file.observability.tracing_filter.as_deref(),
        std::env::var("RUST_LOG").ok().as_deref(),
    ));
    let (store, ctx) = compile_snapshot(config_path, &file)?;
    let server = store.load().server.clone();
    // TLS 装配：enabled=true 而材料缺失/非法 → 启动失败（绝不静默降级
    // 明文）。acceptor 固定于启动期，监听形态运行期不变。
    let tls = tls::build_acceptor(&server.tls)?;
    let client_scheme = if tls.is_some() {
        rgateway_core::ClientScheme::Https
    } else {
        rgateway_core::ClientScheme::Http
    };
    let watcher = spawn_watcher(config_path, Arc::clone(&store), Arc::clone(&ctx))?;
    let dispatcher = Arc::new(Dispatcher::new(Arc::clone(&store), client_scheme));
    let metrics = shared_metrics_handle(store.load().metrics_enabled);
    let upkeep = spawn_upkeep(metrics.clone());
    let mgmt_state = build_mgmt_state(Arc::clone(&store), Arc::clone(&ctx.states), metrics);
    let mgmt_listen = store.load().mgmt_listen;
    let listener = TcpListener::bind(server.listen).await?;
    let mgmt_listener = TcpListener::bind(mgmt_listen).await?;
    let local_addr = listener.local_addr()?;
    let mgmt_addr = mgmt_listener.local_addr()?;
    let (trigger, shutdown) = watch::channel(false);
    let (done_tx, done) = oneshot::channel();
    let graceful_window = server.graceful_shutdown;
    tokio::spawn(async move {
        let data = tokio::spawn(accept_loop(listener, dispatcher, shutdown.clone(), server, tls));
        let mgmt = tokio::spawn(mgmt_loop(
            mgmt_listener,
            mgmt_state,
            shutdown,
            graceful_window,
        ));
        let _ = data.await;
        let _ = mgmt.await;
        let _ = done_tx.send(());
    });
    tracing::info!(%local_addr, %mgmt_addr, "gateway listening");
    Ok(RunningGateway {
        local_addr,
        mgmt_addr,
        _watcher: Some(watcher),
        _upkeep: upkeep,
        trigger: Some(trigger),
        done: Some(done),
    })
}

/// 进程级指标句柄缓存：`metrics` 全局 recorder 只能安装一次，而一个进程
/// 可能多次装配网关（测试并发、进程内重启）。首次安装缓存句柄
/// （[`MetricsHandle`] 可克隆），之后各次装配共享同一 recorder。
///
/// `enabled=false` 返回 `None`（`/metrics` 端点随之 404）；安装失败只降级
/// 指标面（warn 后返回 `None`），不拖垮网关启动——指标是观测面不是数据面。
///
/// 与快照开关的分工（文档化取舍）：**启动期** `metrics.enabled` 决定是否
/// 安装 recorder（全局一次性安装，中途开启做不到）；**热重载**后端点可见性
/// 由快照 `metrics.enabled` 跟随（见 rgateway-mgmt 的 `metrics_endpoint`）。
/// 启动期关闭、之后经热重载打开的配置只能得到 404——没有 recorder 可渲染。
fn shared_metrics_handle(enabled: bool) -> Option<MetricsHandle> {
    if !enabled {
        return None;
    }
    let mut cached = match METRICS_HANDLE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(handle) = cached.as_ref() {
        return Some(handle.clone());
    }
    match rgateway_obs::install() {
        Ok(handle) => {
            *cached = Some(handle.clone());
            Some(handle)
        }
        Err(err) => {
            tracing::warn!(error = %err, "metrics recorder unavailable, /metrics disabled");
            None
        }
    }
}

/// 进程级指标句柄缓存（见 [`shared_metrics_handle`]）。
static METRICS_HANDLE: Mutex<Option<MetricsHandle>> = Mutex::new(None);

/// 指标周期维护任务：折叠直方图累积桶（见 [`MetricsHandle`] 维护契约）；
/// 无指标句柄时不起任务。
fn spawn_upkeep(metrics: Option<MetricsHandle>) -> Option<JoinHandle<()>> {
    let handle = metrics?;
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(UPKEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            handle.run_upkeep();
        }
    }))
}

/// 构造管理面共享状态：快照存储 + 治理状态注册表 + `/metrics` 渲染闭包
/// （指标句柄存在才注入——未启用指标时端点 404，见 rgateway-mgmt 文档）。
fn build_mgmt_state(
    store: Arc<SnapshotStore>,
    states: Arc<StateRegistry>,
    metrics: Option<MetricsHandle>,
) -> Arc<MgmtState> {
    let state = match metrics {
        Some(handle) => {
            MgmtState::new(store, states).with_metrics(Arc::new(move || Some(handle.render())))
        }
        None => MgmtState::new(store, states),
    };
    Arc::new(state)
}

/// 运行时过滤器注册表：[`noop_filters`] 恒等占位为底座，`rgateway-filters`
/// 的真实工厂覆盖同名（RateLimit/Retry/CircuitBreaker/Timeout/BodyLimit）。
///
/// 覆盖而非整体替换：未实装的 canonical 名（JwtAuth、RewritePath 等）保持
/// 占位语义——配置可加载、槽位排链、运行期零拦截，实装落地后自然升级。
fn runtime_registry() -> FilterRegistry {
    let mut registry = noop_filters::noop_registry();
    let real = rgateway_filters::registry();
    for name in real.names() {
        if let Some(factory) = real.get(name) {
            registry.register(Arc::clone(factory));
        }
    }
    registry
}

/// 过滤表达式三级优先级（纯函数，便于表格驱动测试）：
/// `config.observability.tracing_filter` → 环境变量 `RUST_LOG`（由调用方
/// 传入）→ `"info"`。空白串视为未设置——空表达式没有任何指令，静默全量
/// 过滤掉日志比退回默认更糟。
pub fn resolve_tracing_filter(config_filter: Option<&str>, env_filter: Option<&str>) -> String {
    [config_filter, env_filter]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|expr| !expr.is_empty())
        .unwrap_or("info")
        .to_string()
}

/// 加载 + 校验（取第一个错误）：错误全部带 [`rgateway_config::Location`]，
/// 经 anyhow 链原样带给调用方（启动失败的消息就是运维的第一现场）。
fn load_validated(config_path: &Path) -> anyhow::Result<ConfigFile> {
    let file = load_from_file(config_path)?;
    if let Some(first) = validate(&file).into_iter().next() {
        return Err(first.into());
    }
    Ok(file)
}

/// 初始化 tracing：过滤表达式非法时退回 `"info"` 并告警；全局订阅器已
/// 安装（重复装配 / 测试并发）时 `try_init` 失败只记 debug，保持幂等。
fn init_tracing(expr: &str) {
    let filter = match tracing_subscriber::EnvFilter::try_new(expr) {
        Ok(filter) => filter,
        Err(err) => {
            tracing::warn!("invalid tracing filter {expr:?}: {err}; falling back to info");
            tracing_subscriber::EnvFilter::new("info")
        }
    };
    if let Err(err) = tracing_subscriber::fmt().with_env_filter(filter).try_init() {
        tracing::debug!("tracing subscriber already installed, init skipped: {err}");
    }
}

/// 编译快照 + 热重载上下文：过滤器用 [`runtime_registry`]（占位 + 真实
/// 工厂覆盖），handler 用 `rgateway-proxy` 的生产 [`HttpHandlerFactory`]。
/// 两者共享同一治理状态注册表，跨重载存活。
fn compile_snapshot(
    config_path: &Path,
    file: &ConfigFile,
) -> anyhow::Result<(Arc<SnapshotStore>, Arc<ReloadContext>)> {
    let connect_timeout = dsl::parse_duration(&file.server.timeouts.connect)
        .map_err(|msg| anyhow::anyhow!("invalid server.timeouts.connect: {msg}"))?;
    let handler_factory: Arc<dyn RouteHandlerFactory> =
        Arc::new(HttpHandlerFactory::new(connect_timeout));
    let states = Arc::new(StateRegistry::new());
    let registry = runtime_registry();
    let compiled = compile(CompileInput {
        file,
        registry: &registry,
        states: &states,
        handler_factory: handler_factory.as_ref(),
        config_dir: config_dir_of(config_path),
    })?;
    Ok((
        Arc::new(SnapshotStore::new(compiled)),
        Arc::new(ReloadContext {
            registry,
            states,
            handler_factory,
        }),
    ))
}

/// 相对路径（TLS 材料等）的解析基准 = 配置文件所在目录。
fn config_dir_of(config_path: &Path) -> &Path {
    config_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
#[path = "bootstrap_tests.rs"]
mod tests;
